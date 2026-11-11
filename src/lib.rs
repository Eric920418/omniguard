//! OmniGuard：跑在客戶 VPC 裡的 AI 出入口防火牆。
//!
//! 本地保存狀態與事件；已啟用的 AI 檢查會呼叫設定的模型服務：
//! - [`proxy`] 執行本地與 AI 提示詞檢查，並掃描上下游 body
//! - [`inspect`] 對 Agent 送來的外部網址做註冊域與視覺訊號比對
//! - 儀表板只讀這塊行程記憶體裡的決定，對話本身不落盤

pub mod dashboard;
pub mod guard;
pub mod http;
pub mod inspect;
pub mod proxy;
pub mod state;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;

pub use state::Config;

/// 組出給 `axum::serve` 與測試共用的路由。
/// 狀態放在 `Arc` 裡，儀表板與代理看的是同一份斷路器與事件環。
pub fn router(config: Config) -> Router {
    let state = state::AppState::new(config);
    Router::new()
        .route("/v1/chat/completions", post(proxy::handle))
        .route("/v1/messages", post(proxy::handle))
        // 明確掛上 Responses，不開萬用路由，embeddings 也不在這次範圍。
        .route("/v1/responses", post(proxy::handle))
        .route("/v1/guard/analyze", post(proxy::handle))
        .route("/v1/guard/inspect", post(inspect::handle))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            http::limit_requests,
        ))
        .route("/api/events", get(http::events))
        .route("/api/stats", get(http::stats))
        .route("/api/circuit/reset", post(http::reset_circuit))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            http::authenticate,
        ))
        .route("/", get(dashboard::page))
        .route("/healthz", get(http::health))
        // 請求必須先收完才能決定切不切斷，所以先卡上限，避免無界緩衝。
        .layer(DefaultBodyLimit::max(state::BODY_LIMIT))
        .with_state(state)
}
