//! Задача `logging_task`: усреднение показаний и история измерений.
//!
//! Схема (ОВ-3, решён): замеры приходят из канала сенсора каждые
//! `intervals.sensor_poll_secs`; задача копит среднее за окно
//! `intervals.history_point_secs` — по числу замеров (окно закрывается,
//! когда набрано `window_samples` значений; если датчик в окне молчал —
//! окно не закрывается и точка не пишется). Точки держатся в кольцевом
//! буфере в RAM (`history.max_points`).
//!
//! Раз в `intervals.history_save_secs` кольцо сохраняется в NVS бинарным
//! блобом (~3.5 КБ при 288 точках — в пределах одной страницы NVS, без
//! multi-page). Ресурс флеша при такой схеме — десятки лет (ОВ-3).
//! При потере питания история восстанавливается из снапшота; теряется
//! не более `history_save_secs` плюс незакрытое окно усреднения.
//!
//! Усредняются уже откалиброванные значения из канала: точка отражает
//! калибровку, действовавшую на момент замера.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::sensor::Reading;
use crate::storage::NvsStore;

/// Одно усреднённое значение истории.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Средняя температура за окно, °C.
    pub temperature: f32,
    /// Средняя влажность за окно, %.
    pub humidity: f32,
    /// Секунды с момента старта (конец окна усреднения).
    pub uptime_secs: u64,
}

/// Кольцевой буфер истории.
pub struct History {
    ring: VecDeque<Point>,
    cap: usize,
    dirty: bool,
}

impl History {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            ring: VecDeque::with_capacity(cap),
            cap,
            dirty: false,
        }
    }

    pub fn push(&mut self, point: Point) {
        if self.ring.len() == self.cap {
            self.ring.pop_front();
        }
        self.ring.push_back(point);
        self.dirty = true;
    }

    pub fn points(&self) -> Vec<Point> {
        self.ring.iter().copied().collect()
    }

    /// Был ли буфер менялся с последнего снапшота.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Восстанавливает точки из снапшота; лишние (сверх ёмкости) отбрасываются.
    pub fn restore(&mut self, points: impl IntoIterator<Item = Point>) {
        self.ring.clear();
        for point in points {
            if self.ring.len() == self.cap {
                self.ring.pop_front();
            }
            self.ring.push_back(point);
        }
    }
}

/// Размер записи точки в снапшоте: f32 + f32 + u32.
const RECORD_SIZE: usize = 12;

/// Метка формата снапшота (версия 1).
const SNAPSHOT_MAGIC: &[u8; 4] = b"THS1";

/// Кодирует кольцо в бинарный блоб: `THS1` + u32 count + записи по 12 байт.
pub fn encode_snapshot(points: &[Point]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + points.len() * RECORD_SIZE);
    out.extend_from_slice(SNAPSHOT_MAGIC);
    out.extend_from_slice(&(points.len() as u32).to_le_bytes());
    for p in points {
        out.extend_from_slice(&p.temperature.to_le_bytes());
        out.extend_from_slice(&p.humidity.to_le_bytes());
        out.extend_from_slice(&(p.uptime_secs as u32).to_le_bytes());
    }
    out
}

/// Разбирает блоб снапшота.
///
/// # Errors
/// Возвращает ошибку, если формат не совпадает, размер не сходится или
/// количество точек превышает ёмкость кольца.
pub fn decode_snapshot(bytes: &[u8], cap: usize) -> Result<Vec<Point>> {
    ensure!(bytes.len() >= 8, "снапшот истории слишком короткий");
    ensure!(
        &bytes[..4] == SNAPSHOT_MAGIC,
        "снапшот истории: неизвестный формат"
    );
    let count = u32::from_le_bytes(bytes[4..8].try_into().expect("8 байт выделено")) as usize;
    ensure!(
        count <= cap,
        "снапшот истории: {count} точек больше ёмкости {cap}"
    );
    ensure!(
        bytes.len() == 8 + count * RECORD_SIZE,
        "снапшот истории: размер {} не сходится с {count} точками",
        bytes.len()
    );

    let mut points = Vec::with_capacity(count);
    let mut offset = 8;
    for _ in 0..count {
        let temperature = f32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("4 байта выделено"),
        );
        let humidity = f32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .expect("4 байта выделено"),
        );
        let uptime_secs = u32::from_le_bytes(
            bytes[offset + 8..offset + 12]
                .try_into()
                .expect("4 байта выделено"),
        ) as u64;
        points.push(Point {
            temperature,
            humidity,
            uptime_secs,
        });
        offset += RECORD_SIZE;
    }
    Ok(points)
}

/// Параметры задачи логирования.
pub struct LoggingParams {
    /// Замеров в окне усреднения (`history_point_secs / sensor_poll_secs`).
    pub window_samples: u32,
    /// Период снапшота кольца в NVS.
    pub snapshot_interval: Duration,
}

/// Запускает `logging_task` в отдельном потоке.
///
/// # Errors
/// Возвращает ошибку, если поток не удалось создать.
pub fn start(
    rx: Receiver<Reading>,
    history: Arc<Mutex<History>>,
    store: Arc<NvsStore>,
    params: LoggingParams,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("logging_task".into())
        .stack_size(8192)
        .spawn(move || run(rx, history, store, params))
        .context("не удалось запустить поток logging_task")
}

fn run(
    rx: Receiver<Reading>,
    history: Arc<Mutex<History>>,
    store: Arc<NvsStore>,
    params: LoggingParams,
) {
    log::info!(
        "logging_task: запущен, окно {} замеров (~{} сек), снапшот каждые {:?}",
        params.window_samples,
        params.window_samples * 2,
        params.snapshot_interval
    );

    let mut sum_temperature = 0.0f32;
    let mut sum_humidity = 0.0f32;
    let mut samples = 0u32;
    let mut last_uptime;
    let mut last_snapshot = Instant::now();

    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(reading) => {
                sum_temperature += reading.temperature;
                sum_humidity += reading.humidity;
                last_uptime = reading.uptime_secs;
                samples += 1;

                if samples >= params.window_samples {
                    let point = Point {
                        temperature: sum_temperature / samples as f32,
                        humidity: sum_humidity / samples as f32,
                        uptime_secs: last_uptime,
                    };
                    match history.lock() {
                        Ok(mut guard) => guard.push(point),
                        Err(poisoned) => poisoned.into_inner().push(point),
                    }
                    log::info!(
                        "logging_task: точка истории T={:.1}°C H={:.1}% ({} замеров)",
                        point.temperature,
                        point.humidity,
                        samples
                    );
                    sum_temperature = 0.0;
                    sum_humidity = 0.0;
                    samples = 0;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                log::info!("logging_task: канал закрыт, поток завершается");
                break;
            }
        }

        if last_snapshot.elapsed() >= params.snapshot_interval {
            let snapshot = save_snapshot(&history, &store);
            if snapshot.is_ok() {
                last_snapshot = Instant::now();
            }
        }
    }

    // Финальный снапшот при штатном завершении — наилучшим образом.
    let _ = save_snapshot(&history, &store);
}

/// Сохраняет кольцо в NVS, если оно менялось со времени последнего снапшота.
fn save_snapshot(history: &Arc<Mutex<History>>, store: &Arc<NvsStore>) -> Result<()> {
    let bytes = {
        let mut guard = match history.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !guard.take_dirty() {
            return Ok(());
        }
        encode_snapshot(&guard.points())
    };
    store.save_history(&bytes)
}
