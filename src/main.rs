mod calibration;
mod config;
mod display;
mod history;
mod net;
mod pins;
mod sensor;
mod storage;
mod web;

use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

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
    let peripherals_modem = peripherals.modem;

    let nvs_partition = EspDefaultNvsPartition::take().context("NVS partition")?;

    // Хранилище NVS: калибровка + история (снапшоты).
    let store = Arc::new(storage::NvsStore::new(nvs_partition.clone())?);
    let config_calibration = calibration::Calibration::try_from(&config.calibration)
        .context("секция [calibration] в config.toml")?;
    let calibration = Arc::new(Mutex::new(match store.load_calibration()? {
        Some(saved) => {
            log::info!("калибровка загружена из NVS");
            saved
        }
        None => config_calibration,
    }));

    // История: кольцо в RAM + снапшот из NVS при наличии.
    let history = Arc::new(Mutex::new(history::History::with_capacity(
        config.history.max_points,
    )));
    match store.load_history()? {
        Some(bytes) => match history::decode_snapshot(&bytes, config.history.max_points) {
            Ok(points) => {
                match history.lock() {
                    Ok(mut guard) => guard.restore(points),
                    Err(poisoned) => poisoned.into_inner().restore(points),
                }
                log::info!("история восстановлена из NVS");
            }
            Err(e) => log::warn!("история в NVS не читается ({e:#}) — начинаем заново"),
        },
        None => log::info!("снапшота истории нет — начинаем с пустого кольца"),
    }

    anyhow::ensure!(
        config.intervals.history_point_secs >= config.intervals.sensor_poll_secs,
        "intervals.history_point_secs ({}) должен быть не меньше периода опроса ({})",
        config.intervals.history_point_secs,
        config.intervals.sensor_poll_secs
    );

    // SNTP: сервер из NVS поверх config.toml (веб имеет приоритет).
    let sntp_state = Arc::new(Mutex::new(net::SntpState {
        server: match store.load_sntp_server()? {
            Some(saved) => {
                log::info!("SNTP-сервер загружен из NVS: {saved}");
                saved
            }
            None => config.sntp.server.clone(),
        },
        restart_pending: false,
    }));

    let net_status = Arc::new(Mutex::new(net::NetStatus::default()));
    let _wifi_task = net::start(
        peripherals_modem,
        nvs_partition,
        &secrets.wifi,
        Arc::clone(&net_status),
        Arc::clone(&sntp_state),
    )?;

    // По каналу на подписчика: экран, веб, логгер истории.
    let (tx_display, rx_display) = mpsc::channel::<sensor::Reading>();
    let (tx_web, rx_web) = mpsc::channel::<sensor::Reading>();
    let (tx_logging, rx_logging) = mpsc::channel::<sensor::Reading>();

    let _sensor_task = sensor::start(
        device_pins.dht11_data,
        Duration::from_secs(config.intervals.sensor_poll_secs),
        vec![tx_display, tx_web, tx_logging],
        Arc::clone(&calibration),
    )?;

    let _logging_task = history::start(
        rx_logging,
        Arc::clone(&history),
        Arc::clone(&store),
        history::LoggingParams {
            window_samples: (config.intervals.history_point_secs
                / config.intervals.sensor_poll_secs) as u32,
            snapshot_interval: Duration::from_secs(config.intervals.history_save_secs),
        },
    )?;

    let _display_task = display::start(
        peripherals_i2c0,
        device_pins.i2c_sda,
        device_pins.i2c_scl,
        &config.display,
        Duration::from_secs(config.intervals.display_refresh_secs),
        rx_display,
        Arc::clone(&net_status),
    )?;

    let latest_reading = Arc::new(Mutex::new(None));
    let _web_pump = web::start_pump(rx_web, Arc::clone(&latest_reading))?;
    let _http_server = web::start_server(web::WebState {
        latest: latest_reading,
        calibration,
        config_calibration,
        store,
        net: net_status,
        history,
        history_point_secs: config.intervals.history_point_secs,
        sntp: sntp_state,
        sntp_config_server: config.sntp.server.clone(),
    })?;
    log::info!("web: HTTP-сервер запущен на порту 80");

    // Supervisor: устройство без присмотра — смерть любого фонового потока
    // означает невосстановимую деградацию (драйверы периферии уничтожены
    // вместе с потоком и не пересоздаются без перезагрузки), поэтому —
    // лог + перезагрузка устройства.
    supervise(vec![
        ("sensor_task", _sensor_task),
        ("display_task", _display_task),
        ("logging_task", _logging_task),
        ("web_pump", _web_pump),
        ("wifi_keepalive", _wifi_task),
    ]);
}

/// Следит за фоновыми потоками: при завершении любого (паника или закрытие
/// канала) логирует имя и перезагружает устройство. Не возвращает управление.
fn supervise(tasks: Vec<(&'static str, std::thread::JoinHandle<()>)>) -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(5));
        for (name, handle) in &tasks {
            if handle.is_finished() {
                log::error!("supervisor: задача {name} завершилась — перезагрузка устройства");
                // Даём ESP_LOG успеть отправить сообщение перед сбросом.
                std::thread::sleep(Duration::from_millis(200));
                // SAFETY: esp_restart — штатная программная перезагрузка ESP-IDF;
                // требования к контексту (не ISR) соблюдены — вызов из обычного потока.
                unsafe { esp_idf_svc::sys::esp_restart() };
            }
        }
    }
}
