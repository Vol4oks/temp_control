//! Конфигурация устройства: пины, дисплей, интервалы, история, Wi-Fi.
//!
//! `config.toml` и `secrets.toml` запекаются в прошивку при сборке через
//! `include_str!` и парсятся один раз при старте. Секреты (`secrets.toml`)
//! в git не попадают — см. `.gitignore` и `secrets.toml.example`.

use serde::Deserialize;

/// Корневой конфиг из `config.toml`.
#[derive(Debug, Deserialize)]
pub struct Config {
    pub pins: Pins,
    pub display: Display,
    pub intervals: Intervals,
    pub history: History,
    #[serde(default)]
    pub calibration: CalibrationConfig,
    #[serde(default)]
    pub sntp: SntpConfig,
}

/// Секция `[calibration]` — двухточечная калибровка. Опциональна:
/// без секции (или отдельных каналов) используется идентичность.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct CalibrationConfig {
    #[serde(default)]
    pub temperature: CalibrationChannel,
    #[serde(default)]
    pub humidity: CalibrationChannel,
}

impl Default for CalibrationConfig {
    fn default() -> Self {
        Self {
            temperature: CalibrationChannel::IDENTITY,
            humidity: CalibrationChannel::IDENTITY,
        }
    }
}

/// Опорные точки одного канала калибровки.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct CalibrationChannel {
    pub measured_1: f32,
    pub real_1: f32,
    pub measured_2: f32,
    pub real_2: f32,
}

impl CalibrationChannel {
    /// Идентичная калибровка (значения проходят без изменений).
    pub const IDENTITY: Self = Self {
        measured_1: 0.0,
        real_1: 0.0,
        measured_2: 1.0,
        real_2: 1.0,
    };
}

impl Default for CalibrationChannel {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Распиновка периферии.
#[derive(Debug, Deserialize)]
pub struct Pins {
    /// DHT11 — линия DATA.
    pub dht11_data: u8,
    /// SSD1306 — линия SDA.
    pub i2c_sda: u8,
    /// SSD1306 — линия SCL.
    pub i2c_scl: u8,
}

/// Параметры OLED-дисплея SSD1306.
#[derive(Debug, Deserialize)]
pub struct Display {
    /// I2C-адрес (обычно 0x3C).
    pub i2c_address: u8,
    pub width: u32,
    pub height: u32,
}

/// Периодичность задач (в секундах).
#[derive(Debug, Deserialize)]
pub struct Intervals {
    pub sensor_poll_secs: u64,
    pub display_refresh_secs: u64,
    /// Окно усреднения истории.
    #[serde(default = "default_history_point_secs")]
    pub history_point_secs: u64,
    /// Период снапшота истории в NVS.
    #[serde(default = "default_history_save_secs")]
    pub history_save_secs: u64,
}

fn default_history_point_secs() -> u64 {
    300
}

fn default_history_save_secs() -> u64 {
    3600
}

/// Параметры истории измерений.
#[derive(Debug, Deserialize)]
pub struct History {
    /// Максимум точек в NVS (см. docs/QUESTIONS.md ОВ-3).
    pub max_points: usize,
}

/// Секция `[sntp]` — сервер времени. Опциональна; меняется и через веб
/// (значение из браузера хранится в NVS и имеет приоритет).
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct SntpConfig {
    pub server: String,
}

impl Default for SntpConfig {
    fn default() -> Self {
        Self {
            server: "pool.ntp.org".to_owned(),
        }
    }
}

/// Секреты из `secrets.toml`.
#[derive(Debug, Deserialize)]
pub struct Secrets {
    pub wifi: Wifi,
}

/// Учётные данные Wi-Fi. Пароль маскируется в `Debug`, чтобы не уйти в лог.
#[derive(Deserialize)]
pub struct Wifi {
    pub ssid: String,
    // используется задачей Wi-Fi (Этап 4 из docs/PLAN.md)
    #[allow(dead_code)]
    pub password: String,
}

impl std::fmt::Debug for Wifi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wifi")
            .field("ssid", &self.ssid)
            .field("password", &"***")
            .finish()
    }
}

/// Загружает конфигурацию из запечённых TOML-файлов и проверяет значения.
///
/// # Errors
/// Возвращает ошибку, если файлы не парсятся или значения невалидны.
pub fn load() -> anyhow::Result<(Config, Secrets)> {
    let config: Config = toml::from_str(include_str!("../config.toml"))
        .map_err(|e| anyhow::anyhow!("config.toml: {e}"))?;
    let secrets: Secrets = toml::from_str(include_str!("../secrets.toml"))
        .map_err(|e| anyhow::anyhow!("secrets.toml: {e}"))?;

    anyhow::ensure!(
        config.history.max_points > 0,
        "history.max_points должен быть больше нуля"
    );
    anyhow::ensure!(
        config.intervals.sensor_poll_secs > 0
            && config.intervals.display_refresh_secs > 0
            && config.intervals.history_save_secs > 0
            && config.intervals.history_point_secs > 0,
        "все intervals.*_secs должны быть больше нуля"
    );
    anyhow::ensure!(
        !config.sntp.server.trim().is_empty(),
        "sntp.server не должен быть пустым"
    );

    Ok((config, secrets))
}
