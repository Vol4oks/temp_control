//! Wi-Fi STA и общий сетевой статус (`NetStatus`).
//!
//! Подключение — по данным из `secrets.toml`; после успешного DHCP адрес
//! пишется в `NetStatus`, который читает `display_task`. Фоновый поток
//! (`wifi_keepalive`) держит Wi-Fi живым: проверяет связь и переподключается.
//!
//! Владелец драйвера — именно этот поток: потеря Wi-Fi — нормальное событие,
//! паник в цикле нет.

use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};

use crate::config::Wifi as WifiConfig;

/// Сетевой статус устройства, общий для задач (экран, веб).
#[derive(Debug, Default, Clone)]
pub struct NetStatus {
    /// IP-адрес устройства (None, пока Wi-Fi не подключён).
    pub ip: Option<String>,
}

/// Период проверки связи в keepalive-потоке.
const CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// Стек keepalive-потока: драйвер Wi-Fi + логирование.
const THREAD_STACK_SIZE: usize = 8192;

/// Подключается к Wi-Fi (блокирующе) и запускает keepalive-поток.
///
/// Возвращает ручку потока. Ошибка подключения не прерывает загрузку
/// устройства — keepalive продолжит попытки, прочие задачи работают без сети.
///
/// # Errors
/// Возвращает ошибку, только если поток не удалось создать.
pub fn start(
    modem: Modem<'static>,
    nvs: EspDefaultNvsPartition,
    wifi_cfg: &WifiConfig,
    net_status: Arc<Mutex<NetStatus>>,
) -> Result<JoinHandle<()>> {
    let (wifi, ip) = init_and_connect(modem, nvs, wifi_cfg).unwrap_or_else(|e| {
        log::warn!("wifi: подключение не удалось ({e:#}); keepalive продолжит попытки");
        (None, None)
    });

    set_ip(&net_status, ip);

    thread::Builder::new()
        .name("wifi_keepalive".into())
        .stack_size(THREAD_STACK_SIZE)
        .spawn(move || match wifi {
            Some(wifi) => keepalive_loop(wifi, net_status),
            // Драйвер не создался — но поток нужен: без него Modem/'static
            // объекты умрут, и повторной попытки не будет. Пока ограничимся
            // логом: перезапуск Wi-Fi без перезагрузки устройства в Этапе 6.
            None => {
                log::error!("wifi: драйвер недоступен, автоматическое восстановление отключено");
                loop {
                    thread::sleep(CHECK_INTERVAL);
                }
            }
        })
        .context("не удалось запустить поток wifi_keepalive")
}

/// Создаёт драйвер и подключается; `(Some(драйвер), Some(ip))` при успехе,
/// `(None, None)` при любой ошибке подключения.
fn init_and_connect(
    modem: Modem<'static>,
    nvs: EspDefaultNvsPartition,
    wifi_cfg: &WifiConfig,
) -> Result<(Option<BlockingWifi<EspWifi<'static>>>, Option<String>)> {
    let sys_loop =
        esp_idf_svc::eventloop::EspSystemEventLoop::take().context("системный event loop")?;

    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(modem, sys_loop.clone(), Some(nvs)).context("драйвер Wi-Fi")?,
        sys_loop,
    )
    .context("обёртка BlockingWifi")?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: wifi_cfg.ssid.as_str().try_into().context("wifi.ssid")?,
        auth_method: AuthMethod::WPA2Personal,
        password: wifi_cfg
            .password
            .as_str()
            .try_into()
            .context("wifi.password")?,
        ..Default::default()
    }))
    .context("конфигурация Wi-Fi")?;

    wifi.start().context("старт Wi-Fi")?;
    wifi.connect().context("подключение Wi-Fi")?;
    wifi.wait_netif_up().context("ожидание сети (netif up)")?;

    let ip = wifi.wifi().sta_netif().get_ip_info()?.ip.to_string();
    log::info!("wifi: подключено, IP {ip}");

    Ok((Some(wifi), Some(ip)))
}

/// Цикл поддержки связи: раз в `CHECK_INTERVAL` проверяет подключение и
/// переподключается при потере, обновляя `NetStatus`.
fn keepalive_loop(mut wifi: BlockingWifi<EspWifi<'static>>, net_status: Arc<Mutex<NetStatus>>) {
    let mut was_connected = true;

    loop {
        thread::sleep(CHECK_INTERVAL);

        let connected = match wifi.is_connected() {
            Ok(connected) => connected,
            Err(e) => {
                log::warn!("wifi: не удалось опросить состояние: {e}");
                false
            }
        };

        if connected {
            if !was_connected {
                log::info!("wifi: связь восстановлена");
                match wifi.wifi().sta_netif().get_ip_info() {
                    Ok(info) => set_ip(&net_status, Some(info.ip.to_string())),
                    Err(e) => log::warn!("wifi: не удалось получить IP: {e}"),
                }
            }
            was_connected = true;
            continue;
        }

        if was_connected {
            log::warn!("wifi: связь потеряна, переподключение...");
            set_ip(&net_status, None);
        }
        was_connected = false;

        // Попытка переподключения; ошибки не фатальны — повтор на следующем тике.
        if let Err(e) = wifi.connect() {
            log::warn!("wifi: переподключение не удалось: {e}");
            continue;
        }
        if let Err(e) = wifi.wait_netif_up() {
            log::warn!("wifi: сеть не поднялась: {e}");
            continue;
        }

        match wifi.wifi().sta_netif().get_ip_info() {
            Ok(info) => {
                let ip = info.ip.to_string();
                log::info!("wifi: переподключено, IP {ip}");
                set_ip(&net_status, Some(ip));
                was_connected = true;
            }
            Err(e) => log::warn!("wifi: не удалось получить IP: {e}"),
        }
    }
}

fn set_ip(net_status: &Arc<Mutex<NetStatus>>, ip: Option<String>) {
    // Отравление невозможно (под мьютексом нет паник), но обрабатываем явно.
    match net_status.lock() {
        Ok(mut status) => status.ip = ip,
        Err(poisoned) => poisoned.into_inner().ip = ip,
    }
}
