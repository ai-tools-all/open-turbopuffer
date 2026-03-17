# open-turbopuffer

Vector database server inspired by [turbopuffer](https://turbopuffer.com), built in Rust. Uses SPFresh-based ANN indexing with S3 as the storage backend.

## Architecture

```
Client ──REST──▶ tpuf-server          tpuf-indexer
                  writes → WAL + S3      reads WAL from S3
                  queries → ANN + tail   builds SPFresh index
                  hot-reloads index      writes index to S3
                        │                      │
                        └──────── S3 ──────────┘
```

- **tpuf-server** — Axum HTTP server. Writes go to WAL on S3. Queries merge ANN index results with brute-force scan of unindexed WAL tail.
- **tpuf-indexer** — Separate binary. Reads WAL, builds/updates SPFresh index, writes back to S3. Run as cron job or on-demand.
- **tpuf-cli** — CLI client for testing (create namespaces, generate vectors, query).

## Quick Start

```bash
# Start MinIO (S3-compatible storage)
docker-compose up -d

# Start the server
./run.sh

# Create a namespace and ingest vectors
cargo run -p tpuf-cli -- create-ns --name demo --metric euclidean_squared
cargo run -p tpuf-cli -- generate --namespace demo --count 1000 --dims 64 --batch-size 200

# Build the index
cargo run -p tpuf-indexer -- --namespace demo

# Query (server hot-reloads index within 30s)
cargo run -p tpuf-cli -- query --namespace demo --dims 64 --top-k 10
```

## API

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/v1/namespaces` | Create namespace |
| GET | `/v1/namespaces` | List namespaces |
| DELETE | `/v1/namespaces/{ns}` | Delete namespace |
| POST | `/v1/namespaces/{ns}/documents` | Upsert documents |
| DELETE | `/v1/namespaces/{ns}/documents` | Delete documents |
| GET | `/v1/namespaces/{ns}/documents/{id}` | Get document |
| POST | `/v1/namespaces/{ns}/query` | Vector search |

## Development

```bash
cargo build                  # build workspace
cargo test                   # run all tests (76 tests)
cargo clippy -p tpuf-server  # lint
```

Requires Docker for MinIO. Config in `.env`, sourced by `run.sh`.
