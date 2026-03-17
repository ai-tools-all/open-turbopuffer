---
title: "WAL Compaction + Async Indexing Plan"
date: 2026-03-17
depends_on:
  - docs/high-level-summaries/2026-03-17-gap-analysis.md
  - docs/2026-03-17-spfresh-s3-persistence.md
covers:
  - "A5: WAL compaction"
  - "B4/C4: Search unindexed WAL tail"
  - "B5: Async indexing (background, not separate binary)"
status: plan
---

# WAL Compaction + Async Indexing

## The Three Problems

### Problem 1: WAL grows unbounded
Every write appends a new `.wal` file to S3. Nothing ever removes them. After 10K writes, startup replays 10K files sequentially. Gets worse linearly.

**Current:** `load_namespace()` reads every WAL entry to rebuild documents HashMap.

**Fix:** After persisting the index, compact all WAL entries up to the manifest's `wal_sequence` into a single snapshot file (`{ns}/snapshot.bin`). Delete old WAL files.

### Problem 2: WAL entries after index persist are invisible to search
If vectors are inserted after the last `persist_index()`, the index doesn't know about them. The query path uses `index.search()` which only sees indexed vectors. The documents HashMap has them, but they're not searched.

**Current query path:**
```
if index exists → index.search()       ← misses unindexed vectors
else            → brute_force_knn()
```

**Fix:** Hybrid search — run ANN on the index, AND brute-force on documents newer than the index, merge results.

### Problem 3: Indexing is inline and blocks writes
`flush_batcher()` calls `index.insert()` synchronously for every document. For large batches with splits, this can block the write path for seconds.

**Fix:** Move indexing to a background task. Writes go to WAL + documents HashMap only. A background loop periodically drains unindexed documents into the SPFresh index.

---

## Design

### Snapshot file format

```rust
#[derive(Serialize, Deserialize)]
struct Snapshot {
    wal_sequence: u64,        // highest WAL seq included
    documents: Vec<Document>, // full materialized state
}
```

S3 key: `{ns}/snapshot.bin`

On startup: load snapshot first (if exists), then replay WAL entries after `snapshot.wal_sequence`.

### Hybrid query (index + WAL tail)

```
query(vector, top_k):
  1. index_results = index.search(vector, top_k)        // ANN on indexed data
  2. tail_docs = documents where id NOT in index         // unindexed recent writes
  3. tail_results = brute_force_knn(vector, tail_docs, top_k)
  4. return merge_top_k(index_results, tail_results, top_k)
```

To know which documents are "unindexed", track `index_up_to_seq: u64` — the WAL sequence up to which the index has consumed. Documents from WAL entries > `index_up_to_seq` need brute-force.

### Background indexing loop

```
loop:
  sleep(5s)
  for each namespace:
    drain unindexed documents into index
    if enough new data:
      persist_index()
      compact_wal()
```

This decouples write latency from indexing cost.

---

## Implementation Phases

### Phase 1: WAL compaction (snapshot write + old WAL cleanup)

**Changes:**
- `storage/mod.rs`: add `write_snapshot()`, `read_snapshot()`, `delete_wal_entries(ns, up_to_seq)`
- `engine/namespace.rs`: add `compact_wal()` that writes snapshot + deletes old WALs
- `engine/namespace.rs`: update `load_namespace()` to load snapshot first, then replay WAL tail

**Tests:**
- `test_compact_wal` — upsert 500, persist index, compact, verify snapshot exists, old WALs gone
- `test_load_from_snapshot` — compact, reload namespace from new manager, verify all docs present
- `test_snapshot_plus_wal_tail` — compact at seq 5, add more writes (seq 6-8), reload, verify all data

### Phase 2: Hybrid query (search index + brute-force WAL tail)

**Changes:**
- `NamespaceState`: add `indexed_up_to_seq: u64` tracking which WAL sequence the index has consumed
- `query()`: if index exists AND there are documents newer than `indexed_up_to_seq`, combine ANN + brute-force on tail
- `flush_batcher()`: stop feeding inserts into the index (just WAL + documents)

**Tests:**
- `test_hybrid_query_finds_unindexed` — persist index at N vectors, add M more, query finds vectors from both
- `test_hybrid_query_recall` — 1000 indexed + 200 unindexed, recall@10 > 0.85 vs all-brute-force ground truth
- `test_no_tail_skips_brute_force` — when `indexed_up_to_seq == wal_sequence`, no brute-force scan needed

### Phase 3: Background indexing loop

**Changes:**
- `engine/namespace.rs`: add `drain_unindexed()` — feeds unindexed docs into the SPFresh index, updates `indexed_up_to_seq`
- `engine/namespace.rs` or new `engine/background.rs`: spawn a tokio task that periodically calls `drain_unindexed()` → `persist_index()` → `compact_wal()`
- Remove inline `index.insert()` from `flush_batcher()`

**Tests:**
- `test_background_indexing` — upsert docs, wait for background loop, verify `indexed_up_to_seq` advances
- `test_writes_not_blocked_by_indexing` — concurrent writes + indexing don't deadlock

### Phase 4: Periodic persist + compact cycle

**Changes:**
- Add config for persist interval (default 30s) and compaction threshold
- After `persist_index()` succeeds, call `compact_wal()` to clean old entries
- Log metrics: index lag (wal_sequence - indexed_up_to_seq), WAL file count

**Tests:**
- `test_persist_compact_cycle` — simulate multiple persist+compact rounds, verify WAL stays bounded

---

## Ordering & Dependencies

```
Phase 1 (WAL compaction)
   ↓
Phase 2 (hybrid query)     ← can work independently of Phase 1
   ↓
Phase 3 (background indexing) ← needs Phase 2 (queries must handle unindexed tail)
   ↓
Phase 4 (periodic cycle)     ← needs Phase 1 + 3
```

Phase 1 and Phase 2 are independent and can be done in either order.
Phase 3 requires Phase 2 (once we stop inline indexing, queries must handle unindexed data).
Phase 4 ties it all together.

---

## State Changes Summary

### NamespaceState after all phases

```rust
struct NamespaceState {
    metadata: NamespaceMetadata,
    documents: HashMap<u64, Document>,
    index: Option<SPFreshIndex>,
    manifest_etag: Option<String>,
    indexed_up_to_seq: u64,       // NEW — WAL seq the index has consumed up to
    snapshot_seq: u64,             // NEW — WAL seq the snapshot covers
}
```

### S3 layout after all phases

```
{ns}/meta.bin
{ns}/snapshot.bin                      ← NEW: compacted document state
{ns}/wal/{seq}.wal                     ← only entries AFTER snapshot_seq
{ns}/index/manifest.bin
{ns}/index/centroids.bin
{ns}/index/version_map.bin
{ns}/index/postings/{hid}.bin
```

### Write path after all phases

```
upsert(docs):
  1. batch (500ms window)
  2. write WAL to S3
  3. apply to documents HashMap
  4. return success                    ← index is NOT updated here
```

### Background loop after all phases

```
every 30s:
  1. drain_unindexed() — feed new docs into SPFresh index
  2. persist_index() — write index to S3
  3. compact_wal() — snapshot + delete old WALs
```

### Query path after all phases

```
query(vector, top_k):
  1. ANN search on index (if exists)
  2. brute-force on documents with seq > indexed_up_to_seq
  3. merge top_k from both
```
