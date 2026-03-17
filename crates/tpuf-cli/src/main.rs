use clap::{Parser, Subcommand};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Parser)]
#[command(name = "tpuf-cli", about = "CLI for testing open-turbopuffer")]
struct Cli {
    #[arg(long, default_value = "http://localhost:3000")]
    server: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    CreateNs {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "cosine_distance")]
        metric: String,
    },
    ListNs,
    DeleteNs {
        #[arg(long)]
        name: String,
    },
    Generate {
        #[arg(long)]
        namespace: String,
        #[arg(long, default_value_t = 1000)]
        count: usize,
        #[arg(long, default_value_t = 128)]
        dims: usize,
        #[arg(long, default_value_t = 100)]
        batch_size: usize,
    },
    Query {
        #[arg(long)]
        namespace: String,
        #[arg(long, default_value_t = 128)]
        dims: usize,
        #[arg(long, default_value_t = 10)]
        top_k: usize,
        #[arg(long)]
        include_vectors: bool,
    },
    Get {
        #[arg(long)]
        namespace: String,
        #[arg(long)]
        id: u64,
    },
}

#[derive(Serialize)]
struct UpsertReq {
    documents: Vec<DocInput>,
}

#[derive(Serialize)]
struct DocInput {
    id: u64,
    vector: Vec<f32>,
    attributes: HashMap<String, serde_json::Value>,
}

#[derive(Serialize)]
struct QueryReq {
    vector: Vec<f32>,
    top_k: usize,
    include_vectors: bool,
}

#[derive(Deserialize, Debug)]
struct QueryResp {
    results: Vec<QueryResultItem>,
}

#[derive(Deserialize, Debug)]
struct QueryResultItem {
    id: u64,
    dist: f32,
    attributes: HashMap<String, serde_json::Value>,
}

fn random_vector(dims: usize) -> Vec<f32> {
    let mut rng = rand::thread_rng();
    let v: Vec<f32> = (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect();
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.into_iter().map(|x| x / norm).collect()
    } else {
        v
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();
    let base = &cli.server;

    match cli.command {
        Commands::CreateNs { name, metric } => {
            let resp = client.post(format!("{}/v1/namespaces", base))
                .json(&serde_json::json!({
                    "name": name,
                    "distance_metric": metric,
                }))
                .send().await?;
            println!("status: {}", resp.status());
        }
        Commands::ListNs => {
            let resp = client.get(format!("{}/v1/namespaces", base))
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            println!("{}", serde_json::to_string_pretty(&body)?);
        }
        Commands::DeleteNs { name } => {
            let resp = client.delete(format!("{}/v1/namespaces/{}", base, name))
                .send().await?;
            println!("status: {}", resp.status());
        }
        Commands::Generate { namespace, count, dims, batch_size } => {
            println!("generating and ingesting {} vectors ({}d) into '{}'", count, dims, namespace);
            let mut ingested = 0;
            while ingested < count {
                let batch_end = (ingested + batch_size).min(count);
                let documents: Vec<DocInput> = (ingested..batch_end).map(|i| {
                    let mut attrs = HashMap::new();
                    attrs.insert("batch".to_string(), serde_json::json!(i / batch_size));
                    DocInput {
                        id: i as u64,
                        vector: random_vector(dims),
                        attributes: attrs,
                    }
                }).collect();

                let resp = client.post(format!("{}/v1/namespaces/{}/documents", base, namespace))
                    .json(&UpsertReq { documents })
                    .send().await?;

                if !resp.status().is_success() {
                    let body = resp.text().await?;
                    eprintln!("error at batch {}: {}", ingested, body);
                    return Ok(());
                }

                ingested = batch_end;
                println!("  ingested {}/{}", ingested, count);
            }
            println!("done: {} vectors ingested", count);
        }
        Commands::Query { namespace, dims, top_k, include_vectors } => {
            let query_vec = random_vector(dims);
            println!("querying '{}' with random {}d vector, top_k={}", namespace, dims, top_k);

            let start = std::time::Instant::now();
            let resp = client.post(format!("{}/v1/namespaces/{}/query", base, namespace))
                .json(&QueryReq { vector: query_vec, top_k, include_vectors })
                .send().await?;
            let elapsed = start.elapsed();

            if !resp.status().is_success() {
                let body = resp.text().await?;
                eprintln!("error: {}", body);
                return Ok(());
            }

            let result: QueryResp = resp.json().await?;
            println!("results ({} hits, {:.1}ms):", result.results.len(), elapsed.as_secs_f64() * 1000.0);
            for r in &result.results {
                println!("  id={} dist={:.6} attrs={:?}", r.id, r.dist, r.attributes);
            }
        }
        Commands::Get { namespace, id } => {
            let resp = client.get(format!("{}/v1/namespaces/{}/documents/{}", base, namespace, id))
                .send().await?;
            if resp.status().is_success() {
                let body: serde_json::Value = resp.json().await?;
                println!("{}", serde_json::to_string_pretty(&body)?);
            } else {
                println!("status: {}", resp.status());
            }
        }
    }

    Ok(())
}
