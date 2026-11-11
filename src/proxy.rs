//! 反向代理。
//!
//! 客戶端設定 SDK 的 `base_url` 與閘道驗證標頭。這裡先收完 body（有上限），跑本地與 AI 檢查，
//! 通過才轉發。上游回應先完整緩衝與秘密掃描，再保持原格式回寫。
//!
//! 沒有 `UPSTREAM_API_KEY` 時，chat／messages 回本地完成。`/v1/responses` 不假造成功，回 503。
//! 本地攔截不會把 body 送出去；AI 檢查會送往設定的分類模型，攔截後不轉發到應用上游。

use std::time::Instant;

use axum::body::Body;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;

use crate::guard::{ai, inbound, outbound, response, urls};
use crate::http::{self, block};
use crate::state::{AppState, NewEvent};

const STRIP_REQUEST: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "trailers",
    "transfer-encoding",
    "upgrade",
    // 讓上游回未壓縮 JSON。我們沒開 reqwest 的自動解壓，不能代客協商 gzip。
    "accept-encoding",
    // 閘道是實際的 API 呼叫端，不沿用可能被供應商拒絕的 SDK 識別。
    "user-agent",
];

const STRIP_RESPONSE: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "trailers",
    "transfer-encoding",
    "upgrade",
    // 串流回寫時長度由 axum 重算，沿用上游的 content-length 會對不齊。
    "content-length",
];

