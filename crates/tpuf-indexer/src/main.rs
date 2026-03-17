mod indexer;

use clap::Parser;
use tracing::info;
use tpuf_server::storage::ObjectStore;

#[derive(Parser)]
#[command(name = "tpuf-indexer", about = "Build SPFresh index from WAL entries")]
struct Cli {
    #[arg(long)]
    namespace: String,

    #[arg(long, default_value = "http://localhost:9002")]
    s3_endpoint: String,

    #[arg(long, default_value = "turbopuffer")]
    s3_bucket: String,

    #[arg(long, default_value = "minioadmin")]
    s3_access_key: String,

    #[arg(long, default_value = "minioadmin")]
    s3_secret_key: String,

    #[arg(long, default_value = "us-east-1")]
    s3_region: String,

    #[arg(long)]
    r2_account_id: Option<String>,

    #[arg(long)]
    r2_bucket: Option<String>,

    #[arg(long)]
    r2_access_key: Option<String>,

    #[arg(long)]
    r2_secret_key: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tpuf_indexer=info".into()),
        )
        .init();

    let cli = Cli::parse();

    let store = if let (Some(account_id), Some(bucket), Some(access_key), Some(secret_key)) = (
        &cli.r2_account_id,
        &cli.r2_bucket,
        &cli.r2_access_key,
        &cli.r2_secret_key,
    ) {
        info!("connecting to R2");
        ObjectStore::new_r2(account_id, bucket, access_key, secret_key)?
    } else {
        info!(endpoint = %cli.s3_endpoint, bucket = %cli.s3_bucket, "connecting to S3");
        ObjectStore::new(
            &cli.s3_endpoint,
            &cli.s3_bucket,
            &cli.s3_access_key,
            &cli.s3_secret_key,
            &cli.s3_region,
        )?
    };

    indexer::run_indexer(&store, &cli.namespace).await?;

    Ok(())
}
