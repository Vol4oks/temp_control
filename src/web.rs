//! Задача `web_server_task`: HTTP-сервер и JSON API.
//!
//! Эндпоинты:
//! - `GET /` — веб-интерфейс (показания + сетка калибровки)
//! - `GET /api/temperature` — последнее измерение (raw + calibrated)
//! - `GET /api/calibration` — активная калибровка и её источник
//! - `POST /api/calibration` — задать точки (валидация → NVS → применение)
//! - `POST /api/calibration/reset` — сброс к значениям из config.toml
//!
//! `EspHttpServer` живёт в вызывающем контексте (`main`) — падать нельзя;
//! хендлеры вызываются во внутренних потоках ESP-IDF и работают только с
//! общими `Arc<Mutex<..>>`-состояниями.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use embedded_svc::http::{Headers, Method};
use esp_idf_svc::http::server::{Configuration, EspHttpServer, Request};
use esp_idf_svc::io::{Read, Write};
use serde_json::json;

use crate::calibration::Calibration;
use crate::net::NetStatus;
use crate::sensor::Reading;
use crate::storage::CalibrationStore;

/// Ограничение тела POST-запросов (JSON калибровки ~250 байт).
const MAX_BODY_LEN: usize = 2048;

/// Общие состояния, доступные HTTP-хендлерам.
pub struct WebState {
    /// Последнее измерение (заполняет поток-насос из канала сенсора).
    pub latest: Arc<Mutex<Option<Reading>>>,
    /// Активная калибровка (веб её меняет, сенсор читает).
    pub calibration: Arc<Mutex<Calibration>>,
    /// Калибровка из config.toml — цель для `POST /api/calibration/reset`.
    pub config_calibration: Calibration,
    /// Хранилище калибровки в NVS.
    pub store: Arc<CalibrationStore>,
    /// Сетевой статус (IP) — виден на странице.
    pub net: Arc<Mutex<NetStatus>>,
}

/// Запускает поток-насос: канал сенсора → `latest` для HTTP-хендлеров.
///
/// # Errors
/// Возвращает ошибку, если поток не удалось создать.
pub fn start_pump(
    rx: Receiver<Reading>,
    latest: Arc<Mutex<Option<Reading>>>,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("web_pump".into())
        .stack_size(4096)
        .spawn(move || {
            for reading in rx {
                match latest.lock() {
                    Ok(mut guard) => *guard = Some(reading),
                    Err(poisoned) => *poisoned.into_inner() = Some(reading),
                }
            }
            log::info!("web_pump: канал закрыт, поток завершается");
        })
        .context("не удалось запустить поток web_pump")
}

