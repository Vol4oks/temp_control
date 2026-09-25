//! Хранилище NVS (namespace `temp_control`): калибровка и история.
//!
//! Калибровка — JSON-строка, история — бинарный блоб (см. `src/history.rs`).
//! NVS API ESP-IDF сам потокобезопасен (nvs api locks), но тип `EspNvs`
//! помечен только `Send`, поэтому доступ сериализуется `Mutex`.
//!
//! Порядок применения для калибровки: успешная запись в NVS предшествует
//! применению в `sensor_task` — состояние не разъедется с хранилищем.

use std::sync::Mutex;

use anyhow::{Context, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition, EspNvs};

use crate::calibration::Calibration;

/// Ключ калибровки.
const KEY_CALIBRATION: &str = "calibration";

/// Ключ истории (бинарный снапшот, формат — `history::encode_snapshot`).
const KEY_HISTORY: &str = "history";

/// Ключ NTP-сервера (строка, меняется через веб).
const KEY_SNTP: &str = "sntp_server";

/// Размер буфера под JSON калибровки (с запасом: ~250 байт фактических).
const VALUE_BUF_SIZE: usize = 1024;

/// Хранилище NVS.
pub struct NvsStore {
    nvs: Mutex<EspDefaultNvs>,
}

impl NvsStore {
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

    // --- калибровка ---

    /// Загружает калибровку; `None` — ключа нет (первый запуск или сброс).
    ///
    /// # Errors
    /// Возвращает ошибку при сбое NVS или невалидном JSON (повреждённые
    /// данные) — вызывающий решает, что делать (fallback на config.toml).
    pub fn load_calibration(&self) -> Result<Option<Calibration>> {
        let mut buf = [0u8; VALUE_BUF_SIZE];
        let json = self.lock()?.get_str(KEY_CALIBRATION, &mut buf)?;

        match json {
            Some(json) => serde_json::from_str(json)
                .map(Some)
                .context("калибровка в NVS: некорректный JSON"),
            None => Ok(None),
        }
    }

    /// Сохраняет калибровку.
    ///
    /// # Errors
    /// Возвращает ошибку, если запись в NVS не удалась.
    pub fn save_calibration(&self, calibration: &Calibration) -> Result<()> {
        let json = serde_json::to_string(calibration).context("сериализация калибровки")?;
        self.lock()?
            .set_str(KEY_CALIBRATION, &json)
            .context("запись NVS")
    }

    /// Удаляет сохранённую калибровку (возврат к config.toml).
    ///
    /// # Errors
    /// Возвращает ошибку, если стирание в NVS не удалось.
    pub fn clear_calibration(&self) -> Result<()> {
        self.lock()?
            .remove(KEY_CALIBRATION)
            .context("стирание NVS")?;
        Ok(())
    }

    /// Есть ли сохранённая калибровка в NVS (для отображения источника).
    ///
    /// # Errors
    /// Возвращает ошибку при сбое NVS.
    pub fn has_calibration_key(&self) -> Result<bool> {
        let mut buf = [0u8; VALUE_BUF_SIZE];
        Ok(self.lock()?.get_str(KEY_CALIBRATION, &mut buf)?.is_some())
    }

    // --- история ---

    /// Сохраняет бинарный снапшот истории.
    ///
    /// # Errors
    /// Возвращает ошибку, если запись в NVS не удалась.
    pub fn save_history(&self, bytes: &[u8]) -> Result<()> {
        self.lock()?
            .set_blob(KEY_HISTORY, bytes)
            .context("запись истории в NVS")
    }

    /// Загружает бинарный снапшот истории; `None` — снапшота нет.
    ///
    /// # Errors
    /// Возвращает ошибку при сбое NVS.
    pub fn load_history(&self) -> Result<Option<Vec<u8>>> {
        let nvs = self.lock()?;

        let Some(len) = nvs.blob_len(KEY_HISTORY)? else {
            return Ok(None);
        };
        let mut buf = vec![0u8; len];
        let data = nvs
            .get_blob(KEY_HISTORY, &mut buf)?
            .context("история: ключ исчез между чтениями")?;
        Ok(Some(data.to_vec()))
    }

    // --- SNTP-сервер ---

    /// Загружает NTP-сервер; `None` — ключа нет (fallback на config.toml).
    ///
    /// # Errors
    /// Возвращает ошибку при сбое NVS.
    pub fn load_sntp_server(&self) -> Result<Option<String>> {
        let mut buf = [0u8; 128];
        match self.lock()?.get_str(KEY_SNTP, &mut buf)? {
            Some(s) => Ok(Some(s.to_owned())),
            None => Ok(None),
        }
    }

    /// Сохраняет NTP-сервер.
    ///
    /// # Errors
    /// Возвращает ошибку, если запись в NVS не удалась.
    pub fn save_sntp_server(&self, server: &str) -> Result<()> {
        self.lock()?.set_str(KEY_SNTP, server).context("запись NVS")
    }

    /// Удаляет NTP-сервер из NVS (возврат к config.toml).
    ///
    /// # Errors
    /// Возвращает ошибку, если стирание в NVS не удалось.
    pub fn clear_sntp_server(&self) -> Result<()> {
        self.lock()?.remove(KEY_SNTP).context("стирание NVS")?;
        Ok(())
    }

    /// Есть ли сохранённый NTP-сервер в NVS (для отображения источника).
    ///
    /// # Errors
    /// Возвращает ошибку при сбое NVS.
    pub fn has_sntp_key(&self) -> Result<bool> {
        let mut buf = [0u8; 128];
        Ok(self.lock()?.get_str(KEY_SNTP, &mut buf)?.is_some())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, EspDefaultNvs>> {
        // Отравление невозможно (под мьютексом нет паник), но обрабатываем явно.
        match self.nvs.lock() {
            Ok(guard) => Ok(guard),
            Err(poisoned) => Ok(poisoned.into_inner()),
        }
    }
}
