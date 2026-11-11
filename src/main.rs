//! 程序進入點。設定由環境變數注入，金鑰不寫入事件。

use omniguard::Config;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "omniguard=info,tower_http=info".into()),
        )
        .init();

    let config = Config::from_env();
    let listen_at = config.bind.clone();
    let shown = listen_at.replacen("0.0.0.0", "127.0.0.1", 1);
    tracing::info!(
        upstream = config.upstream_mode(),
        vision = config.vision_mode(),
        semantic = config.semantic_mode(),
        "OmniGuard dashboard http://{shown}/"
    );

    let app = omniguard::router(config);
    let listener = tokio::net::TcpListener::bind(&listen_at)
        .await
        .unwrap_or_else(|err| panic!("bind {listen_at} failed: {err}"));
    axum::serve(listener, app)
        .await
        .unwrap_or_else(|err| panic!("server stopped: {err}"));
}
