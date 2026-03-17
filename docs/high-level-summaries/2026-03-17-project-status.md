---
title: "open-turbopuffer — Project Status"
date: 2026-03-17
---

# open-turbopuffer — What's Built

S3-native vector search engine inspired by turbopuffer. Rust workspace, ~4400 LOC, 67 tests (+ 3 R2 integration tests).

## Architecture

```
Client → HTTP/JSON → tpuf-server (Axum) → OpenDAL → S3 (MinIO / Cloudflare R2)
                         │
                         ├── NamespaceManager (CRUD, query, upsert, delete)
                         ├── SPFreshIndex (ANN search, in-memory)
                         ├── WAL (append-only on S3)
                         └── Index persistence (centroids, postings, version_map on S3)
```

## Layers

### 1. HTTP API (`api/`)
REST endpoints for namespace CRUD, document upsert/delete/get, and vector query.

### 2. Engine (`engine/`)

**NamespaceManager** — owns namespace state: documents (HashMap), SPFreshIndex, metadata.
- Inserts/deletes feed into both WAL and the SPFresh index
- Queries use ANN index when available, brute-force fallback for empty namespaces
- WAL replay rebuilds index on startup (or loads from S3 if persisted)

**WriteBatcher** — 500ms batching window before flushing to WAL.

### 3. SPFresh Index (`engine/index/spfresh/`)

Cluster-based ANN index (10 files, based on the SPFresh paper):

| File | What |
|------|------|
| `config.rs` | Index configuration (dims, max_posting_size, replica_count, etc.) |
| `head_index.rs` | Centroid storage — flat Vec<f32>, brute-force centroid search |
| `posting.rs` | PostingEntry/PostingList — vector ID + version + vector data |
| `posting_store.rs` | MemoryPostingStore — HashMap<u32, PostingList>, trait-based |
| `version_map.rs` | Atomic u8 per vector, CAS increment, deletion tracking |
| `kmeans.rs` | Binary k-means for cluster splitting |
| `rebuilder.rs` | LIRE split/reassign protocol — splits oversized postings recursively |
| `updater.rs` | Insert with RNG diversity filter, delete via version map |
| `search.rs` | Search postings with version/deletion filtering, dedup across clusters |
| `mod.rs` | SPFreshIndex — ties everything together, implements VectorIndex trait |

**Recall:** ~90%+ recall@10 on random 128-dim data at 5K vectors. E2E through NamespaceManager: recall@10 = 1.0.

### 4. Storage (`storage/`)

OpenDAL-based S3 abstraction. bincode + zstd encoding for all objects.

**S3 layout:**
```
{ns}/meta.bin                          — namespace metadata
{ns}/wal/{seq:020}.wal                 — WAL entries (If-None-Match: *)
{ns}/index/manifest.bin                — index commit point (If-Match CAS)
{ns}/index/centroids.bin               — HeadIndex snapshot
{ns}/index/version_map.bin             — VersionMap snapshot
{ns}/index/postings/{hid:010}.bin      — one file per active centroid
```

**Conditional writes:**
- WAL: `If-None-Match: *` — prevents duplicate sequence numbers
- Manifest: `If-Match: <etag>` — CAS prevents double-indexer conflicts
- Falls back to unconditional writes on backends without support (Memory)

### 5. CLI (`tpuf-cli/`)

Test client: create namespaces, generate/ingest random vectors, query, get documents.

## Key Design Decisions

| Decision | Choice |
|----------|--------|
| Index persistence | Manifest-last protocol — crash between writes is safe |
| WAL catch-up | Load index from S3, replay WAL entries after manifest's sequence |
| Fallback | No manifest → full WAL rebuild → brute-force works until index ready |
| Conditional writes | Assertions (fail loud), not retry logic — single-writer invariant |
| Testing | In-memory OpenDAL for unit tests, Cloudflare R2 for integration tests |
| Serialization | bincode + zstd (same as turbopuffer) |

## Test Coverage

67 unit/integration tests + 3 R2 integration tests (`--ignored`):

| Area | Tests | What |
|------|-------|------|
| SPFresh core | 27 | Insert, delete, search, splits, replica placement, recall, concurrent |
| Serialization | 6 | bincode roundtrips for all index types |
| Storage (Memory) | 6 | Write/read for centroids, postings, version_map, manifest |
| Storage (R2) | 3 | WAL exclusive, manifest CAS, full persist roundtrip |
| Namespace (E2E) | 8 | Index wiring, query path, deletion, recall@10, mixed ops, persist/load |
| Types | 1 | IndexManifest roundtrip |

## What's Not Built Yet

| Feature | Notes |
|---------|-------|
| Periodic index persistence | `persist_index()` is manual — needs background timer (~30s) |
| Shadow verification | ANN vs brute-force comparison behind config flag |
| LRU posting cache | All postings in memory — need LRU for datasets > RAM |
| Attribute filtering | Filter query results by metadata predicates |
| WAL compaction | Merge old WAL entries, reclaim S3 space |
| Immutable generation objects | S3-native redesign from `spfresh-s3-redesign.md` |

## How to Run

```bash
# Dev server (MinIO)
./run.sh

# Unit tests
cargo test -p tpuf-server

# R2 integration tests
source .env.local.cf && cargo test -p tpuf-server -- --ignored test_r2 --nocapture
```
