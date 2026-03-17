# SPFresh: Wiring the Index + S3 Centroid Storage

**Date:** 2026-02-26
**Priority:** Wire SPFresh into query path, persist centroids to S3
**Depends on:** spfresh-integration-plan.md, spfresh-s3-redesign.md

---

## 1. Current State: What Exists vs What's Missing

### What Works (implemented + tested)

| Component | Location | Status |
|-----------|----------|--------|
| SPFreshIndex (insert/delete/search) | `engine/index/spfresh/mod.rs` | 7 integration tests, ~90% recall@10 |
| HeadIndex (centroid storage) | `spfresh/head_index.rs` | In-memory flat Vec<f32>, brute-force centroid search |
| MemoryPostingStore | `spfresh/posting_store.rs` | HashMap<u32, PostingList>, trait-based |
| VersionMap | `spfresh/version_map.rs` | Atomic u8 per vector, CAS increment |
| Binary k-means splitting | `spfresh/rebuilder.rs` | Split + NPA reassign + recursive |
| RNG filter for replica placement | `spfresh/updater.rs` | Diversity-aware placement |
| WAL persistence to S3 | `storage/mod.rs` | bincode + zstd encoding |
| Brute-force KNN | `engine/search.rs` | Used by query path today |

### What's Missing (the gaps)

| Gap | Impact | Severity |
|-----|--------|----------|
| **SPFreshIndex not wired into query path** | Queries use brute-force, index is dead code | Critical |
| **No index in NamespaceState** | `namespace.rs` holds only `documents: HashMap` | Critical |
| **No centroid persistence** | HeadIndex rebuilt from scratch on restart | Critical |
| **No posting persistence** | PostingStore is memory-only | Critical |
| **No version map persistence** | VersionMap lost on restart | Critical |
| **Inserts don't update the index** | `flush_batcher` writes WAL but doesn't call `index.insert()` | Critical |
| **No VectorIndex trait in NamespaceState** | Trait exists but nothing uses it | High |
| **HeadIndex search is O(n) linear scan** | No tree/graph structure for centroids | Medium (ok for <10K centroids) |
| **No Serialize/Deserialize on HeadIndex** | Can't persist to S3 | High |

---

## 2. Correctness Verification Strategy

### 2.1 What We Can Already Verify

The existing tests cover component-level correctness:

```
test_recall_at_10           → 5000 vectors, recall > 0.90 vs brute-force
test_split_triggers         → splits happen, >85% findability after
test_incremental_stability  → recall degrades <15% over 6 batches
test_replica_placement_*    → RNG filter places correctly
test_concurrent_*           → multi-threaded insert+search doesn't crash
test_reassign_fixes_npa     → boundary vectors move after split
```

### 2.2 What's Missing for Correctness

**A) Invariant tests (should run after every operation)**

```
INV-1: Coverage
  Every non-deleted vector appears in >= 1 posting list with current version.
  Scan all postings, collect {vid: max_version}, compare with version_map.
  Failure means data loss.

INV-2: Version consistency
  No posting entry has version > version_map.get_version(vid).
  Failure means a "future" entry exists (corrupted state).

INV-3: Posting size bound
  After all splits resolve, no posting list exceeds max_posting_size.
  (Temporarily may exceed during insert, but process_pending_splits should fix it.)

INV-4: Centroid-posting alignment
  Every active centroid ID in HeadIndex has a corresponding posting list.
  Every posting list key maps to an active centroid.
  Failure means orphaned postings or missing clusters.

INV-5: Deletion completeness
  After delete(vid), search should never return vid.
  (Already tested, but should be an invariant check, not just one test.)
```

**B) End-to-end recall tests through the full stack**

Currently `test_recall_at_10` calls `SPFreshIndex` directly. Need:

