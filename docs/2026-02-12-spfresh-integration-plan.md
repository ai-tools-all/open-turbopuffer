# SPFresh Integration Plan — Separate Indexer + S3 Storage

**Status**: DRAFT
**Date**: 2026-02-12
**Depends on**: v0 plan (2026-02-11-001), SPFresh impl plan (2026-02-12-spfresh-index-plan.md)

---

## 1. The Idea

Decouple indexing from serving. Two processes:

```
                  ┌─────────────┐
                  │  tpuf-server │  (query server)
                  │  reads index │
                  │  from S3     │
                  └──────┬───────┘
                         │ reads
                         ▼
┌─────────────┐    ┌──────────────┐
│  tpuf-server │    │   MinIO/S3   │
│  ingests     │───→│              │
│  writes WAL  │    │  /{ns}/wal/  │
└─────────────┘    │  /{ns}/index/ │  ← indexer writes here
                   └──────┬───────┘
                          │ reads WAL
                          │ writes index
                   ┌──────┴───────┐
                   │  tpuf-indexer │  (CLI / background)
                   │  builds index │
                   └──────────────┘
```

**Query server** never builds the index. It loads pre-built index files from S3.
**Indexer** reads WAL, builds SPFresh centroids + postings, writes them to S3.

---

## 2. Why This Design

| Concern | Decision | Rationale |
|---------|----------|-----------|
| Indexing is CPU-heavy | Separate process | Don't block queries during split/reassign |
| Index is read-heavy | Pre-built on S3 | Query server just fetches, no compute |
| Crash isolation | Indexer crash ≠ query outage | WAL still works, brute-force fallback |
| Scaling | Run N query servers, 1 indexer | Index built once, read many |
| Incremental | Indexer picks up from last WAL seq | No full rebuild needed |

---

## 3. S3 Key Layout

Current (v0):
```
{ns}/meta.bin
{ns}/wal/{seq:020}.wal
```

New (with index):
```
{ns}/meta.bin                              # namespace metadata
{ns}/wal/{seq:020}.wal                     # WAL entries (unchanged)
{ns}/index/manifest.bin                    # index manifest (version, wal_seq, config)
{ns}/index/centroids.bin                   # all centroids (flat f32 array + active flags)
{ns}/index/postings/{head_id:010}.bin      # one file per posting list
{ns}/index/version_map.bin                 # version map snapshot
```

### File Formats

All files use existing `bincode + zstd` encoding (same as WAL).

**`manifest.bin`**:
```rust
struct IndexManifest {
    version: u32,                    // format version (1)
    wal_sequence: u64,               // WAL seq this index covers up to
    config: SPFreshConfig,           // index parameters used
    num_centroids: u32,              // total centroid slots (including inactive)
    active_centroids: u32,           // count of active centroids
    num_vectors: u64,                // total indexed vectors
    created_at: u64,                 // timestamp ms
}
```

**`centroids.bin`**:
```rust
struct CentroidsFile {
    dims: usize,
    centroids: Vec<f32>,             // flat [c0_d0, c0_d1, ..., c1_d0, ...]
    active: Vec<bool>,               // active[i] = whether centroid i exists
}
```

**`postings/{head_id}.bin`**:
```rust
// Just a serialized PostingList — Vec<PostingEntry>
// One file per active centroid
// Typical size: 4KB–256KB depending on max_posting_size and dims
```

**`version_map.bin`**:
```rust
struct VersionMapFile {
    capacity: u64,
    data: Vec<u8>,                   // raw version bytes
}
```

### Why Separate Files for Postings

- Query server only needs to fetch postings for the top-K centroids (typically 32-64 out of thousands)
- One file per posting → fetch only what's needed → minimal S3 reads
- Posting files are small enough for single GET requests
- Alternative (one big file) would require range reads or loading everything

---

## 4. Indexer CLI (`tpuf-indexer`)

### 4.1 Binary

New crate: `crates/tpuf-indexer/` — a CLI binary.

```
tpuf-indexer --namespace my_ns [--once | --watch]
```

**Modes**:
- `--once`: Build/update index, then exit (cron/CI friendly)
- `--watch`: Poll for new WAL entries, re-index incrementally (daemon mode)

### 4.2 Algorithm

