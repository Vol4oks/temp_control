mod calibration;
mod config;
mod display;
mod pins;
mod sensor;

use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use esp_idf_hal::peripherals::Peripherals;

fn main() -> anyhow::Result<()> {
    // It is necessary to call this function once. Otherwise, some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    let (config, secrets) = config::load()?;

    log::info!(
        "пины: DHT11=GPIO{}, I2C SDA=GPIO{} SCL=GPIO{}; дисплей {}x{} @ {:#04X}",
        config.pins.dht11_data,
        config.pins.i2c_sda,
        config.pins.i2c_scl,
        config.display.width,
        config.display.height,
        config.display.i2c_address
    );
    log::info!(
        "интервалы (сек): сенсор {}, дисплей {}, история {}; буфер истории: {} точек",
        config.intervals.sensor_poll_secs,
        config.intervals.display_refresh_secs,
        config.intervals.history_save_secs,
        config.history.max_points
    );
    // Пароль не логируем — Debug-вывод Wifi маскирует его, здесь только SSID
    log::info!("Wi-Fi: ssid={:?}", secrets.wifi.ssid);

    // SAFETY-контракт pins::configure: Peripherals::take() вызывается ровно
    // один раз здесь; сами пины ниже используются только через pins::configure.
    let peripherals = Peripherals::take().context("периферия уже занята")?;
    let device_pins = pins::configure(&config.pins)?;
    let peripherals_i2c0 = peripherals.i2c0;

    let (tx, rx) = mpsc::channel::<sensor::Reading>();
    let calibration = Arc::new(Mutex::new(
        calibration::Calibration::try_from(&config.calibration)
            .context("секция [calibration] в config.toml")?,
    ));
    let _sensor_task = sensor::start(
        device_pins.dht11_data,
        Duration::from_secs(config.intervals.sensor_poll_secs),
        tx,
        Arc::clone(&calibration),
    )?;

    let _display_task = display::start(
        peripherals_i2c0,
        device_pins.i2c_sda,
        device_pins.i2c_scl,
        &config.display,
        Duration::from_secs(config.intervals.display_refresh_secs),
        rx,
    )?;

    // Потоки задач — не-демоны (как в std): ждем завершения сенсора, чтобы
    // отловить падение задачи. Дисплей держит поток неявно через main.
    _sensor_task
        .join()
        .map_err(|_| anyhow::anyhow!("поток sensor_task завершился паникой"))?;

    log::info!("сенсорная задача завершена");
    Ok(())
}
