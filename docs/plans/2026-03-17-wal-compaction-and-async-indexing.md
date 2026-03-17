---
title: "Hybrid Query + Separate Indexer Plan"
date: 2026-03-17
depends_on:
  - docs/high-level-summaries/2026-03-17-gap-analysis.md
  - docs/2026-03-17-spfresh-s3-persistence.md
covers:
  - "B4/C4: Search unindexed WAL tail"
  - "B5: Separate indexer crate"
status: plan
---

# Hybrid Query + Separate Indexer

## Architecture After This Work

```
┌──────────┐         ┌────────────────────────────────────────┐
│  Client   │──API──▶│  tpuf-server                           │
└──────────┘         │  - writes: WAL + documents HashMap      │
                     │  - queries: ANN index + brute-force tail│
                     │  - loads index from S3 on startup       │
                     │  - NO inline indexing                   │
                     └─────────────────────┬──────────────────┘
                                           │ reads/writes S3
                                           ▼
                     ┌─────────────────────────────────────────┐
                     │  S3 (MinIO / R2)                        │
                     │  {ns}/meta.bin                          │
                     │  {ns}/wal/{seq}.wal                     │
                     │  {ns}/index/manifest.bin    ← marker    │
                     │  {ns}/index/centroids.bin               │
                     │  {ns}/index/version_map.bin             │
                     │  {ns}/index/postings/{hid}.bin          │
                     └─────────────────────────────────────────┘
                                           ▲
                                           │ reads WAL, writes index
┌──────────────────────────────────────────┘
│  tpuf-indexer (separate crate, runs per namespace)
│  - reads WAL entries from S3
│  - builds/updates SPFresh index
│  - writes centroids + postings + version_map to S3
│  - writes manifest as marker (CAS) with wal_sequence
│  - can run independently, on a different machine
└──────────────────────────────────────────────────
```

**Key insight:** The manifest's `wal_sequence` is the marker. The server knows: everything up to that sequence is indexed. Everything after needs brute-force.

No WAL compaction — WAL stays as-is. Server replays WAL on startup to rebuild documents HashMap. Index is loaded from S3 separately.

---

## The Two Problems

### Problem 1: Unindexed WAL tail is invisible to queries

Writes after the last index persist are in the documents HashMap but not in the SPFresh index. The current query path only uses `index.search()`, so these vectors are invisible.

**Fix:** Hybrid query — ANN on the index, brute-force on documents added after the manifest's `wal_sequence`, merge results.

### Problem 2: Indexing is inline and blocks writes

`flush_batcher()` calls `index.insert()` for every document. Splits can block writes for seconds.

**Fix:** Remove inline indexing from the server. A separate `tpuf-indexer` crate reads WAL from S3, builds the index, and writes it back to S3. Server just loads the index and queries it.

---

## Phase 1: Hybrid query in tpuf-server

**Goal:** Queries return results from both the indexed data AND the unindexed WAL tail.

### Changes to NamespaceState

```rust
struct NamespaceState {
    metadata: NamespaceMetadata,
    documents: HashMap<u64, Document>,
    index: Option<SPFreshIndex>,
    manifest_etag: Option<String>,
    indexed_up_to_seq: u64,   // NEW — from manifest.wal_sequence
}
```

`indexed_up_to_seq` is set from `manifest.wal_sequence` when loading the index from S3. Documents in WAL entries with `sequence > indexed_up_to_seq` are the "tail" that needs brute-force.

### Changes to documents tracking

Need to know which documents came from which WAL sequence. Add per-document sequence tracking:

```rust
struct NamespaceState {
    ...
    doc_sequences: HashMap<u64, u64>,  // doc_id → wal_sequence it was last written in
}
```

Populated during WAL replay and `flush_batcher()`. On delete, remove from `doc_sequences`.

### Changes to query()

```rust
let scored = if let Some(index) = &state.index {
    // ANN on indexed data
    let ann_results = index.search(&vector, top_k)?;

    // brute-force on unindexed tail
    let tail_vectors: Vec<(u64, &[f32])> = state.documents.iter()
        .filter(|(id, _)| {
            state.doc_sequences.get(id)
                .map_or(false, |&seq| seq > state.indexed_up_to_seq)
        })
        .filter_map(|(id, doc)| doc.vector.as_ref().map(|v| (*id, v.as_slice())))
        .collect();

    if tail_vectors.is_empty() {
        ann_results
    } else {
        let tail_results = brute_force_knn(&vector, &tail_vectors, metric, top_k);
        merge_top_k(&ann_results, &tail_results, top_k)
    }
} else {
    // no index at all — full brute-force
    brute_force_knn(...)
};
```

### Changes to flush_batcher()

Remove the inline `index.insert()` / `index.delete()` calls. Writes only go to WAL + documents HashMap + doc_sequences.

### Tests

