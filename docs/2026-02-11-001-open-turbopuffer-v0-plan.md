# Open Turbopuffer v0 — S3-Backed Vector Search

> Date: 2026-02-11 | Status: APPROVED

## Goal

Build a minimal, working S3-native vector search engine inspired by turbopuffer's architecture. V0 focuses on: **WAL on S3 (MinIO) + brute-force vector search + REST API**.

No indexing in v0 — just correct, durable, searchable vector storage on object storage.

---

## Architecture Overview

```
┌──────────┐     HTTP/JSON      ┌────────────────┐
│  Client   │ ──────────────→   │  Query Server   │
└──────────┘                    │  (Rust/Axum)    │
                                │                  │
                                │  ┌────────────┐ │
                                │  │ RAM Cache   │ │
                                │  │ (moka)      │ │
                                │  └──────┬─────┘ │
                                └─────────┼───────┘
                                          │
                                ┌─────────▼───────┐
                                │  MinIO (S3)      │
                                │  /{ns}/wal/      │
                                │  /{ns}/meta.bin  │
                                └──────────────────┘
```

**Single binary, single process for v0.** No separate indexer node yet.

---

## Language & Stack

| Component | Choice | Rationale |
|-----------|--------|-----------|
| Language | **Rust** | Matches turbopuffer; perf-critical |
| HTTP | axum | Async, lightweight |
| S3 client | opendal | Unified abstraction; MinIO/S3/local FS |
| Serialization | bincode + serde | Fast binary, turbopuffer uses this |
| Compression | zstd | Turbopuffer default |
| Cache | moka | W-TinyLFU, async, weight-based eviction |
| Vector math | manual f32 ops (no SIMD v0) | Keep simple |
| Dev S3 | MinIO via Docker | Local dev parity |

---

## Data Model

### Document
```rust
struct Document {
    id: u64,                                    // simple u64 for v0
    vector: Vec<f32>,                           // f32 only for v0
    attributes: HashMap<String, AttributeValue>,
}

enum AttributeValue {
    String(String),
    U64(u64),
    F64(f64),
    Bool(bool),
    Null,
}
```

### Namespace
```rust
struct NamespaceMetadata {
    name: String,
    dimensions: usize,
    distance_metric: DistanceMetric,  // Cosine | Euclidean | DotProduct
    wal_sequence: u64,
    doc_count: u64,
    created_at: u64,
}
```

### WAL Entry (stored on S3)
```rust
struct WalEntry {
    sequence: u64,
    timestamp_ms: u64,
    operations: Vec<WriteOp>,
}

enum WriteOp {
    Upsert(Vec<Document>),
    Delete(Vec<u64>),       // by ID
}
```

### S3 Key Layout
```
{namespace}/meta.bin                    — namespace metadata (bincode+zstd)
{namespace}/wal/{sequence:020}.wal      — WAL entries (bincode+zstd)
```

---

## API Design (REST/JSON)

### Namespaces
```
POST   /v1/namespaces                          — create namespace
GET    /v1/namespaces                          — list namespaces
DELETE /v1/namespaces/{ns}                     — delete namespace
```

### Documents
```
POST   /v1/namespaces/{ns}/documents           — upsert batch
DELETE /v1/namespaces/{ns}/documents            — delete by IDs
GET    /v1/namespaces/{ns}/documents/{id}      — get single doc
```

### Search
```
POST   /v1/namespaces/{ns}/query
Body: {
  "vector": [0.1, 0.2, ...],
  "top_k": 10,
  "distance_metric": "cosine_distance",   // optional override
  "include_vectors": false,
  "include_attributes": ["field1"]
}
Response: {
  "results": [
    { "id": 123, "dist": 0.05, "attributes": {...} }
  ]
}
```

---

## V0 Implementation Phases

### Phase 1: Project Scaffold & MinIO Setup
- Cargo workspace setup
- Docker Compose with MinIO
- OpenDAL S3 connection verified
- Basic health endpoint

