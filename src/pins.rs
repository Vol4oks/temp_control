//! Выделение GPIO по рантайм-номерам из `config.toml`.
//!
//! esp-idf-hal использует типизированные пины (`Gpio4`, `Gpio6`, ...), поэтому
//! номера из конфига нельзя безопасно сопоставить с пинами при нескольких
//! устройствах: матч по одному `Pins`-значению для разных пинов даёт ошибку
//! «value moved». Поэтому номера превращаются в handle'ы `AnyIOPin` через
//! `steal` — это единственное место в проекте с `unsafe`.

use anyhow::{ensure, Context, Result};
use esp_idf_hal::gpio::AnyIOPin;

use crate::config::Pins as PinConfig;

/// Последний GPIO, определённый в esp-idf-hal для ESP32-C6 (Gpio0..Gpio30).
const MAX_GPIO: u8 = 30;

/// Пины устройства, сконфигурированные из `config.toml`.
pub struct DevicePins {
    pub dht11_data: AnyIOPin<'static>,
    pub i2c_sda: AnyIOPin<'static>,
    pub i2c_scl: AnyIOPin<'static>,
}

/// Валидирует `[pins]` и фабрикует handle'ы пинов.
///
/// # Errors
/// Возвращает ошибку, если пин вне диапазона `0..=MAX_GPIO` или два
/// устройства сидят на одном пине.
///
/// # Safety
/// Фабрикация `AnyIOPin::steal` безопасна при соблюдении контракта:
/// `Peripherals::take()` вызывается ровно один раз в `main` **до** этого
/// вызова, а структура `peripherals.pins` после этого не используется для
/// создания драйверов — каждый пин получает ровно один драйвер из handle'а
/// здесь. `steal` сам по себе не трогает железо (только создаёт handle);
/// реальная настройка пина происходит в драйверах. Уникальность пинов и
/// диапазон проверены выше.
pub fn configure(cfg: &PinConfig) -> Result<DevicePins> {
    let check = |num: u8, name: &str| -> Result<()> {
        ensure!(
            num <= MAX_GPIO,
            "pins.{name}: GPIO{num} вне диапазона 0..={MAX_GPIO}"
        );
        Ok(())
    };
    check(cfg.dht11_data, "dht11_data").with_context(|| "пин DHT11")?;
    check(cfg.i2c_sda, "i2c_sda").with_context(|| "пин SDA")?;
    check(cfg.i2c_scl, "i2c_scl").with_context(|| "пин SCL")?;

    ensure!(
        cfg.dht11_data != cfg.i2c_sda
            && cfg.dht11_data != cfg.i2c_scl
            && cfg.i2c_sda != cfg.i2c_scl,
        "пины в [pins] должны быть разными: dht11_data={}, i2c_sda={}, i2c_scl={}",
        cfg.dht11_data,
        cfg.i2c_sda,
        cfg.i2c_scl
    );

    // SAFETY: см. контракт в doc-комментарии функции; дубликаты и диапазон
    // проверены ensure! выше.
    Ok(DevicePins {
        dht11_data: unsafe { AnyIOPin::steal(cfg.dht11_data) },
        i2c_sda: unsafe { AnyIOPin::steal(cfg.i2c_sda) },
        i2c_scl: unsafe { AnyIOPin::steal(cfg.i2c_scl) },
    })
}