- `test_hybrid_query_finds_unindexed` — persist index at seq=N, add more docs, query finds docs from both index and tail
- `test_hybrid_query_recall` — 1000 indexed + 200 unindexed, recall@10 > 0.85
- `test_fully_indexed_no_tail_scan` — when indexed_up_to_seq == wal_sequence, tail is empty, no brute-force
- `test_delete_in_tail` — delete a doc from the tail, query doesn't return it
- `test_no_index_full_brute_force` — empty namespace, all brute-force, still works

---

## Phase 2: tpuf-indexer crate

**Goal:** Separate binary that reads WAL from S3, builds SPFresh index, writes index files to S3.

### Crate structure

```
crates/
  tpuf-server/     # existing — query server
  tpuf-cli/        # existing — test client
  tpuf-indexer/    # NEW — indexer binary
    src/
      main.rs      # CLI: --namespace, --endpoint, --bucket, etc.
      indexer.rs   # core logic: read WAL → build index → write to S3
```

### Shared code

The indexer needs types and storage from `tpuf-server`. Two options:
1. Extract shared code into `tpuf-core` crate (types, storage, SPFresh index)
2. Have `tpuf-indexer` depend on `tpuf-server` as a library

**Choice: option 2 for now.** Extract `tpuf-core` later when it's worth the refactor. Add `[lib]` section to tpuf-server's Cargo.toml so the indexer can import it.

### Indexer logic

```
tpuf-indexer --namespace my-ns --s3-endpoint ... --s3-bucket ...

1. Read namespace metadata from S3
2. Read current manifest (if exists) → get wal_sequence marker
3. List WAL sequences > manifest.wal_sequence
4. If no new WAL entries → exit (nothing to do)
5. Load existing index from S3 (if manifest exists)
   OR create new SPFreshIndex
6. For each new WAL entry:
     replay into index (insert/delete)
7. Write updated index to S3:
     postings → centroids → version_map → manifest (CAS, manifest-last)
8. Log: "indexed {ns}: {old_seq} → {new_seq}, {n_vectors} vectors, {n_centroids} centroids"
```

The manifest's `wal_sequence` advances to the latest WAL entry processed. This is the marker that tells the server what's indexed.

### CLI interface

```
tpuf-indexer \
  --namespace my-ns \
  --s3-endpoint http://localhost:9002 \
  --s3-bucket turbopuffer \
  --s3-access-key minioadmin \
  --s3-secret-key minioadmin \
  --s3-region us-east-1
```

Or with R2:
```
tpuf-indexer \
  --namespace my-ns \
  --r2-account-id cd0ba9... \
  --r2-bucket test \
  --r2-access-key ... \
  --r2-secret-key ...
```

Can be run as a cron job, k8s Job, or in a loop with sleep.

### Tests

- `test_indexer_builds_from_scratch` — write WAL entries to S3, run indexer, verify manifest + centroids + postings exist
- `test_indexer_incremental` — run indexer at seq 5, add more WAL, run again, manifest advances to new seq
- `test_indexer_noop_when_caught_up` — run indexer when no new WAL, exits cleanly
- `test_server_loads_indexer_output` — indexer writes index, server loads it, queries work with correct recall

---

## Phase 3: Remove inline indexing from server

**Goal:** Server no longer builds or updates the index. It only loads from S3 and queries.

### Changes

- `flush_batcher()`: remove all `index.insert()` / `index.delete()` calls
- `load_namespace()`: load index from S3 manifest (already works), set `indexed_up_to_seq`
- `build_index_from_documents()`: remove — server doesn't build indexes anymore
- If no manifest exists on startup, server runs with `index: None`, all queries are brute-force until indexer runs

### Optional: index reload

Server could periodically check if the manifest has been updated (new etag) and hot-reload the index. Not required for Phase 3 but useful:

```
every 60s:
  for each namespace:
    stat manifest.bin → get etag
    if etag != state.manifest_etag:
      reload index from S3
      update indexed_up_to_seq
```

### Tests

- `test_server_without_index` — start server with no manifest, queries work via brute-force
- `test_server_after_indexer` — indexer writes index, server reloads, queries use ANN + tail

---

## Ordering

```
Phase 1 (hybrid query)
   ↓
Phase 2 (tpuf-indexer crate)     ← independent of Phase 1, but Phase 1 needed for server to handle the gap
   ↓
Phase 3 (remove inline indexing) ← needs Phase 1 + 2
```

Phase 1 first — server must handle unindexed tail before we can remove inline indexing.
Phase 2 can be developed in parallel with Phase 1.
Phase 3 is the cleanup — flip the switch.

---

## S3 Layout (unchanged)

```
{ns}/meta.bin
{ns}/wal/{seq:020}.wal                 ← WAL stays, grows, not compacted
{ns}/index/manifest.bin                ← marker: wal_sequence = what's indexed
{ns}/index/centroids.bin
{ns}/index/version_map.bin
{ns}/index/postings/{hid:010}.bin
```

No new files. The manifest's `wal_sequence` is the only coordination point between server and indexer.
