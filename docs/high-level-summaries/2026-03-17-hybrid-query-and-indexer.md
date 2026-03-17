---
title: "Hybrid Query + Separate Indexer + Hot-Reload"
date: 2026-03-17
covers:
  - "B4/C4: Search unindexed WAL tail"
  - "B5/D5: Separate indexer crate"
  - "Index hot-reload"
status: done
---

# Hybrid Query + Separate Indexer + Hot-Reload

## What changed

The server no longer builds or updates the index inline. A separate `tpuf-indexer` binary reads WAL from S3, builds the SPFresh index, and writes it back. The server loads the index from S3 and hot-reloads it when the manifest changes.

Queries use a **hybrid strategy**: ANN search on the indexed portion + brute-force on the unindexed WAL tail, merged into a single result set.

## Architecture

```
Client ──API──▶ tpuf-server
                 - writes: WAL + documents HashMap (no inline indexing)
                 - queries: ANN index + brute-force tail
                 - hot-reloads index from S3 every 30s
                      │
                      ▼
                 S3 (MinIO / R2)
                 {ns}/wal/{seq}.wal
                 {ns}/index/manifest.bin     ← wal_sequence marker
                 {ns}/index/centroids.bin
                 {ns}/index/postings/{hid}.bin
                      ▲
                      │
                 tpuf-indexer (separate binary)
                 - reads WAL from S3
                 - builds/updates SPFresh index
                 - writes index + manifest (CAS)
```

## Key design decisions

**Manifest's `wal_sequence` is the coordination point.** The server knows: everything ≤ this sequence is in the index, everything after is the tail. No WAL compaction needed.

**Deduplication on upsert.** If a document is updated after the index was built, the ANN result for the old vector is filtered out — the tail's brute-force result (with the current vector) takes precedence.

**No inline indexing.** `flush_batcher()` writes to WAL + documents HashMap only. Index is never modified by the server. This means writes never block on SPFresh splits.

**Hot-reload.** Background task polls manifest etag every 30s. On change, reloads the full index from S3 and updates `indexed_up_to_seq`. No restart needed.

## Commits

| Commit | What |
|--------|------|
| `07fa7d9` | Phase 1: hybrid query — ANN + brute-force tail, merge, dedup |
| `294a7d1` | Phase 2: tpuf-indexer crate (CLI + core logic) |
| `585d3bc` | Phase 3: remove build_index_from_documents, server loads from manifest only |
| `a51875e` | Query source logging (index vs tail vs brute-force) |
| `8f086b4` | Index hot-reload (30s poll) |

## Test workflow

```bash
# Start server
./run.sh

# Ingest 1000 vectors
cargo run -p tpuf-cli -- generate --namespace logs --count 1000 --dims 64 --batch-size 200

# Build index
cargo run -p tpuf-indexer -- --namespace logs

# Wait 30s for hot-reload (or restart server)

# Ingest 50 more (tail)
cargo run -p tpuf-cli -- generate --namespace logs --count 50 --dims 64 --batch-size 50

# Query — hybrid results
cargo run -p tpuf-cli -- query --namespace logs --dims 64 --top-k 10
```

Server logs show: `query: hybrid ... from_index=N from_tail=M`

## Test coverage

76 tests (72 server + 4 indexer):
- Hybrid query: finds unindexed, recall@10 > 0.85, dedup, delete in tail
- Indexer: from scratch, incremental, noop, server loads output
- Existing: persist/load roundtrip, recall, mixed ops, brute-force fallback

## Remaining gaps

| Gap | Status |
|-----|--------|
| A5: WAL compaction | Not started — WAL grows unbounded |
| B6: BM25 full-text search | Not started |
| B7: Metadata attribute filtering | Not started |