```
E2E-1: Recall through NamespaceManager
  Create namespace → upsert 5000 docs → query via NamespaceManager → recall > 0.85
  This tests the full insert→WAL→index→search path.

E2E-2: Recall after restart (persistence roundtrip)
  Insert 5000 vectors → persist index to S3 → drop everything → reload from S3 → same recall.

E2E-3: Recall after mixed operations
  Insert 5000 → delete 1000 → insert 2000 different → query → recall > 0.80 on remaining.

E2E-4: Recall at different scales
  Test with n=100, 1K, 10K, 50K. Recall should stay > 0.80 at all scales.
```

**C) Brute-force shadow verification**

For development/testing, run every query through both paths and compare:

```rust
// In query(), behind a config flag:
let ann_results = state.index.search(&vector, top_k)?;
let bf_results = brute_force_knn(&vector, &vectors, metric, top_k);
let recall = compute_recall(&ann_results, &bf_results, top_k);
if recall < 0.7 {
    tracing::warn!(recall, "low recall detected");
}
```

This catches regressions during development without requiring separate test runs.

### 2.3 How to Check: Practical Commands

```bash
# Run all SPFresh unit tests
cargo test -p tpuf-server -- spfresh

# Run integration tests (recall, splits, concurrent)
cargo test -p tpuf-server -- integration_tests

# Run with output to see recall numbers
cargo test -p tpuf-server -- test_recall_at_10 --nocapture
```

After wiring is complete, add:
```bash
# End-to-end through API (needs server + MinIO running)
cargo test -p tpuf-server -- test_e2e_index_query --nocapture
```

---

## 3. Three-Phase Centroid-Based ANN Search

This is the target search architecture. It works by downloading progressively more specific data from S3.

### The Three Phases

```
┌──────────────────────────────────────────────────────────────────────┐
│ Phase 1: CENTROID SEARCH (in-memory, microseconds)                   │
│                                                                       │
│  HeadIndex lives in memory (loaded from S3 on startup).              │
│  Size: K centroids × D dims × 4 bytes.                              │
│  Example: 1000 centroids × 128 dims = 512 KB.                       │
│                                                                       │
│  query_vector → search HeadIndex → top-N centroid IDs                │
│  (N = config.num_search_heads, typically 32-64)                      │
│                                                                       │
│  Cost: O(K × D) distance computations. For K=1000, D=128: ~128K ops │
│  Latency: <1ms                                                       │
└──────────────────────────┬───────────────────────────────────────────┘
                           │ top-N centroid IDs
                           ▼
┌──────────────────────────────────────────────────────────────────────┐
│ Phase 2: CLUSTER SELECTION + FETCH (S3 parallel GET, milliseconds)   │
│                                                                       │
│  For each of the N candidate centroid IDs:                           │
│    1. Check LRU cache for posting list                               │
│    2. If cache miss: GET {ns}/index/postings/{head_id}.bin from S3   │
│    3. All cache misses fire in parallel (tokio join_all)             │
│                                                                       │
│  Optional Phase 2 optimization — centroid metadata:                  │
│    Store per-centroid stats: { count, radius, min_dist_to_query }    │
│    Skip centroids where min possible distance > current k-th best    │
│    (Requires storing bounding info — future optimization)            │
│                                                                       │
│  Cost: 0 to N S3 GETs (depends on cache hit rate)                   │
│  Latency: 0ms (all cached) to 20ms (all cold, parallel)             │
└──────────────────────────┬───────────────────────────────────────────┘
                           │ N PostingLists (each: Vec<PostingEntry>)
                           ▼
┌──────────────────────────────────────────────────────────────────────┐
│ Phase 3: POSTING SEARCH (in-memory, milliseconds)                    │
│                                                                       │
│  For each entry in each fetched posting list:                        │
│    1. Check version_map: skip if deleted or stale                    │
│    2. Compute distance(query, entry.vector)                          │
│    3. Track top-k results (dedup across postings)                    │
│                                                                       │
│  This is the existing search_postings() function — unchanged.        │
│                                                                       │
│  Cost: O(N × avg_posting_size × D) distance computations            │
│  Example: 64 postings × 256 entries × 128 dims = ~2M ops            │
│  Latency: 1-5ms                                                      │
└──────────────────────────────────────────────────────────────────────┘
```

