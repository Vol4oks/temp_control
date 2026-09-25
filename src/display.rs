//! Задача `display_task`: вывод показаний на OLED SSD1306 (I2C).
//!
//! Раз в `intervals.display_refresh_secs` забирает из канала самое свежее
//! измерение (дренаж, без накопления очереди) и перерисовывает экран.
//! I2C и SSD1306 инициализируются здесь; ошибка инициализации — критичная
//! (поток завершается с логом), ошибки отрисовки/записи по I2C — мягкие
//! (лог + повтор на следующем тике).
//!
//! IP-адрес до Этапа 4 (Wi-Fi) неизвестен — рисуем плейсхолдер.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_6X10};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::text::{Baseline, Text};
use esp_idf_hal::gpio::AnyIOPin;
use esp_idf_hal::i2c::{I2cConfig, I2cDriver, I2C0};
use esp_idf_hal::units::FromValueType;
use ssd1306::mode::{BufferedGraphicsMode, DisplayConfig as _};
use ssd1306::{prelude::*, I2CDisplayInterface, Ssd1306};

use crate::config::Display as DisplaySettings;
use crate::net::NetStatus;
use crate::sensor::Reading;

/// Тип дисплея. Размер — параметр компиляции (`DisplaySize128x64`);
/// конфиг на этот размер проверяется при старте.
type Display = Ssd1306<
    I2CInterface<I2cDriver<'static>>,
    DisplaySize128x64,
    BufferedGraphicsMode<DisplaySize128x64>,
>;

/// Разрешение, зашитое в `DisplaySize128x64`.
const WIDTH: u32 = 128;
const HEIGHT: u32 = 64;

/// Драйвер + буфер кадра (1 КБ) живут в потоке — стек с запасом.
const THREAD_STACK_SIZE: usize = 8192;

/// Запускает `display_task` в отдельном потоке, забирая себе приёмник канала.
///
/// # Errors
/// Возвращает ошибку, если конфиг дисплея не поддерживается или поток
/// не удалось создать.
pub fn start(
    i2c: I2C0<'static>,
    sda: AnyIOPin<'static>,
    scl: AnyIOPin<'static>,
    cfg: &DisplaySettings,
    refresh: Duration,
    rx: Receiver<Reading>,
    net_status: Arc<Mutex<NetStatus>>,
) -> Result<JoinHandle<()>> {
    ensure!(
        cfg.width == WIDTH && cfg.height == HEIGHT,
        "дисплей {}x{} не поддерживается: сейчас только {WIDTH}x{HEIGHT}",
        cfg.width,
        cfg.height
    );

    // cfg передаётся по ссылке, а потоку нужна 'static: копируем поля конфига.
    let cfg = DisplaySettings {
        i2c_address: cfg.i2c_address,
        width: cfg.width,
        height: cfg.height,
    };

    thread::Builder::new()
        .name("display_task".into())
        .stack_size(THREAD_STACK_SIZE)
        .spawn(move || run(i2c, sda, scl, cfg, refresh, rx, net_status))
        .context("не удалось запустить поток display_task")
}

fn run(
    i2c: I2C0<'static>,
    sda: AnyIOPin<'static>,
    scl: AnyIOPin<'static>,
    cfg: DisplaySettings,
    refresh: Duration,
    rx: Receiver<Reading>,
    net_status: Arc<Mutex<NetStatus>>,
) {
    let mut display = match init(i2c, sda, scl, &cfg) {
        Ok(display) => display,
        Err(e) => {
            log::error!("display_task: инициализация не удалась: {e:#}");
            return;
        }
    };

    // Baseline::Top — y в Point означает ВЕРХ текста. Дефолт Text —
    // Baseline::Alphabetic (y = базовая линия): глифы уезжают вверх на
    // baseline шрифта (15px у FONT_10X20), что и съело строку T.
    let big = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);

    log::info!("display_task: запущен, период обновления {refresh:?}");

    let mut latest: Option<Reading> = None;
    loop {
        // Дренаж: за тик может прийти несколько измерений — показываем последнее.
        while let Ok(reading) = rx.try_recv() {
            latest = Some(reading);
        }

        display.clear_buffer();
        draw_values(&mut display, latest.as_ref(), big);
        draw_net_line(&mut display, &net_status, small);
        if let Err(e) = display.flush() {
            log::warn!("display_task: не удалось вывести кадр: {e:?}");
        }

        thread::sleep(refresh);
    }
}

/// Инициализирует I2C-шину и SSD1306 в буферизованном графическом режиме.
///
/// # Errors
/// Возвращает ошибку, если шина или дисплей не инициализировались.
fn init(
    i2c: I2C0<'static>,
    sda: AnyIOPin<'static>,
    scl: AnyIOPin<'static>,
    cfg: &DisplaySettings,
) -> Result<Display> {
    // SSD1306 по спецификации тянет 400 кГц; подтяжки включаем встроенные.
    let i2c_config = I2cConfig::new().baudrate(400.kHz().into());
    let driver =
        I2cDriver::new(i2c, sda, scl, &i2c_config).context("инициализация I2C (SDA/SCL)")?;

    let interface = I2CDisplayInterface::new_custom_address(driver, cfg.i2c_address);
    let mut display = Ssd1306::new(interface, DisplaySize128x64, DisplayRotation::Rotate0)
        .into_buffered_graphics_mode();
    // DisplayError не реализует std::error::Error — мапим вручную.
    display
        .init()
        .map_err(|e| anyhow::anyhow!("инициализация SSD1306: {e:?}"))?;
    display.clear_buffer();
    Ok(display)
}

/// T и H крупно; `None` — плейсхолдеры, пока нет ни одного измерения.
/// Координаты y — верх строки при `Baseline::Top`.
fn draw_values(
    display: &mut Display,
    reading: Option<&Reading>,
    style: MonoTextStyle<BinaryColor>,
) {
    let (temp, hum) = match reading {
        Some(r) => (
            format!("T: {:>5.1} C", r.temperature),
            format!("H: {:>5.1} %", r.humidity),
        ),
        None => ("T:   -- C".to_owned(), "H:   -- %".to_owned()),
    };
    draw_text(display, &temp, Point::new(0, 0), style);
    draw_text(display, &hum, Point::new(0, 22), style);
}

/// Строка сети внизу экрана: IP после подключения Wi-Fi, плейсхолдер до него.
fn draw_net_line(
    display: &mut Display,
    net_status: &Arc<Mutex<NetStatus>>,
    style: MonoTextStyle<BinaryColor>,
) {
    // Мьютекс держим микросекунды — копируем IP и отпускаем.
    let ip = match net_status.lock() {
        Ok(status) => status.ip.clone(),
        Err(poisoned) => poisoned.into_inner().ip.clone(),
    };
    let line = match ip {
        Some(ip) => format!("IP: {ip}"),
        None => "IP: ---".to_owned(),
    };
    draw_text(display, &line, Point::new(0, 54), style);
}

fn draw_text(display: &mut Display, text: &str, pos: Point, style: MonoTextStyle<BinaryColor>) {
    let text = Text::with_baseline(text, pos, style, Baseline::Top);
    if let Err(e) = text.draw(display) {
        log::warn!("display_task: ошибка отрисовки текста: {e:?}");
    }
}
