//! HTTP 小工具與儀表板用的唯讀 API。
//!
//! 攔截回應一律帶 `x-omniguard-rule` 與 `x-omniguard-guard-us`，
//! 讓畫面可以直接念這次檢查花了幾微秒，不必等輪詢。

use axum::body::Bytes;
use axum::extract::{FromRequest, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::state::{AppState, NewEvent};

pub async fn authenticate(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let supplied = request
        .headers()
        .get("x-omniguard-token")
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    let expected = state.config().gateway_token.as_bytes();
    let difference = supplied
        .iter()
        .zip(expected)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b));
    if supplied.len() != expected.len() || difference != 0 {
        return tagged_json(
            StatusCode::UNAUTHORIZED,
            "auth.required",
            0,
            json!({"error": {"type": "omniguard_auth", "message": "A valid x-omniguard-token header is required."}}),
        );
    }
    next.run(request).await
}

/// 不排隊累積大型本文；管理端點不使用這個名額。
pub async fn limit_requests(
    State(state): State<AppState>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let Ok(_permit) = state.capacity().try_acquire() else {
        let mut response = tagged_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "capacity.full",
            0,
            json!({"error":{"type":"omniguard_busy","rule":"capacity.full","message":"All 16 guard processing slots are busy. Nothing was forwarded; retry later."}}),
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, "1".parse().unwrap());
        return response;
    };
    next.run(request).await
}

pub async fn read_body(request: Request, state: &AppState) -> Result<Bytes, Response> {
    let (status, rule, message) = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Bytes::from_request(request, state),
    )
    .await
    {
        Ok(Ok(bytes)) => return Ok(bytes),
        Ok(Err(err)) => (err.status(), "body.read-error", err.body_text()),
        Err(_) => (
            StatusCode::REQUEST_TIMEOUT,
            "body.timeout",
            "Request body was not received within 10 seconds. Nothing was forwarded.".into(),
        ),
    };
    Err(tagged_json(
        status,
        rule,
        0,
        json!({"error":{"type":"omniguard_body","rule":rule,"message":message}}),
    ))
}

pub async fn health() -> Json<serde_json::Value> {
    Json(json!({"ok": true}))
}

pub async fn events(State(state): State<AppState>) -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Json(state.events()))
}

pub async fn stats(State(state): State<AppState>) -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Json(state.stats()))
}

pub async fn reset_circuit(State(state): State<AppState>) -> impl IntoResponse {
    state.reset_circuit();
    state.record(NewEvent {
        direction: "outbound",
        rule: "outbound.circuit-reset",
        snippet: "operator reset the breaker",
        decision: "info",
        guard_us: 0,
        status: 200,
    });
    ([(header::CACHE_CONTROL, "no-store")], Json(state.stats()))
}

pub fn tagged_json(
    status: StatusCode,
    rule: &str,
    guard_us: u64,
    body: serde_json::Value,
) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-omniguard-guard-us", guard_us.to_string())
        .header("x-omniguard-rule", rule)
        .body(axum::body::Body::from(body.to_string()))
        .expect("json response")
}

pub fn block(rule: &str, message: &str, guard_us: u64) -> Response {
    tagged_json(
        StatusCode::FORBIDDEN,
        rule,
        guard_us,
        json!({
            "error": {
                "type": "omniguard_block",
                "rule": rule,
                "message": message,
                "guard_us": guard_us
            }
        }),
    )
}
