//! 外部內容檢查站。
//!
//! Agent 要打開未知 URL 或圖片前先打這裡。網域訊號永遠在本地算完；
//! 視覺訊號依設定可能是模型，也可能是標成 `palette` 的色塊啟發式。
//! 高風險（視覺像 Stripe、註冊域不是 stripe.com）回 403，其餘回 200 並帶 verdict。

use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;

use crate::guard::domain::{self, analyze_url};
use crate::guard::vision::{self, decode_image};
use crate::http;
use crate::state::{AppState, NewEvent};

#[derive(Debug, Deserialize)]
struct InspectRequest {
    url: String,
    #[serde(default)]
    image_base64: Option<String>,
}

pub async fn handle(State(state): State<AppState>, request: Request) -> Response {
    let started = Instant::now();
    let bytes = match http::read_body(request, &state).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let request: InspectRequest = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(err) => {
            return bad_request(
                elapsed_us(started),
                &format!("inspect body must be JSON with a url: {err}"),
            )
        }
    };
    let info = match analyze_url(&request.url) {
        Ok(info) => info,
        Err(message) => {
            let guard_us = elapsed_us(started);
            state.record(NewEvent {
                direction: "content",
                rule: "content.bad-url",
                snippet: "unreadable url",
                decision: "review",
                guard_us,
                status: 400,
            });
            return http::tagged_json(
                StatusCode::BAD_REQUEST,
                "content.bad-url",
                guard_us,
                json!({
                    "verdict": "review",
                    "rule": "content.bad-url",
                    "reason": message,
                    "guard_us": guard_us
                }),
            );
        }
    };

    let image_bytes = match request.image_base64.as_deref() {
        Some(raw) => match decode_image(raw) {
            Some(bytes) => {
                if let Err(err) = vision::validate_image(&bytes) {
                    return bad_request(elapsed_us(started), &err);
                }
                Some(bytes)
            }
            None => return bad_request(elapsed_us(started), "image_base64 is not valid base64"),
        },
        None => None,
    };
    let visual = vision::classify(state.http(), state.config(), image_bytes.as_deref()).await;
    let judgement = domain::judge(&info, visual.brand.as_deref());
    let guard_us = elapsed_us(started);
    let status = if judgement.verdict == "block" {
        403
    } else {
        200
    };

    state.record(NewEvent {
        direction: "content",
        rule: judgement.rule,
        snippet: &judgement.reason,
        decision: judgement.verdict,
        guard_us,
        status,
    });

    http::tagged_json(
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        judgement.rule,
        guard_us,
        json!({
            "url": request.url,
            "host": info.host,
            "registrable_domain": info.registrable_domain,
            "domain_matches_stripe": info.matches_official,
            "visual_brand": visual.brand,
            "visual_source": visual.source,
            "visual_error": visual.error,
            "verdict": judgement.verdict,
            "rule": judgement.rule,
            "reason": judgement.reason,
            "guard_us": guard_us
        }),
    )
}

fn bad_request(guard_us: u64, message: &str) -> Response {
    http::tagged_json(
        StatusCode::BAD_REQUEST,
        "content.bad-url",
        guard_us,
        json!({
            "verdict": "review",
            "rule": "content.bad-url",
            "reason": message,
            "guard_us": guard_us
        }),
    )
}

fn elapsed_us(started: Instant) -> u64 {
    started.elapsed().as_micros() as u64
}