pub async fn handle(State(state): State<AppState>, request: Request) -> Response {
    let path = request.uri().path().to_string();
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|part| part.as_str().to_string())
        .unwrap_or_else(|| path.clone());
    let method = request.method().clone();
    let headers = request.headers().clone();
    if headers
        .get("content-encoding")
        .is_some_and(|v| v != "identity")
    {
        return http::tagged_json(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "body.encoding",
            0,
            serde_json::json!({"error": {"message": "Compressed request bodies are not supported. Nothing was forwarded."}}),
        );
    }

    let bytes = match http::read_body(request, &state).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let started = Instant::now();
    let value: serde_json::Value = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value) if value.is_object() => value,
        result => {
            return http::tagged_json(
                StatusCode::BAD_REQUEST,
                "body.invalid-json",
                elapsed_us(started),
                serde_json::json!({"error": {"message": match result {
                    Err(err) => err.to_string(),
                    _ => "Request body must be a JSON object.".into(),
                }}}),
            )
        }
    };
    // 同時掃原文與解析後的字串，保留重複欄位中的命中並消除 JSON 跳脫。
    let text = value.to_string();
    let raw = String::from_utf8_lossy(&bytes);
    let inbound_hit = inbound::scan(&raw).or_else(|| inbound::scan(&text));
    let outbound_hit = outbound::scan(&raw).or_else(|| outbound::scan(&text));
    let outbound_hit = if outbound_hit.is_some() {
        outbound_hit
    } else {
        match outbound::scan_json(&value) {
            Ok(hit) => hit,
            Err(message) => {
                return http::tagged_json(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "body.encoding-depth",
                    elapsed_us(started),
                    serde_json::json!({"error":{"type":"omniguard_body","rule":"body.encoding-depth","message":message}}),
                )
            }
        }
    };
    if outbound_hit.is_some() {
        state.trip_circuit();
    }
    let guard_us = elapsed_us(started);

    // 入口比出口先報。斷路器開著時，越獄句仍回自己的規則，示範才分得清兩道鎖。
    if let Some(hit) = inbound_hit {
        tracing::info!(rule = hit.rule, guard_us, "blocked");
        state.record(NewEvent {
            direction: "inbound",
            rule: hit.rule,
            snippet: &hit.snippet,
            decision: "block",
            guard_us,
            status: 403,
        });
        return block(
            hit.rule,
            "Blocked by the inbound prompt guard. This is a known-phrase filter, not proof that every injection was found. The request was not forwarded.",
            guard_us,
        );
    }

    if let Some(hit) = outbound_hit {
        // 先開路再開 403。下一筆乾淨轉發會在視窗內失敗關閉。
        tracing::info!(rule = hit.rule, guard_us, "blocked");
        state.record(NewEvent {
            direction: "outbound",
            rule: hit.rule,
            snippet: &hit.snippet,
            decision: "block",
            guard_us,
            status: 403,
        });
        return block(
            hit.rule,
            "Blocked: a plaintext Stripe secret was in the request body. Nothing was forwarded. The outbound circuit is open for 30 seconds.",
            guard_us,
        );
    }

    if state.circuit_open() {
        let guard_us = elapsed_us(started);
        tracing::info!(rule = "outbound.circuit-open", guard_us, "blocked");
        state.record(NewEvent {
            direction: "outbound",
            rule: "outbound.circuit-open",
            snippet: "circuit open for 30s after a plaintext secret",
            decision: "block",
            guard_us,
            status: 403,
        });
        return block(
            "outbound.circuit-open",
            "Blocked: the outbound circuit is open after a plaintext secret leak. Reset it on the local dashboard, or wait 30 seconds.",
            guard_us,
        );
    }

    if let Some(hit) = urls::scan(&value) {
        let guard_us = elapsed_us(started);
        state.record(NewEvent {
            direction: "content",
            rule: hit.rule,
            snippet: &hit.snippet,
            decision: "block",
            guard_us,
            status: 403,
        });
        return block(hit.rule, "Blocked a high-confidence deceptive Stripe URL or insecure HTTP Stripe URL before AI or application upstream calls.", guard_us);
    }

    let analysis = if state.config().gmi_key.is_some() {
        match ai::analyze(state.http(), state.config(), &value).await {
            Ok(analysis) => {
                let (decision, status, rule) = match analysis.verdict {
                    ai::Verdict::Allow => ("info", StatusCode::OK, "inbound.ai-allow"),
                    ai::Verdict::Block => ("block", StatusCode::FORBIDDEN, "inbound.ai-block"),
                    ai::Verdict::Review => ("review", StatusCode::CONFLICT, "inbound.ai-review"),
                };
                let guard_us = elapsed_us(started);
                state.record(NewEvent {
                    direction: "inbound",
                    rule,
                    snippet: &format!("semantic classification: {:?}", analysis.category),
                    decision,
                    guard_us,
                    status: status.as_u16(),
                });
                if analysis.verdict != ai::Verdict::Allow {
                    return http::tagged_json(
                        status,
                        rule,
                        guard_us,
                        serde_json::json!({
                            "verdict": analysis.verdict, "source": "ai", "model": state.config().semantic_model,
                            "analysis": analysis,
                            "error": {"type": "omniguard_block", "rule": rule,
                                "message": "Semantic guard stopped this request; nothing was sent to the application upstream."},
                            "guard_us": guard_us,
                        }),
                    );
                }
                Some(analysis)
            }
            Err((status, message)) => {
                let guard_us = elapsed_us(started);
                state.record(NewEvent {
                    direction: "inbound",
                    rule: "inbound.ai-error",
                    snippet: "semantic analysis failed; forwarding stopped",
                    decision: "review",
                    guard_us,
                    status: status.as_u16(),
                });
                return http::tagged_json(
                    status,
                    "inbound.ai-error",
                    guard_us,
                    serde_json::json!({
                        "verdict": "review", "source": "ai", "model": state.config().semantic_model,
                        "error": {"type": "omniguard_ai", "rule": "inbound.ai-error", "message": message},
                        "guard_us": guard_us,
                    }),
                );
            }
        }
    } else {
        None
    };
    let guard_us = elapsed_us(started);
    if path == "/v1/guard/analyze" {
        return http::tagged_json(
            StatusCode::OK,
            "inbound.analysis",
            guard_us,
            serde_json::json!({
                "verdict": if analysis.is_some() { "allow" } else { "review" },
                "source": if analysis.is_some() { "ai" } else { "rules" },
                "semantic_enabled": analysis.is_some(),
                "model": state.config().gmi_key.as_ref().map(|_| &state.config().semantic_model),
                "analysis": analysis,
                "reason": if state.config().gmi_key.is_some() { "AI analysis completed; this endpoint never forwards to application upstream." }
                    else { "AI is disabled: set GMI_API_KEY to enable semantic analysis. Only local rules ran; no AI safety conclusion is available." },
                "guard_us": guard_us,
            }),
        );
    }
    let snippet = "request passed configured guards";

    if state.config().upstream_key.is_none() {
        // Responses 沒有可對照的本地完成格式。假造 output 會讓呼叫端以為模型真的跑過。
        if path == "/v1/responses" {
            state.record(NewEvent {
                direction: "forward",
                rule: "forward.responses-unavailable",
                snippet: "responses api requires an application upstream key",
                decision: "review",
                guard_us,
                status: 503,
            });
            return http::tagged_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "forward.responses-unavailable",
                guard_us,
                serde_json::json!({
                    "error": {
                        "type": "omniguard_upstream",
                        "rule": "forward.responses-unavailable",
                        "message": "POST /v1/responses requires UPSTREAM_API_KEY. No Responses body was fabricated and nothing was forwarded.",
                        "guard_us": guard_us
                    }
                }),
            );
        }
        state.record(NewEvent {
            direction: "forward",
            rule: "forward.demo",
            snippet,
            decision: "allow",
            guard_us,
            status: 200,
        });
        return demo_response(&path, guard_us);
    }

    match forward(&state, method, &path_and_query, &headers, text.into_bytes()).await {
        Ok(upstream) => match response::check(upstream).await {
            Ok(checked) => {
                state.record(NewEvent {
                    direction: "forward",
                    rule: "forward.upstream",
                    snippet,
                    decision: if checked.status.is_success() {
                        "allow"
                    } else {
                        "review"
                    },
                    guard_us,
                    status: checked.status.as_u16(),
                });
                buffered_back(checked, guard_us)
            }
            Err(err) => {
                let (status, rule, decision) = if err.secret {
                    state.trip_circuit();
                    (StatusCode::FORBIDDEN, "response.stripe-secret", "block")
                } else {
                    (StatusCode::BAD_GATEWAY, "response.invalid", "review")
                };
                state.record(NewEvent {
                    direction: "response",
                    rule,
                    snippet: "upstream response stopped before delivery",
                    decision,
                    guard_us,
                    status: status.as_u16(),
                });
                http::tagged_json(
                    status,
                    rule,
                    guard_us,
                    serde_json::json!({
                        "error": {"type": "omniguard_response", "rule": rule, "message": err.message},
                        "guard_us": guard_us,
                    }),
                )
            }
        },
        Err(err) => {
            tracing::warn!(error = %err, "upstream unreachable");
            state.record(NewEvent {
                direction: "forward",
                rule: "forward.unreachable",
                snippet: "upstream did not accept the connection",
                decision: "review",
                guard_us,
                status: 502,
            });
            http::tagged_json(
                StatusCode::BAD_GATEWAY,
                "forward.unreachable",
                guard_us,
                serde_json::json!({
                    "error": {
                        "type": "omniguard_upstream",
                        "rule": "forward.unreachable",
                        "message": err.to_string(),
                        "guard_us": guard_us
                    }
                }),
            )
        }
    }
}