### What Lives Where

```
IN MEMORY (always loaded):
  ├── HeadIndex (centroids)      — loaded from S3 on startup, refreshed periodically
  ├── VersionMap                 — loaded from S3 on startup, refreshed periodically
  └── LRU Cache (hot postings)   — populated on demand, evicted by size

ON S3 (persisted by indexer):
  ├── {ns}/index/manifest.bin    — points to current generation
  ├── {ns}/index/centroids.bin   — serialized HeadIndex
  ├── {ns}/index/version_map.bin — serialized VersionMap
  └── {ns}/index/postings/{hid}.bin  — one file per active centroid
```

### Why This Works

The key insight: **centroid search is a coarse filter that eliminates >95% of vectors from consideration.**

With 1000 centroids and num_search_heads=64, Phase 1 selects the 64 most promising clusters. If each cluster has ~256 vectors, Phase 3 scans 64×256 = ~16K vectors instead of the full dataset. For a 1M vector database, that's a 62x reduction.

The S3 fetch in Phase 2 is the latency bottleneck, but:
- Posting lists are small (4-256 KB each)
- All fetches fire in parallel
- Hot postings stay in LRU cache
- After warmup, cache hit rate is typically >90%

---

## 4. Implementation Plan: Wire SPFresh + S3 Centroids

### Step 1: Add SPFreshIndex to NamespaceState

**File: `engine/namespace.rs`**

```rust
// Current
struct NamespaceState {
    metadata: NamespaceMetadata,
    documents: HashMap<u64, Document>,
}

// Target
struct NamespaceState {
    metadata: NamespaceMetadata,
    documents: HashMap<u64, Document>,
    index: Option<SPFreshIndex>,  // None until first vector is inserted
}
```

SPFreshIndex requires knowing dimensions upfront (via config). Dimensions are set on first vector insert. So `index` starts as `None` and is lazily created when dimensions become known.

### Step 2: Feed inserts/deletes into the index

**File: `engine/namespace.rs`, `flush_batcher()`**

After `apply_wal_entry(&mut state.documents, &entry)`, replay the same ops into the index:

```rust
for op in &entry.operations {
    match op {
        WriteOp::Upsert(docs) => {
            let index = state.index.get_or_insert_with(|| {
                let config = SPFreshConfig {
                    dimensions: state.metadata.dimensions.unwrap(),
                    ..Default::default()
                };
                SPFreshIndex::new(config)
            });
            for doc in docs {
                if let Some(vec) = &doc.vector {
                    index.insert(doc.id, vec)?;
                }
            }
        }
        WriteOp::Delete(ids) => {
            if let Some(index) = &state.index {
                for &id in ids {
                    index.delete(id)?;
                }
            }
        }
    }
}
```

### Step 3: Use index in query path

**File: `engine/namespace.rs`, `query()`**

Replace brute-force with index search when available:

```rust
let results = if let Some(index) = &state.index {
    if index.len() > 0 {
        index.search(&vector, top_k)?
    } else {
        brute_force_knn(&vector, &vectors, metric, top_k)
    }
} else {
    brute_force_knn(&vector, &vectors, metric, top_k)
};
```

### Step 4: Add Serialize/Deserialize to index types

**HeadIndex** — needs `Serialize`/`Deserialize`:

```rust
#[derive(Serialize, Deserialize)]
pub struct CentroidsFile {
    pub dims: usize,
    pub next_id: u32,
    pub centroids: Vec<f32>,
    pub active: Vec<bool>,
}
```

Add `to_file()` / `from_file()` on HeadIndex:

```rust
impl HeadIndex {
    pub fn to_file(&self) -> CentroidsFile { ... }
    pub fn from_file(file: CentroidsFile) -> Self { ... }
}
```