/// Поднимает HTTP-сервер на порту 80 и регистрирует хендлеры.
///
/// Возвращённый сервер должен жить всё время работы устройства.
///
/// # Errors
/// Возвращает ошибку, если сервер не удалось создать или зарегистрировать
/// хендлеры.
pub fn start_server(state: WebState) -> Result<EspHttpServer<'static>> {
    let config = Configuration {
        stack_size: 10240,
        ..Default::default()
    };
    let mut server = EspHttpServer::new(&config).context("создание HTTP-сервера")?;

    let index_page = include_str!("../web/index.html");

    server.fn_handler::<anyhow::Error, _>("/", Method::Get, move |req| {
        respond(req, 200, "text/html", index_page.as_bytes())
    })?;

    let state_temperature = state.latest.clone();
    let state_net = state.net.clone();
    server.fn_handler::<anyhow::Error, _>("/api/temperature", Method::Get, move |req| {
        let latest = clone_latest(&state_temperature);
        let ip = clone_ip(&state_net);
        let body = json!({
            "temperature": latest.as_ref().map(|r| r.temperature),
            "humidity": latest.as_ref().map(|r| r.humidity),
            "raw_temperature": latest.as_ref().map(|r| r.raw_temperature),
            "raw_humidity": latest.as_ref().map(|r| r.raw_humidity),
            "uptime_secs": latest.as_ref().map(|r| r.uptime_secs),
            "ip": ip,
        });
        respond_json(req, 200, &body)
    })?;

    let cal_state = state.calibration.clone();
    let cal_store = state.store.clone();
    server.fn_handler::<anyhow::Error, _>("/api/calibration", Method::Get, move |req| {
        let calibration = clone_calibration(&cal_state);
        let source = if cal_store.has_key()? {
            "nvs"
        } else {
            "config"
        };
        respond_json(
            req,
            200,
            &json!({
                "source": source,
                "temperature": calibration.temperature,
                "humidity": calibration.humidity,
            }),
        )
    })?;

    let cal_state = state.calibration.clone();
    let cal_store = state.store.clone();
    server.fn_handler::<anyhow::Error, _>("/api/calibration", Method::Post, move |mut req| {
        let body = read_body(&mut req)?;
        let proposed: Calibration =
            serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("некорректный JSON: {e}"))?;
        proposed.temperature.validate().context("temperature")?;
        proposed.humidity.validate().context("humidity")?;

        // Сначала сохраняем в NVS, затем применяем: калибровка применяется
        // только если запись прошла, иначе состояние разъедется с хранилищем.
        cal_store.save(&proposed)?;
        match cal_state.lock() {
            Ok(mut guard) => *guard = proposed,
            Err(poisoned) => *poisoned.into_inner() = proposed,
        }
        log::info!("web: калибровка обновлена через браузер");
        respond_json(req, 200, &json!({ "ok": true }))
    })?;

    let cal_state = state.calibration.clone();
    let cal_store = state.store.clone();
    let config_calibration = state.config_calibration;
    server.fn_handler::<anyhow::Error, _>("/api/calibration/reset", Method::Post, move |req| {
        cal_store.clear()?;
        match cal_state.lock() {
            Ok(mut guard) => *guard = config_calibration,
            Err(poisoned) => *poisoned.into_inner() = config_calibration,
        }
        log::info!("web: калибровка сброшена к config.toml");
        respond_json(req, 200, &json!({ "ok": true }))
    })?;

    Ok(server)
}

fn clone_latest(state: &Arc<Mutex<Option<Reading>>>) -> Option<Reading> {
    match state.lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

fn clone_ip(state: &Arc<Mutex<NetStatus>>) -> Option<String> {
    match state.lock() {
        Ok(guard) => guard.ip.clone(),
        Err(poisoned) => poisoned.into_inner().ip.clone(),
    }
}

fn clone_calibration(state: &Arc<Mutex<Calibration>>) -> Calibration {
    match state.lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

/// Читает тело запроса с ограничением размера.
///
/// # Errors
/// Возвращает ошибку при превышении лимита или сбое чтения.
fn read_body(
    req: &mut Request<&mut esp_idf_svc::http::server::EspHttpConnection>,
) -> Result<String> {
    let len = req.content_len().unwrap_or(0) as usize;
    if len > MAX_BODY_LEN {
        anyhow::bail!("тело запроса {len} байт превышает лимит {MAX_BODY_LEN}");
    }
    let mut buf = vec![0u8; len];
    req.read_exact(&mut buf)?;
    Ok(String::from_utf8(buf)?)
}

/// Отправляет ответ с заданным Content-Type.
fn respond(
    req: Request<&mut esp_idf_svc::http::server::EspHttpConnection>,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let headers = [("Content-Type", content_type)];
    let mut response = req.into_response(status, None, &headers)?;
    response.write_all(body)?;
    Ok(())
}

/// Отправляет JSON-ответ.
fn respond_json(
    req: Request<&mut esp_idf_svc::http::server::EspHttpConnection>,
    status: u16,
    body: &serde_json::Value,
) -> Result<()> {
    respond(req, status, "application/json", body.to_string().as_bytes())
}
