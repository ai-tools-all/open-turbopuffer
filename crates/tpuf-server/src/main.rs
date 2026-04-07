use std::sync::Arc;
use std::path::PathBuf;
use tracing::info;
use tpuf_server::{api, engine, storage};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tpuf_server=info".into()),
        )
        .init();

    let endpoint = std::env::var("S3_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".into());
    let bucket = std::env::var("S3_BUCKET").unwrap_or_else(|_| "turbopuffer".into());
    let access_key = std::env::var("S3_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".into());
    let secret_key = std::env::var("S3_SECRET_KEY").unwrap_or_else(|_| "minioadmin".into());
    let region = std::env::var("S3_REGION").unwrap_or_else(|_| "us-east-1".into());
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3000);

    let cache_dir = std::env::var("CACHE_DIR").unwrap_or_else(|_| "/tmp/tpuf-cache".into());
    let cache_memory_mb: usize = std::env::var("CACHE_MEMORY_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(64);
    let cache_disk_mb: usize = std::env::var("CACHE_DISK_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(1024);

    let cache_config = storage::CacheConfig {
        dir: PathBuf::from(cache_dir),
        memory_capacity: cache_memory_mb * 1024 * 1024,
        disk_capacity: cache_disk_mb * 1024 * 1024,
    };

    let store = storage::ObjectStore::new(&endpoint, &bucket, &access_key, &secret_key, &region)?;
    let mgr = Arc::new(engine::NamespaceManager::with_cache(store, &cache_config).await?);

    info!("replaying WAL from S3...");
    mgr.init().await?;
    info!("WAL replay complete");

    let reload_mgr = Arc::clone(&mgr);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.tick().await;
        loop {
            interval.tick().await;
            reload_mgr.reload_indexes_if_changed().await;
        }
    });
    info!("index hot-reload enabled (30s interval)");

    let app = api::router().with_state(mgr);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    info!(port, "tpuf-server listening");
    axum::serve(listener, app).await?;

    Ok(())
}