fn elapsed_us(started: Instant) -> u64 {
    started.elapsed().as_micros() as u64
}

fn demo_response(path: &str, guard_us: u64) -> Response {
    let body = if path.ends_with("/messages") {
        serde_json::json!({
            "id": "msg_omniguard_demo",
            "type": "message",
            "role": "assistant",
            "model": "omniguard-demo",
            "content": [{
                "type": "text",
                "text": "Local demo completion. No application upstream was called; enabled AI guards may contact the configured provider."
            }],
            "stop_reason": "end_turn"
        })
    } else {
        serde_json::json!({
            "id": "chatcmpl-omniguard-demo",
            "object": "chat.completion",
            "model": "omniguard-demo",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Local demo completion. No application upstream was called; enabled AI guards may contact the configured provider."
                },
                "finish_reason": "stop"
            }]
        })
    };
    let mut response = http::tagged_json(StatusCode::OK, "forward.demo", guard_us, body);
    response
        .headers_mut()
        .insert("x-omniguard-upstream", HeaderValue::from_static("demo"));
    response
}

/// 只在守衛放行之後呼叫。body 是檢查過的 JSON 解析結果，不是打碼後的文字。
async fn forward(
    state: &AppState,
    method: Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> Result<reqwest::Response, reqwest::Error> {
    let url = format!("{}{path_and_query}", state.config().upstream_base);
    let method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::POST);
    let mut builder = state.http().request(method, url);
    for (name, value) in headers {
        let lower = name.as_str();
        if STRIP_REQUEST.contains(&lower)
            || lower.starts_with("x-omniguard-")
            || matches!(lower, "authorization" | "x-api-key" | "api-key")
        {
            continue;
        }
        if let Ok(text) = value.to_str() {
            builder = builder.header(lower, text);
        }
    }
    // SDK 常要求占位金鑰；上游一律使用伺服器設定，不能被呼叫端覆寫。
    if let Some(key) = &state.config().upstream_key {
        builder = builder.bearer_auth(key);
        if path_and_query.split('?').next() == Some("/v1/messages") {
            builder = builder.header("x-api-key", key);
        }
    }
    builder.body(body).send().await
}

fn buffered_back(upstream: response::CheckedResponse, guard_us: u64) -> Response {
    let mut builder = Response::builder().status(upstream.status);
    for (name, value) in &upstream.headers {
        if STRIP_RESPONSE.contains(&name.as_str()) || name.as_str().starts_with("x-omniguard-") {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .header("x-omniguard-guard-us", guard_us.to_string())
        .header("x-omniguard-rule", "forward.upstream")
        .header("x-omniguard-upstream", "live")
        .body(Body::from(upstream.body))
        .expect("checked response")
}