```
index_namespace(ns):
  1. Read manifest from S3 (if exists)
     → last_indexed_seq = manifest.wal_sequence (or 0 if no manifest)

  2. List WAL sequences > last_indexed_seq
     → if none, exit (index is up to date)

  3. Load existing index from S3 (if manifest exists):
     - Read centroids.bin → rebuild HeadIndex
     - Read version_map.bin → rebuild VersionMap
     - Read postings/*.bin → rebuild MemoryPostingStore
     → This gives us SPFreshIndex at state=last_indexed_seq

  4. Replay WAL entries sequentially:
     for seq in new_wal_seqs:
       wal = read_wal(ns, seq)
       for op in wal.operations:
         match op:
           Upsert(docs) → for doc in docs: index.insert(doc.id, &doc.vector)
           Delete(ids)  → for id in ids: index.delete(id)

  5. Write updated index to S3:
     - Write centroids.bin
     - Write version_map.bin
     - Write postings/{hid}.bin for each active centroid
     - Write manifest.bin (with new wal_sequence)

  6. Done. Query server can now load the new index.
```

### 4.3 Cold Start vs Incremental

**Cold start** (no manifest): Step 3 creates empty SPFreshIndex, step 4 replays all WAL entries.

**Incremental** (manifest exists): Step 3 loads existing index, step 4 only replays new WAL entries. SPFresh handles this natively — inserts trigger splits/reassign as needed.

### 4.4 Atomicity

Problem: Query server might read index mid-write (partial centroids, missing postings).

Solution: **Manifest-last write protocol**.
1. Write all posting files first
2. Write centroids.bin
3. Write version_map.bin
4. Write manifest.bin **last**

