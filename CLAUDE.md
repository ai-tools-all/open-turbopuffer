# open-turbopuffer

Rust workspace implementing a vector database server inspired by turbopuffer, using SPFresh-based indexing with S3 (MinIO) as the storage backend.

## Project Structure

```
crates/
  tpuf-server/    # Axum HTTP server — API, engine (namespaces, search, batcher), storage (OpenDAL/S3), SPFresh index
  tpuf-cli/       # CLI client (clap + reqwest)
```

## Dev Environment

- **MinIO** via `docker-compose.yml` — S3-compatible storage (API: port 9002, console: port 9003)
- **`.env`** — sourced by `run.sh`, contains `S3_ENDPOINT`, `S3_BUCKET`, `S3_ACCESS_KEY`, `S3_SECRET_KEY`, `S3_REGION`, `PORT`, `RUST_LOG`
- Default S3 endpoint in code is `:9000` but docker maps to `:9002` — always use `.env` or `run.sh`

## Commands

```bash
./run.sh                     # start MinIO (if needed) + run server with .env
cargo build                  # build workspace
cargo test                   # run all tests
cargo test -p tpuf-server    # server tests only
cargo clippy --workspace     # lint
```

## Key Conventions

- All tests are inline `#[cfg(test)]` modules (no separate `tests/` dirs)
- Error handling: `anyhow` for main, `thiserror` for library errors
- Logging: `tracing` crate, controlled via `RUST_LOG` env var
- Storage abstraction via OpenDAL — bucket must exist before server starts
- No comments in code unless necessary
