//! Двухточечная линейная калибровка показаний DHT11.
//!
//! Пользователь задаёт две пары «датчик показал → реально есть» (отдельно
//! для температуры и влажности), показания пересчитываются по прямой через
//! эти точки: `real = k·measured + b`. Чистый сдвиг — частный случай
//! (наклон 1). Опорные точки нужно снимать в заметно разных условиях:
//! близкие `measured` дают нестабильный наклон.

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::CalibrationConfig;

/// Пара «показание датчика → реальное значение».
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Сырое значение датчика.
    pub measured: f32,
    /// Реальное значение по эталону.
    pub real: f32,
}

/// Две опорные точки калибровки.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TwoPoint {
    pub point1: Point,
    pub point2: Point,
}

impl TwoPoint {
    /// # Errors
    /// Возвращает ошибку, если значения не конечные или точки совпадают
    /// по оси измерений (`measured`) — через такие точки нельзя провести прямую.
    pub fn validate(&self) -> Result<()> {
        for (name, p) in [("point1", &self.point1), ("point2", &self.point2)] {
            ensure!(
                p.measured.is_finite() && p.real.is_finite(),
                "{name}: значения должны быть конечными"
            );
        }
        ensure!(
            self.point1.measured != self.point2.measured,
            "точки совпадают по оси измерений (measured): {} и {}",
            self.point1.measured,
            self.point2.measured
        );
        Ok(())
    }

    /// Пересчёт сырого значения по прямой через опорные точки.
    /// Точки должны быть провалидированы до попадания в структуру;
    /// при совпадающих X вернёт вход без изменений (защита от NaN).
    pub fn apply(&self, measured: f32) -> f32 {
        let dx = self.point2.measured - self.point1.measured;
        if dx == 0.0 {
            return measured;
        }
        let k = (self.point2.real - self.point1.real) / dx;
        k * measured + self.point1.real - k * self.point1.measured
    }
}

/// Активная калибровка датчика (температура + влажность).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    pub temperature: TwoPoint,
    pub humidity: TwoPoint,
}

impl Calibration {
    /// # Errors
    /// Возвращает ошибку, если хотя бы один канал не проходит валидацию.
    pub fn new(temperature: TwoPoint, humidity: TwoPoint) -> Result<Self> {
        temperature.validate().context("temperature")?;
        humidity.validate().context("humidity")?;
        Ok(Self {
            temperature,
            humidity,
        })
    }
}

impl TryFrom<&CalibrationConfig> for Calibration {
    type Error = anyhow::Error;

    fn try_from(cfg: &CalibrationConfig) -> Result<Self> {
        let to_points = |ch: &crate::config::CalibrationChannel| TwoPoint {
            point1: Point {
                measured: ch.measured_1,
                real: ch.real_1,
            },
            point2: Point {
                measured: ch.measured_2,
                real: ch.real_2,
            },
        };
        Self::new(to_points(&cfg.temperature), to_points(&cfg.humidity))
    }
}

// Юнит-тесты математики здесь не живут: у бина `harness = false` (нужно для
// esp-idf-линковки), и `#[test]`-функции не запускаются. Хост-тесты появятся
// после выноса `calibration`/`config` в lib-таргет (Этап 6 из docs/PLAN.md).
