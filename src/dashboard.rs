//! 儀表板頁。HTML 嵌在二進位裡，容器不必再掛靜態檔。

use axum::http::header;
use axum::response::IntoResponse;

pub async fn page() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        include_str!("dashboard.html"),
    )
}