Query server reads manifest first. If manifest points to wal_sequence=X, all files for that index version are guaranteed present. Stale posting files from previous index versions are harmless (they just won't be referenced by the current centroids).

### 4.5 Cleanup

After writing a new index, the indexer can optionally delete posting files that are no longer referenced (centroids that were removed during splits). This is a GC step, not required for correctness.

---

## 5. Query Server Changes

### 5.1 Index Loading

On startup (or periodically), the query server loads the index:

```
load_index(ns):
  1. Read manifest.bin from S3
     → if not found, fall back to brute-force
  2. Read centroids.bin → build HeadIndex
  3. Read version_map.bin → build VersionMap
  4. DON'T load postings yet (lazy fetch on query)
```

### 5.2 Query Flow (Two-Level)

```
query(ns, vector, top_k):
  1. If no index loaded → brute-force fallback (current behavior)

  2. Search centroids: HeadIndex.search(vector, num_search_heads)
     → returns top candidate head_ids

  3. Fetch postings from S3 (parallel):
     for hid in candidate_head_ids:
       posting = cache.get_or_fetch(ns, hid)
     → each posting is a small S3 GET (~4-256KB)

  4. Search within postings:
     search_postings(vector, postings, version_map, metric, top_k)

  5. Return results
```

### 5.3 Posting Cache

Postings are fetched from S3 on demand and cached in memory (moka).

```rust
struct PostingCache {
    cache: moka::future::Cache<(String, u32), PostingList>,  // (ns, head_id) → PostingList
    store: ObjectStore,
}
```

- **Eviction**: LRU/W-TinyLFU, weight = posting size in bytes
- **TTL**: Short (30s–60s) to pick up index updates
- **Warm-up**: Optionally pre-fetch top centroids on startup

### 5.4 Index Refresh

The query server periodically checks for new manifest:

```
refresh_index(ns):
  1. Read manifest.bin
  2. If manifest.wal_sequence > current_index.wal_sequence:
     - Reload centroids.bin
     - Reload version_map.bin
     - Invalidate posting cache
  3. Refresh interval: configurable (default: 30s)
```

### 5.5 Fallback

If index doesn't exist or is stale (WAL has entries beyond index), the query server can:
- **Option A**: Brute-force over in-memory docs (current behavior, always correct)
- **Option B**: Use stale index for approximate results + scan recent WAL entries for freshness

For v1, Option A is sufficient. Index is a performance optimization, not a correctness requirement.

---

## 6. What Changes Where

### New Crate
```
crates/tpuf-indexer/
├── Cargo.toml
└── src/
    └── main.rs           # CLI entry point
```

### Shared Code (Extract from tpuf-server)

SPFresh modules need to be usable by both server and indexer. Two options:

**Option A — Shared crate** (recommended):
```
crates/tpuf-core/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── types/mod.rs       # Document, WalEntry, etc. (move from tpuf-server)
    ├── storage/mod.rs     # ObjectStore (move from tpuf-server)
    └── index/
        └── spfresh/       # All SPFresh code (move from tpuf-server)
```

Both `tpuf-server` and `tpuf-indexer` depend on `tpuf-core`.

**Option B — Feature flag**: Keep everything in `tpuf-server`, gate indexer behind a feature. Simpler but messier.

### tpuf-server Changes
- Add `S3PostingStore` (implements `PostingStore` trait, reads from S3)
- Add index loading on startup / periodic refresh
- Add posting cache (moka)
- Query path: check for index → use two-level search if available → fallback to brute-force

### storage/mod.rs Additions
```rust
// New methods on ObjectStore:
fn index_manifest_key(ns: &str) -> String;
fn centroids_key(ns: &str) -> String;
fn posting_key(ns: &str, head_id: u32) -> String;
fn version_map_key(ns: &str) -> String;

async fn write_index_manifest(&self, ns: &str, manifest: &IndexManifest) -> Result<()>;
async fn read_index_manifest(&self, ns: &str) -> Result<Option<IndexManifest>>;
async fn write_centroids(&self, ns: &str, file: &CentroidsFile) -> Result<()>;
async fn read_centroids(&self, ns: &str) -> Result<Option<CentroidsFile>>;
async fn write_posting(&self, ns: &str, head_id: u32, posting: &PostingList) -> Result<()>;
async fn read_posting(&self, ns: &str, head_id: u32) -> Result<Option<PostingList>>;
async fn write_version_map(&self, ns: &str, file: &VersionMapFile) -> Result<()>;
async fn read_version_map(&self, ns: &str) -> Result<Option<VersionMapFile>>;
```

---

## 7. Implementation Phases

### Phase A — Extract Shared Core
- Create `tpuf-core` crate
- Move `types/`, `storage/`, `index/spfresh/` into it
- Both `tpuf-server` and `tpuf-indexer` depend on `tpuf-core`
- All 47 existing tests still pass

### Phase B — Index Serialization
- Add `Serialize`/`Deserialize` to `HeadIndex`, `VersionMap`, `PostingList` (some already have it)
- Add `IndexManifest`, `CentroidsFile`, `VersionMapFile` types
- Add S3 read/write methods for index files
- Unit tests: round-trip serialize/deserialize for all index types

### Phase C — Indexer CLI
- Create `tpuf-indexer` crate with CLI args (clap)
- Implement `--once` mode: read WAL → build index → write to S3
- Test: ingest 1K vectors via tpuf-server → run indexer → verify index files in MinIO

### Phase D — Query Server Integration
- Add `S3PostingStore` implementing `PostingStore` trait (reads from S3, caches in moka)
- Add index loading on startup (manifest → centroids → version_map)
- Wire two-level search into `NamespaceManager::query()`
- Fallback to brute-force if no index
- Test: end-to-end ingest → index → query with recall measurement

### Phase E — Incremental & Watch Mode
- Indexer `--watch` mode: poll for new WAL, re-index incrementally
- Query server periodic index refresh (check manifest every 30s)
- Stale posting GC in indexer
- Test: continuous ingest + background indexing + query recall stability

---

## 8. Query Latency Budget

```
                        Brute-force (current)    With SPFresh index
                        ─────────────────────    ──────────────────
Centroid search         N/A                      <1ms (in-memory, ~1K centroids)
Posting fetch (S3)      N/A                      5-20ms (64 postings × parallel GET)
Posting fetch (cached)  N/A                      <1ms
Posting scan            N/A                      1-5ms (64 × 256 entries × distance)
Brute-force scan        10-100ms (10K vectors)   N/A
─────────────────────────────────────────────────────────────────────
Total (cold cache)      10-100ms                 10-25ms
Total (warm cache)      10-100ms                 2-6ms
```

The big win is at scale (100K+ vectors) where brute-force becomes >1s but SPFresh stays constant.

---

## 9. Open Questions

| # | Question | Options | Recommendation |
|---|----------|---------|----------------|
| 1 | Where does SPFresh code live? | `tpuf-core` shared crate vs keep in `tpuf-server` | `tpuf-core` — cleaner, enables indexer |
| 2 | When does query server refresh? | Poll manifest vs push notification | Poll (30s interval) — simpler |
| 3 | Posting file granularity? | One file per posting vs batched files | One per posting — enables selective fetch |
| 4 | Handle WAL entries beyond index? | Brute-force supplement vs ignore | Brute-force fallback for now |
| 5 | Indexer scheduling? | CLI cron vs built-in daemon vs both | Both (`--once` + `--watch`) |
| 6 | Index versioning? | Overwrite in place vs versioned directories | Overwrite + manifest-last protocol |

---

## 10. Success Criteria

1. `tpuf-indexer --once --namespace test` builds index from WAL and writes to S3
2. `tpuf-server` loads index on startup, uses two-level search
3. Recall@10 ≥ 0.85 on 10K vectors (128-dim) via the full pipeline
4. Query latency improves vs brute-force at 10K+ vectors (warm cache)
5. Incremental: add 1K vectors via WAL → re-index → recall stable
6. No data loss: every vector in WAL is findable (index or brute-force fallback)