**VersionMap** — needs serialization:

```rust
#[derive(Serialize, Deserialize)]
pub struct VersionMapFile {
    pub capacity: u64,
    pub data: Vec<u8>,  // raw atomic bytes read non-atomically for snapshot
}
```

**PostingList** — already has `Serialize`/`Deserialize` via bincode roundtrip tests.

### Step 5: Add S3 index storage methods

**File: `storage/mod.rs`**

```rust
impl ObjectStore {
    // Key helpers
    fn index_manifest_key(ns: &str) -> String { format!("{ns}/index/manifest.bin") }
    fn centroids_key(ns: &str) -> String { format!("{ns}/index/centroids.bin") }
    fn version_map_key(ns: &str) -> String { format!("{ns}/index/version_map.bin") }
    fn posting_key(ns: &str, head_id: u32) -> String {
        format!("{ns}/index/postings/{head_id:010}.bin")
    }

    // Write/read for each type
    pub async fn write_index_manifest(&self, ns: &str, m: &IndexManifest) -> Result<()>;
    pub async fn read_index_manifest(&self, ns: &str) -> Result<Option<IndexManifest>>;

    pub async fn write_centroids(&self, ns: &str, f: &CentroidsFile) -> Result<()>;
    pub async fn read_centroids(&self, ns: &str) -> Result<Option<CentroidsFile>>;

    pub async fn write_posting(&self, ns: &str, hid: u32, p: &PostingList) -> Result<()>;
    pub async fn read_posting(&self, ns: &str, hid: u32) -> Result<Option<PostingList>>;

    pub async fn write_version_map(&self, ns: &str, f: &VersionMapFile) -> Result<()>;
    pub async fn read_version_map(&self, ns: &str) -> Result<Option<VersionMapFile>>;
}
```

All use the existing `encode()`/`decode()` (bincode + zstd).

### Step 6: Index persistence (write)

Add an `IndexManifest` type:

```rust
#[derive(Serialize, Deserialize)]
pub struct IndexManifest {
    pub version: u32,           // format version = 1
    pub wal_sequence: u64,      // WAL coverage
    pub config: SPFreshConfig,  // index config used
    pub active_centroids: u32,
    pub num_vectors: u64,
    pub created_at: u64,
}
```

Persist the index after WAL flush (or on a separate timer):

```rust
async fn persist_index(&self, ns: &str, state: &NamespaceState) -> Result<()> {
    let index = match &state.index { Some(i) => i, None => return Ok(()) };

    // 1. Write all posting lists
    let hi = index.head_index.read().unwrap();
    for hid in 0..hi.next_id() {
        if let Some(posting) = index.posting_store.get(hid) {
            self.store.write_posting(ns, hid, &posting).await?;
        }
    }

    // 2. Write centroids
    self.store.write_centroids(ns, &hi.to_file()).await?;

    // 3. Write version map
    let vm = index.version_map.read().unwrap();
    self.store.write_version_map(ns, &vm.to_file()).await?;

    // 4. Write manifest LAST (commit point)
    let manifest = IndexManifest {
        version: 1,
        wal_sequence: state.metadata.wal_sequence,
        config: index.config.clone(),
        active_centroids: hi.len() as u32,
        num_vectors: index.len() as u64,
        created_at: now_ms(),
    };
    self.store.write_index_manifest(ns, &manifest).await?;

    Ok(())
}
```

**Manifest-last protocol**: If the process crashes between writing postings and writing the manifest, the next startup ignores the partial write (old manifest still points to old state, or no manifest means rebuild from WAL).

### Step 7: Index loading on startup

Modify `load_namespace()` to load pre-built index if manifest exists:

```rust
async fn load_namespace(&self, name: &str) -> Result<()> {
    // ... existing WAL replay to build documents HashMap ...

    // Try to load pre-built index from S3
    let index = if let Some(manifest) = self.store.read_index_manifest(name).await? {
        let centroids_file = self.store.read_centroids(name).await?;
        let vmap_file = self.store.read_version_map(name).await?;

        if let (Some(cf), Some(vf)) = (centroids_file, vmap_file) {
            let hi = HeadIndex::from_file(cf);
            let vm = VersionMap::from_file(vf);
            let posting_store = MemoryPostingStore::new();

            // Load posting lists for all active centroids
            for hid in 0..hi.next_id() {
                if hi.is_active(hid) {
                    if let Some(pl) = self.store.read_posting(name, hid).await? {
                        posting_store.put(hid, pl);
                    }
                }
            }

            // Replay any WAL entries newer than manifest
            let new_seqs: Vec<u64> = sequences.iter()
                .filter(|&&s| s > manifest.wal_sequence)
                .copied()
                .collect();

            let mut index = SPFreshIndex::from_parts(
                manifest.config, hi, posting_store, vm
            );
            for seq in &new_seqs {
                if let Some(entry) = self.store.read_wal(name, *seq).await? {
                    replay_into_index(&mut index, &entry);
                }
            }

            Some(index)
        } else {
            // Manifest exists but files missing — rebuild from WAL
            build_index_from_documents(&documents)
        }
    } else {
        // No manifest — build from documents
        build_index_from_documents(&documents)
    };

    let state = NamespaceState { metadata, documents, index };
    // ... store state ...
}
```

---

## 5. S3 File Layout

```
{ns}/
  meta.bin                              # NamespaceMetadata (unchanged)
  wal/
    00000000000000001.wal               # WAL entries (unchanged)
    00000000000000002.wal
  index/
    manifest.bin                        # IndexManifest — commit point
    centroids.bin                       # HeadIndex serialized
    version_map.bin                     # VersionMap snapshot
    postings/
      0000000000.bin                    # PostingList for centroid 0
      0000000001.bin                    # PostingList for centroid 1
      0000000005.bin                    # PostingList for centroid 5 (gaps ok, inactive centroids have no file)
```

File sizes at 10K vectors, 128 dims, ~40 centroids:

| File | Size Estimate |
|------|---------------|
| manifest.bin | ~200 bytes |
| centroids.bin | 40 × 128 × 4 = ~20 KB |
| version_map.bin | 10K × 1 = ~10 KB |
| Each posting file | 256 × (8 + 1 + 512) bytes = ~130 KB |
| All posting files | 40 × 130 KB = ~5 MB |
| **Total index** | **~5 MB** |

At 1M vectors, ~4000 centroids: total index ~500 MB (dominated by posting lists storing full vectors).

---

## 6. Changes Required Per File

| File | Changes |
|------|---------|
| `engine/namespace.rs` | Add `index: Option<SPFreshIndex>` to NamespaceState. Update `flush_batcher` to feed ops into index. Update `query` to use index. Add `persist_index`, `load_index`. Update `load_namespace` to load index from S3. |
| `engine/index/spfresh/mod.rs` | Add `from_parts()` constructor. Expose `head_index`, `posting_store`, `version_map` for serialization (or add snapshot methods). |
| `engine/index/spfresh/head_index.rs` | Add `to_file()`, `from_file()`, `next_id()`, `is_active()` methods. Add `CentroidsFile` struct with serde derives. |
| `engine/index/spfresh/version_map.rs` | Add `to_file()`, `from_file()` methods. Add `VersionMapFile` struct with serde derives. |
| `engine/index/spfresh/config.rs` | Add `Serialize`/`Deserialize` derives to `SPFreshConfig`. |
| `storage/mod.rs` | Add `IndexManifest` type. Add 8 new methods (write/read for manifest, centroids, postings, version_map). |
| `types/mod.rs` | Add `IndexManifest` (or put it in storage). |

---

## 7. Testing Plan

### Phase 1: Wiring (no S3, in-memory only)

