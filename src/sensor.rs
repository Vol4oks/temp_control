//! Задача `sensor_task`: опрос DHT11 и отправка измерений в канал.
//!
//! Периодичность — `intervals.sensor_poll_secs` из `config.toml` (по
//! архитектуре AGENTS.md — 2 сек; DHT11 физически не опрашивается чаще
//! ~1 Гц). Ошибки чтения логируются, канал при этом не засоряется —
//! подписчики видят только валидные измерения (поведение при потере
//! сенсора уточняется в docs/QUESTIONS.md ОВ-5).

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use embedded_dht_rs::dht11::Dht11;
use esp_idf_hal::delay::Delay;
use esp_idf_hal::gpio::{AnyIOPin, PinDriver, Pull};
use serde::Serialize;

use crate::calibration::Calibration;

/// Одно измерение датчика.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Reading {
    /// Температура после калибровки, °C.
    pub temperature: f32,
    /// Влажность после калибровки, %.
    pub humidity: f32,
    /// Сырая температура с датчика (без калибровки), °C — нужна при
    /// снятии опорных точек.
    pub raw_temperature: f32,
    /// Сырая влажность с датчика, %.
    pub raw_humidity: f32,
    /// Секунды с момента старта задачи (свежесть данных).
    pub uptime_secs: u64,
}

/// DHT-протокол — битовый bang-bang с микросекундными паузами; стек с запасом
/// под драйвер и форматирование логов.
const THREAD_STACK_SIZE: usize = 8192;

/// Запускает `sensor_task` в отдельном потоке.
///
/// `senders` — по каналу на каждого подписчика (экран, веб); рассылка
/// «последнему ценному» подписчику не блокирует остальных, каналы закрытых
/// подписчиков выпадают из рассылки.
/// `calibration` — общее состояние: `sensor_task` читает его раз в цикл,
/// веб-задача обновляет из браузера.
///
/// # Errors
/// Возвращает ошибку, если поток не удалось создать.
pub fn start(
    pin: AnyIOPin<'static>,
    poll: Duration,
    senders: Vec<Sender<Reading>>,
    calibration: Arc<Mutex<Calibration>>,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("sensor_task".into())
        .stack_size(THREAD_STACK_SIZE)
        .spawn(move || run(pin, poll, senders, calibration))
        .context("не удалось запустить поток sensor_task")
}

fn run(
    pin: AnyIOPin<'static>,
    poll: Duration,
    tx: Vec<Sender<Reading>>,
    calibration: Arc<Mutex<Calibration>>,
) {
    let started = Instant::now();

    // DHT11 — однопроводная шина: MCU и сенсор поочерёдно управляют линией,
    // поэтому открытый сток с подтяжкой вверх.
    let pin_driver = match PinDriver::input_output_od(pin, Pull::Up) {
        Ok(driver) => driver,
        Err(e) => {
            log::error!("sensor_task: не удалось настроить GPIO для DHT11: {e}");
            return;
        }
    };
    let mut dht = Dht11::new(pin_driver, Delay::default());

    let mut senders = tx;
    log::info!(
        "sensor_task: запущен, период опроса {poll:?}, подписчиков {}",
        senders.len()
    );

    loop {
        // Пауза и до первого чтения: DHT11 нужен ~1 сек после подачи питания.
        thread::sleep(poll);

        match dht.read() {
            Ok(r) => {
                // Мьютекс держим микросекунды — копируем калибровку и отпускаем.
                // Отравление в текущем коде невозможно (под мьютексом нет паник),
                // но обрабатываем явно — данные валидны в любом случае.
                let cal = match calibration.lock() {
                    Ok(guard) => *guard,
                    Err(poisoned) => *poisoned.into_inner(),
                };

                let raw_temperature = f32::from(r.temperature);
                let raw_humidity = f32::from(r.humidity);
                let reading = Reading {
                    temperature: cal.temperature.apply(raw_temperature),
                    humidity: cal.humidity.apply(raw_humidity),
                    raw_temperature,
                    raw_humidity,
                    uptime_secs: started.elapsed().as_secs(),
                };
                log::info!(
                    "sensor_task: T={:.1}°C (raw {:.1}) H={:.1}% (raw {:.1}) uptime {}с",
                    reading.temperature,
                    reading.raw_temperature,
                    reading.humidity,
                    reading.raw_humidity,
                    reading.uptime_secs
                );
                // Каналы закрытых подписчиков выпадают из рассылки;
                // сенсор умирает, только когда подписчиков не осталось.
                senders.retain(|tx| tx.send(reading).is_ok());
                if senders.is_empty() {
                    log::info!("sensor_task: получателей нет, поток завершается");
                    return;
                }
            }
            Err(e) => {
                // Одиночные сбои (таймаут/CRC) нормальны для бит-bang под RTOS;
                // повторная попытка — на следующем периоде.
                log::warn!("sensor_task: ошибка чтения DHT11: {e:?}");
            }
        }
    }
}
