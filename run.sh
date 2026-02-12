#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

set -a
source .env
set +a

if ! docker-compose ps --status running 2>/dev/null | grep -q minio; then
  echo "Starting MinIO..."
  docker-compose up -d
  echo "Waiting for MinIO to be healthy..."
  docker-compose exec minio mc ready local --quiet 2>/dev/null || sleep 5
fi

echo "Starting tpuf-server (debug)..."
cargo run -p tpuf-server