```
T1: test_index_built_on_upsert
    Create namespace → upsert 100 docs → verify index.len() == 100

T2: test_query_uses_index
    Create namespace → upsert 1000 docs → query → verify results come from index
    (check that results match brute-force within recall tolerance)

T3: test_delete_removes_from_index
    Upsert 100 → delete 50 → query → deleted vectors never returned

T4: test_recall_through_namespace_manager
    Upsert 5000 docs (128-dim) → 50 queries → recall@10 > 0.85

T5: test_fallback_to_brute_force
    Create namespace with no vectors → query returns empty
    Create namespace, upsert 1 vector → query returns it (brute-force path)
```

### Phase 2: Persistence roundtrip

```
T6: test_centroids_roundtrip
    Create HeadIndex → to_file() → encode → decode → from_file() → search returns same results

T7: test_version_map_roundtrip
    Create VersionMap → initialize + mark_deleted → to_file → from_file → same state

T8: test_posting_roundtrip
    Create PostingList → encode → decode → entries identical

T9: test_manifest_roundtrip
    Create IndexManifest → encode → decode → fields identical
```

### Phase 3: S3 integration

```
T10: test_persist_and_load_index
     Upsert 1000 docs → persist_index to S3 → drop all state → load_index from S3 → recall > 0.85

T11: test_incremental_wal_replay
     Upsert 1000 → persist_index (wal_seq=5) → upsert 500 more (wal_seq=8)
     → load_index → should load manifest + replay WAL entries 6,7,8 → total 1500 vectors

T12: test_manifest_last_atomicity
     Persist index but simulate crash before manifest write
     → load_index should fall back to full WAL rebuild
     → verify correctness (all vectors searchable)
```

### Invariant Checks (run as helper in tests)

```rust
fn check_invariants(index: &SPFreshIndex) {
    let hi = index.head_index.read().unwrap();
    let vm = index.version_map.read().unwrap();

    // INV-1: Every non-deleted vector in >= 1 posting with current version
    let all_postings = index.posting_store.all_postings();
    // ... scan and verify ...

    // INV-4: Active centroids have postings, postings reference active centroids
    for (hid, _) in &all_postings {
        assert!(hi.get_centroid(*hid).is_some(), "orphaned posting for inactive centroid {hid}");
    }
}
```

---

## 8. Implementation Order

```
Step 1: Add index to NamespaceState + feed inserts/deletes     [no S3 changes]
Step 2: Switch query path to use index (with brute-force fallback) [no S3 changes]
Step 3: Add invariant checks + recall tests through NamespaceManager
        → CHECKPOINT: verify correctness without persistence
Step 4: Add serialization to HeadIndex, VersionMap, SPFreshConfig
Step 5: Add S3 storage methods for index files
Step 6: Implement persist_index (manifest-last protocol)
Step 7: Implement load_index on startup (with WAL catch-up)
        → CHECKPOINT: verify persistence roundtrip
Step 8: Add shadow verification mode (ANN vs brute-force comparison)
```

Steps 1-3 are purely in-memory changes. Steps 4-7 add S3 persistence. Step 8 adds ongoing correctness monitoring.

---

## 9. Open Decisions

| # | Question | Options | Recommendation |
|---|----------|---------|----------------|
| 1 | When to persist index? | Every WAL flush vs periodic timer vs explicit command | **Periodic timer (30s)** — WAL flush is too frequent, explicit is too manual |
| 2 | SPFreshConfig source? | Hardcoded defaults vs per-namespace config vs API param | **Defaults for now**, make configurable later |
| 3 | What if index load fails? | Crash vs fallback to WAL rebuild | **Fallback to WAL rebuild** — brute-force works until index is ready |
| 4 | Posting cache strategy? | All in memory (current) vs LRU from S3 | **All in memory for now** — switch to LRU when dataset exceeds memory |
| 5 | Separate indexer binary? | tpuf-indexer crate vs inline in server | **Inline for now** — separate later when scaling requires it |