### Phase 2: Namespace CRUD
- Create/list/delete namespaces
- Metadata stored as `{ns}/meta.bin` on S3
- In-memory namespace registry with S3 sync

### Phase 3: Write Path (WAL)
- Upsert & delete operations
- WAL entries serialized with bincode, compressed with zstd
- Sequential numbering, 1 WAL entry per write batch
- Write batching (collect writes for up to 500ms before flushing)

### Phase 4: Read Path (Brute-Force Search)
- Load all WAL entries for namespace → materialize full document set in memory
- Brute-force kNN: compute distance to every vector, return top_k
- Cosine distance, euclidean distance, dot product
- Moka cache for WAL entries to avoid re-reading S3

### Phase 5: Document Retrieval
- Get document by ID
- Delete by ID list

### Phase 6: Integration & Testing
- End-to-end tests against MinIO
- Basic benchmarks (insert N vectors, query latency)
- Error handling & validation

---

## Docker Compose (MinIO)

```yaml
services:
  minio:
    image: minio/minio:latest
    command: server /data --console-address ":9001"
    ports:
      - "9000:9000"   # S3 API
      - "9001:9001"   # Console
    environment:
      MINIO_ROOT_USER: minioadmin
      MINIO_ROOT_PASSWORD: minioadmin
    volumes:
      - minio-data:/data

  createbucket:
    image: minio/mc:latest
    depends_on: [minio]
    entrypoint: >
      /bin/sh -c "
      sleep 3;
      mc alias set local http://minio:9000 minioadmin minioadmin;
      mc mb local/turbopuffer --ignore-existing;
      "

volumes:
  minio-data:
```

---

## What V0 Does NOT Include

| Feature | Why Deferred |
|---------|-------------|
| Vector indexing (SPFresh/IVF) | Brute-force first, index later |
| Binary quantization (RaBitQ) | Optimization, not correctness |
| SSD/NVMe cache tier | RAM cache sufficient for v0 |
| Full-text search (BM25) | Phase 4+ feature |
| Attribute filtering | Phase 3+ feature |
| Multi-node / stateless scaling | Single process for v0 |
| Compaction | WAL-only for v0, compact later |
| Conditional writes / transactions | Advanced feature |
| f16/binary vector types | f32 only |

---

## Decisions

| # | Question | Decision |
|---|----------|----------|
| 1 | Namespace dimensions | Auto-detect from first upsert (turbopuffer style) |
| 2 | ID type | `u64` only |
| 3 | Write batching | 500ms batching discipline from v0 |
| 4 | Cache size | 256MB default |
| 5 | Auth | None — open localhost |
| 6 | Persistence | Replay all WAL on startup (correctness first) |
| 7 | Test data | Separate CLI crate (`tpuf-cli`) for generate/ingest/query |
| 8 | API compat | Best-effort turbopuffer API shape, not a hard constraint |

## Workspace Structure

```
open-turbopuffer/
├── Cargo.toml              # workspace root
├── docker-compose.yml
├── crates/
│   ├── tpuf-server/        # main server binary
│   │   └── src/
│   │       ├── main.rs
│   │       ├── api/        # axum routes
│   │       ├── storage/    # S3/WAL layer
│   │       ├── engine/     # namespace, search, write batcher
│   │       └── types/      # Document, WalEntry, etc.
│   └── tpuf-cli/           # CLI for testing (generate, ingest, query)
│       └── src/
│           └── main.rs
└── docs/
```

---

## Success Criteria for V0

- [ ] `docker compose up` starts MinIO + turbopuffer server
- [ ] Create namespace, upsert 10K vectors (128-dim), query top-10 — correct results
- [ ] Data survives server restart (WAL replay from S3)
- [ ] Query latency < 100ms for 10K vectors (brute-force, warm cache)
- [ ] Clean error messages for invalid requests
