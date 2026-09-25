//! Хранение калибровки в NVS (namespace `temp_control`).
//!
//! Калибровка — JSON-строка в одном ключе. NVS API ESP-IDF сам
//! потокобезопасен (nvs api locks), но тип `EspNvs` помечен только `Send`,
//! поэтому доступ сериализуется `Mutex`. Источник истины для runtime-правок:
//! успешная запись в NVS предшествует применению калибровки в `sensor_task`,
//! `config.toml` — базовые значения при первом старте и после сброса.

use std::sync::Mutex;

use anyhow::{Context, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition, EspNvs};

use crate::calibration::Calibration;

/// Имя ключа в NVS.
const KEY: &str = "calibration";

/// Размер буфера под JSON калибровки (с запасом: ~250 байт фактических).
const VALUE_BUF_SIZE: usize = 1024;

/// Хранилище калибровки.
pub struct CalibrationStore {
    nvs: Mutex<EspDefaultNvs>,
}

impl CalibrationStore {
    /// Открывает namespace в NVS в режиме чтения-записи.
    ///
    /// # Errors
    /// Возвращает ошибку, если NVS недоступен или namespace не открылся.
    pub fn new(partition: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspNvs::new(partition, "temp_control", true).context("открытие NVS")?;
        Ok(Self {
            nvs: Mutex::new(nvs),
        })
    }

    /// Загружает калибровку; `None` — ключа нет (первый запуск или сброс).
    ///
    /// # Errors
    /// Возвращает ошибку при сбое NVS или невалидном JSON (повреждённые
    /// данные) — вызывающий решает, что делать (fallback на config.toml).
    pub fn load(&self) -> Result<Option<Calibration>> {
        let mut buf = [0u8; VALUE_BUF_SIZE];
        let json = self.lock()?.get_str(KEY, &mut buf)?;

        match json {
            Some(json) => serde_json::from_str(json)
                .map(Some)
                .context("калибровка в NVS: некорректный JSON"),
            None => Ok(None),
        }
    }

    /// Есть ли сохранённая калибровка в NVS (для отображения источника).
    ///
    /// # Errors
    /// Возвращает ошибку при сбое NVS.
    pub fn has_key(&self) -> Result<bool> {
        let mut buf = [0u8; VALUE_BUF_SIZE];
        Ok(self.lock()?.get_str(KEY, &mut buf)?.is_some())
    }

    /// Сохраняет калибровку.
    ///
    /// # Errors
    /// Возвращает ошибку, если запись в NVS не удалась.
    pub fn save(&self, calibration: &Calibration) -> Result<()> {
        let json = serde_json::to_string(calibration).context("сериализация калибровки")?;
        self.lock()?.set_str(KEY, &json).context("запись NVS")
    }

    /// Удаляет сохранённую калибровку (возврат к config.toml).
    ///
    /// # Errors
    /// Возвращает ошибку, если стирание в NVS не удалось.
    pub fn clear(&self) -> Result<()> {
        self.lock()?.remove(KEY).context("стирание NVS")?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, EspDefaultNvs>> {
        // Отравление невозможно (под мьютексом нет паник), но обрабатываем явно.
        match self.nvs.lock() {
            Ok(guard) => Ok(guard),
            Err(poisoned) => Ok(poisoned.into_inner()),
        }
    }
}
