# SPFresh Index — Implementation Plan

**Status**: DRAFT
**Date**: 2026-02-12
**Crate**: `tpuf-server` (new module: `engine/index/`)
**References**: [SPFresh paper (SOSP'23)](2410.14452v1.pdf) | [C++ ref impl](https://github.com/Yuming-Xu/SPFresh)

---

## 1. What is SPFresh

SPFresh is a cluster-based vector index that supports **incremental in-place updates** without expensive global rebuilds. At its core is the **LIRE** protocol (Lightweight Incremental RE-balancing).

### Architecture: Two-Level Index

```
┌─────────────────────────────────┐
│  Head Index (in-memory)         │  ← navigable graph over cluster centroids
│  BKT/SPTAG-like graph          │    used to route queries to candidate clusters
└──────────┬──────────────────────┘
           │ top-k head search
           ▼
┌─────────────────────────────────┐
│  Posting Lists (on-disk/S3)     │  ← each head owns a list of vectors
│  PostingID → Vec<(VID, Vec)>    │    full scan within candidate postings
└─────────────────────────────────┘
```

**Query flow**: search head graph → find top-K nearest centroids → load their posting lists → brute-force scan within postings → return top-k results.

**Insert flow**: search head graph → find nearest centroids (with RNG diversity) → append vector to those posting lists → if posting exceeds size limit → trigger async split.

### LIRE Protocol — The Key Innovation

LIRE maintains index quality through three internal operations:

1. **Split**: When a posting list exceeds `max_posting_size`, binary k-means splits it into two. Both new centroids are inserted into the head graph.

2. **Merge**: When a posting list falls below `min_posting_size`, merge it into the nearest posting. Delete the old centroid.

3. **Reassign**: After split/merge, vectors in **neighboring** postings may now violate NPA (Nearest Partition Assignment). LIRE checks two necessary conditions:
   - **Condition 1** (vectors in split posting): `D(v, A_old) ≤ D(v, A_new_i)` for all new centroids → candidate for reassignment
   - **Condition 2** (vectors in neighbor postings): `D(v, A_new_i) ≤ D(v, A_old)` for some new centroid → candidate for reassignment

   These conditions are *necessary* but not *sufficient* — a final NPA check confirms before moving.

### Version Map — Optimistic Concurrency

Each vector has a 1-byte version: 7 bits for version number + 1 bit for deletion.
- Insert: version = 0
- Reassign: increment version, write new copy with new version to target posting
- Old copies become garbage (stale version) — filtered during search and cleaned during splits
- No locks needed for reads — version check is the consistency mechanism

### Convergence Guarantee

The paper proves split-reassign always converges:
- Each split adds exactly 1 new centroid → `|C|` monotonically increases
- `|C| ≤ |V|` (bounded by dataset size)
- Therefore finite number of splits

---

## 2. Our Rust Design

### 2.1 Module Layout

```
crates/tpuf-server/src/engine/index/
├── mod.rs              # VectorIndex trait + factory
├── spfresh/
│   ├── mod.rs          # SPFreshIndex struct — main orchestrator
│   ├── head_index.rs   # In-memory navigable graph over centroids
│   ├── posting.rs      # PostingList data structure + serialization
│   ├── posting_store.rs# PostingStore trait + impls (memory, disk, S3)
│   ├── version_map.rs  # Atomic version tracking per vector
│   ├── updater.rs      # Foreground insert/delete path
│   ├── rebuilder.rs    # Background split/merge/reassign (LIRE)
│   ├── search.rs       # Two-level search: head→posting→results
│   ├── kmeans.rs       # Binary k-means for splitting
│   └── config.rs       # SPFreshConfig parameters
```

### 2.2 Core Traits and Types

```rust
/// Abstract vector index — allows swapping brute-force for SPFresh
pub trait VectorIndex: Send + Sync {
    fn insert(&self, id: u64, vector: &[f32]) -> Result<()>;
    fn delete(&self, id: u64) -> Result<()>;
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>>;
    fn len(&self) -> usize;
}

/// Configuration
pub struct SPFreshConfig {
    pub max_posting_size: usize,     // trigger split (default: 256)
    pub min_posting_size: usize,     // trigger merge (default: 32)
    pub replica_count: usize,        // postings per vector (default: 4)
    pub rng_factor: f32,             // RNG diversity threshold (default: 2.0)
    pub num_search_heads: usize,     // heads to probe during search (default: 64)
    pub reassign_range: usize,       // nearby postings to check (default: 64)
    pub split_threads: usize,        // background split workers (default: 2)
    pub reassign_threads: usize,     // background reassign workers (default: 2)
    pub dimensions: usize,
    pub distance_metric: DistanceMetric,
}
```

### 2.3 Head Index (In-Memory Graph)

The C++ impl uses Microsoft's SPTAG (BKT graph). We simplify for v1:

**Option A — Flat centroid index with greedy graph search**:
- Store centroids in a `Vec<f32>` (row-major, contiguous)
- Build a navigable small-world graph (NSW) over centroids
- NSW is simpler than HNSW and sufficient for ~10K-100K centroids
- On split: add new centroids + edges; on merge: remove centroid + edges

**Option B — Simple flat scan over centroids** (for initial correctness):
- Just brute-force scan all centroids to find top-K
- Replace with NSW/HNSW later once correctness is proven
- At 100K centroids × 128 dims × 4 bytes = ~50MB — fast enough for testing

**Decision**: Start with Option B for correctness, then upgrade to NSW.

```rust
pub struct HeadIndex {
    centroids: Vec<f32>,          // contiguous [c0_d0, c0_d1, ..., c1_d0, ...]
    head_ids: Vec<u32>,           // maps centroid index → head_id
    dims: usize,
    next_head_id: AtomicU32,
    // Future: graph edges for NSW
}

impl HeadIndex {
    fn search(&self, query: &[f32], k: usize) -> Vec<(u32, f32)>; // (head_id, distance)
    fn add_centroid(&mut self, vector: &[f32]) -> u32;              // returns new head_id
    fn remove_centroid(&mut self, head_id: u32);
    fn get_centroid(&self, head_id: u32) -> &[f32];
}
```

### 2.4 Posting Store — Pluggable Storage

```rust
/// Entry in a posting list
#[derive(Clone, Serialize, Deserialize)]
pub struct PostingEntry {
    pub vector_id: u64,
    pub version: u8,
    pub vector: Vec<f32>,
}

/// A posting list = collection of entries for one head
pub struct PostingList {
    pub entries: Vec<PostingEntry>,
}

/// Abstract posting storage
#[async_trait]
pub trait PostingStore: Send + Sync {
    async fn get(&self, head_id: u32) -> Result<PostingList>;
    async fn put(&self, head_id: u32, posting: &PostingList) -> Result<()>;
    async fn append(&self, head_id: u32, entries: &[PostingEntry]) -> Result<()>;
    async fn delete(&self, head_id: u32) -> Result<()>;
    async fn get_multi(&self, head_ids: &[u32]) -> Result<Vec<(u32, PostingList)>>;
}
```

**Implementations**:
1. `MemoryPostingStore` — `HashMap<u32, PostingList>` behind `RwLock` (for testing)
2. `DiskPostingStore` — directory of files, one per posting (for local dev)
3. `S3PostingStore` — OpenDAL-backed (for production, integrates with existing storage layer)

### 2.5 Version Map

```rust
pub struct VersionMap {
    /// version_id → (7-bit version | 1-bit deleted)
    versions: Vec<AtomicU8>,
    capacity: usize,
}

impl VersionMap {
    fn get_version(&self, vid: u64) -> u8;        // lower 7 bits
    fn is_deleted(&self, vid: u64) -> bool;        // top bit
    fn increment_version(&self, vid: u64) -> u8;   // CAS loop, returns new version
    fn mark_deleted(&self, vid: u64);
    fn allocate(&self, vid: u64);                   // ensure capacity
}
```

### 2.6 Insert Path (Foreground Updater)

```
insert(vid, vector):
  1. version_map.allocate(vid)       // version=0
  2. heads = head_index.search(vector, replica_count * 2)
  3. selected = rng_filter(heads, rng_factor)  // select up to replica_count diverse heads
  4. for head_id in selected:
       posting_store.append(head_id, PostingEntry{vid, version=0, vector})
  5. if any posting exceeds max_posting_size:
       rebuilder.queue_split(head_id)
```

**RNG (Relative Neighborhood Graph) filter**:
```
rng_filter(candidates, factor):
  selected = [candidates[0]]  // always include nearest
  for c in candidates[1..]:
    keep = true
    for s in selected:
      if distance(c, s) < factor * distance(c, query):
        keep = false; break
    if keep: selected.push(c)
    if selected.len() == replica_count: break
  return selected
```

### 2.7 Search Path

```
search(query, k):
  1. head_results = head_index.search(query, num_search_heads)
  2. postings = posting_store.get_multi(head_results.head_ids)  // parallel fetch
  3. candidates = []
  4. for posting in postings:
       for entry in posting.entries:
         if version_map.is_deleted(entry.vid): continue
         if version_map.get_version(entry.vid) != entry.version: continue  // stale
         dist = distance(query, entry.vector)
         candidates.push((entry.vid, dist))
  5. candidates.sort_by(dist)
  6. deduplicate by vid (keep lowest distance)
  7. return candidates[..k]
```

### 2.8 Background Rebuilder (LIRE)

Runs in a separate tokio task pool. Processes a job queue:

```rust
enum RebuildJob {
    Split(u32),         // head_id to split
    Merge(u32, u32),    // (small_head, target_head)
    Reassign {
        vector_id: u64,
        old_head: u32,
        vector: Vec<f32>,
    },
}
```

**Split algorithm**:
```
split(head_id):
  1. posting = posting_store.get(head_id)
  2. // GC: filter stale/deleted entries
     clean = posting.entries.filter(|e|
       !version_map.is_deleted(e.vid) &&
       version_map.get_version(e.vid) == e.version
     )
  3. if clean.len() <= max_posting_size:
       posting_store.put(head_id, clean)  // GC was enough
       return
  4. (cluster_a, cluster_b) = binary_kmeans(clean)
  5. centroid_a = mean(cluster_a.vectors)
     centroid_b = mean(cluster_b.vectors)
  6. // Decide which centroid reuses old head
     old_centroid = head_index.get_centroid(head_id)
     if distance(centroid_a, old_centroid) < EPSILON:
       head_a = head_id  // reuse
       head_b = head_index.add_centroid(centroid_b)
     elif distance(centroid_b, old_centroid) < EPSILON:
       head_b = head_id  // reuse
       head_a = head_index.add_centroid(centroid_a)
     else:
       head_a = head_index.add_centroid(centroid_a)
       head_b = head_index.add_centroid(centroid_b)
       head_index.remove_centroid(head_id)
       posting_store.delete(head_id)
  7. posting_store.put(head_a, cluster_a)
     posting_store.put(head_b, cluster_b)
  8. // Queue reassignment for neighbors
     queue_reassign_check(head_id, head_a, head_b)
```

**Reassign check** (after split):
```
queue_reassign_check(old_head, new_head_a, new_head_b):
  old_centroid = saved centroid of old_head before split
  neighbors = head_index.search(old_centroid, reassign_range)
  for neighbor_head in neighbors:
    posting = posting_store.get(neighbor_head)
    for entry in posting:
      // Condition 2: is a new centroid closer than old?
      d_old = distance(entry.vector, old_centroid)
      d_a = distance(entry.vector, centroid_a)
      d_b = distance(entry.vector, centroid_b)
      if d_a <= d_old || d_b <= d_old:
        queue_reassign(entry.vid, entry.vector, neighbor_head)
  // Also check vectors within the split posting itself
  // (Condition 1 — already handled by k-means assignment)
```

**Reassign execution**:
```
reassign(vid, vector):
  1. new_heads = head_index.search(vector, replica_count * 2)
  2. selected = rng_filter(new_heads, rng_factor)
  3. new_version = version_map.increment_version(vid)  // CAS — invalidates all old copies
  4. for head_id in selected:
       posting_store.append(head_id, PostingEntry{vid, version=new_version, vector})
```

### 2.9 Binary K-Means (for splitting)

```rust
pub fn binary_kmeans(
    entries: &[PostingEntry],
    dims: usize,
    max_iters: usize,     // default: 100
    tolerance: f32,        // default: 1e-6
) -> (Vec<PostingEntry>, Vec<PostingEntry>, Vec<f32>, Vec<f32>)
// returns (cluster_a, cluster_b, centroid_a, centroid_b)
```

Algorithm:
1. Initialize two centroids: random pick from entries OR kmeans++ init
2. Iterate: assign each entry to nearest centroid, recompute centroids
3. Stop when: max_iters reached OR centroid movement < tolerance
4. Return two clusters and their centroids

### 2.10 Concurrency Model

```
Foreground (insert/delete)    Background (LIRE)
  │                              │
  ├─ append to posting ──────── shared RwLock per head_id
  │                              ├─ split takes write lock
  │                              ├─ reassign takes write lock on target
  │                              └─ merge takes write locks on both
  │
  └─ search ─────────────────── lock-free (version check filters stale)
```

- Fine-grained locking: hash `head_id` to one of N mutex slots (like C++ impl's 32768 slots)
- Splits are exclusive; appends are shared on the same head
- Search is fully lock-free (read posting, check version map)

---

## 3. Testing Strategy

### 3.1 Unit Tests

**`kmeans.rs` tests**:
- Two well-separated clusters → correct split
- Identical vectors → doesn't crash, one cluster may be empty
- Single vector → degenerate case handled
- Convergence within max_iters

**`version_map.rs` tests**:
- Allocate, get, increment, delete
- Concurrent increment (multi-threaded CAS correctness)
- Overflow at 127 versions (wraps or errors gracefully)

**`head_index.rs` tests**:
- Add/remove centroids, search returns correct nearest
- Empty index search returns empty

**`posting.rs` + `posting_store.rs` tests**:
- Append, get, put, delete round-trip
- get_multi correctness
- Concurrent append to same posting

### 3.2 Integration Tests — Recall Measurement

This is the critical test. Follows the C++ `SPFreshTest` pattern:

```rust
#[test]
fn test_recall_at_k() {
    // 1. Generate dataset: N random normalized vectors, dim=128
    let n = 10_000;
    let dim = 128;
    let vectors = generate_random_vectors(n, dim);

    // 2. Build SPFresh index with all vectors
    let index = SPFreshIndex::new(config);
    for (id, vec) in vectors.iter().enumerate() {
        index.insert(id as u64, vec);
    }
    index.wait_for_background_jobs(); // let splits/reassigns finish

    // 3. Generate query set + compute ground truth via brute-force
    let queries = generate_random_vectors(100, dim);
    let k = 10;
    let ground_truth: Vec<Vec<u64>> = queries.iter().map(|q| {
        brute_force_knn(&vectors, q, k)
    }).collect();

    // 4. Search via SPFresh and compute recall
    let mut total_recall = 0.0;
    for (q, gt) in queries.iter().zip(ground_truth.iter()) {
        let results = index.search(q, k);
        let result_ids: HashSet<u64> = results.iter().map(|(id, _)| *id).collect();
        let gt_ids: HashSet<u64> = gt.iter().copied().collect();
        let recall = result_ids.intersection(&gt_ids).count() as f64 / k as f64;
        total_recall += recall;
    }
    let avg_recall = total_recall / queries.len() as f64;

    // Target: recall@10 > 0.85 for random data
    assert!(avg_recall > 0.85, "recall@10 = {avg_recall:.3}, expected > 0.85");
}
```

### 3.3 Incremental Indexing Test

The key SPFresh differentiator — recall should remain stable as data is added:

```rust
#[test]
fn test_incremental_indexing_recall_stability() {
    let dim = 128;
    let base_size = 5_000;
    let increment = 1_000;
    let num_increments = 10;
    let k = 10;

    // 1. Build base index
    let all_vectors = generate_random_vectors(base_size + increment * num_increments, dim);
    let index = SPFreshIndex::new(config);
    for i in 0..base_size {
        index.insert(i as u64, &all_vectors[i]);
    }
    index.wait_for_background_jobs();

    // 2. Measure baseline recall
    let queries = generate_random_vectors(100, dim);
    let baseline_recall = measure_recall(&index, &all_vectors[..base_size], &queries, k);
    println!("Baseline recall@{k}: {baseline_recall:.3}");

    // 3. Incrementally add vectors and measure recall after each batch
    let mut recalls = vec![baseline_recall];
    for batch in 0..num_increments {
        let start = base_size + batch * increment;
        let end = start + increment;
        for i in start..end {
            index.insert(i as u64, &all_vectors[i]);
        }
        index.wait_for_background_jobs();

        let current_recall = measure_recall(&index, &all_vectors[..end], &queries, k);
        println!("After batch {batch}: recall@{k} = {current_recall:.3} (n={end})");
        recalls.push(current_recall);
    }

    // 4. Verify recall doesn't degrade significantly
    let max_degradation = baseline_recall - recalls.iter().cloned().fold(f64::MAX, f64::min);
    assert!(max_degradation < 0.10,
        "recall degraded by {max_degradation:.3}, max allowed: 0.10");

    // 5. Verify final recall is still reasonable
    let final_recall = *recalls.last().unwrap();
    assert!(final_recall > 0.75,
        "final recall@{k} = {final_recall:.3}, expected > 0.75");
}
```

### 3.4 Split/Merge Correctness Tests

```rust
#[test]
fn test_split_preserves_all_vectors() {
    // Insert enough vectors into one cluster to trigger a split
    // Verify: all vectors are still findable after split
    // Verify: no duplicates in search results
    // Verify: posting sizes are now below max_posting_size
}

#[test]
fn test_split_produces_balanced_clusters() {
    // Insert skewed data that triggers split
    // Verify: neither cluster is empty
    // Verify: size ratio is reasonable (e.g., at least 30/70)
}

#[test]
fn test_reassign_fixes_npa_violations() {
    // Construct a scenario where split creates NPA violations
    // Verify: after reassign completes, vectors are in their nearest posting
}

#[test]
fn test_merge_on_small_posting() {
    // Delete enough vectors from a posting to make it undersized
    // Verify: merge happens and vectors move to nearest posting
    // Verify: old posting is deleted
}
```

### 3.5 Concurrency Tests

```rust
#[test]
fn test_concurrent_insert_and_search() {
    // Spawn N insert threads + M search threads
    // Insert should never panic
    // Search should never return invalid results (deleted/wrong vectors)
    // Version map should remain consistent
}

#[test]
fn test_concurrent_split_and_search() {
    // Force splits while searching
    // Search results should be valid (may miss some vectors during split, but no garbage)
}
```

### 3.6 Recall vs. Parameters Sweep

```rust
#[test]
fn test_recall_vs_num_search_heads() {
    // For num_search_heads in [1, 4, 16, 32, 64, 128]:
    //   measure recall@10
    // Verify: recall monotonically increases
    // Print: table of (num_search_heads, recall, search_time_ms)
}

#[test]
fn test_recall_vs_max_posting_size() {
    // For max_posting_size in [64, 128, 256, 512]:
    //   build index, measure recall@10
    // Document: tradeoff between posting size and recall
}
```

---

## 4. Implementation Phases

### Phase 1 — Core Data Structures (standalone, no integration)
- [ ] `config.rs` — SPFreshConfig
- [ ] `version_map.rs` — atomic version tracking
- [ ] `kmeans.rs` — binary k-means
- [ ] `posting.rs` — PostingList + PostingEntry + serialization
- [ ] `posting_store.rs` — PostingStore trait + MemoryPostingStore
- [ ] `head_index.rs` — flat brute-force centroid search
- [ ] Unit tests for all above

### Phase 2 — Index Operations
- [ ] `search.rs` — two-level search (head → posting → results)
- [ ] `updater.rs` — insert + delete path
- [ ] `mod.rs` — SPFreshIndex struct orchestrating everything
- [ ] Recall@K test (static, no incremental updates)

### Phase 3 — LIRE Protocol
- [ ] `rebuilder.rs` — background job queue + split + reassign
- [ ] RNG filter for diverse cluster assignment
- [ ] NPA violation detection (conditions 1 & 2)
- [ ] Merge support
- [ ] Incremental indexing recall stability test
- [ ] Concurrency tests

### Phase 4 — Storage Integration
- [ ] `DiskPostingStore` — file-based posting storage
- [ ] `S3PostingStore` — OpenDAL-backed, integrates with existing `storage/mod.rs`
- [ ] Wire into `engine/namespace.rs` as optional index backend
- [ ] End-to-end test through REST API

### Phase 5 — Performance
- [ ] Replace flat centroid scan with NSW graph
- [ ] SIMD distance computation
- [ ] Posting list compression (bincode + zstd, matching existing WAL format)
- [ ] Batch parallel posting fetches with tokio::join

---

## 5. Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Head index | Flat scan → NSW later | Correctness first; flat scan works for <100K heads |
| Posting storage | Trait-based pluggable | Test with memory, run with disk/S3 |
| Concurrency | Fine-grained RwLock + version map | Matches C++ impl; proven correct |
| K-means | Binary (k=2) per split | Paper design; simple and convergent |
| Serialization | bincode + zstd | Matches existing WAL format |
| Background jobs | tokio task pool | Integrates with existing async runtime |
| Distance metrics | Reuse existing `search.rs` | Already have cosine, euclidean, dot product |

---

## 6. Relationship to Existing Code

```
engine/
├── namespace.rs     # Will gain: optional SPFreshIndex per namespace
├── search.rs        # Reuse: distance_* functions extracted for index use
├── batcher.rs       # Unchanged
└── index/           # NEW: all SPFresh code lives here
    ├── mod.rs
    └── spfresh/
```

The `VectorIndex` trait allows `namespace.rs` to swap between brute-force (current) and SPFresh based on namespace config. No breaking changes to existing API or storage.

---

## 7. C++ Reference File Map

The C++ reference implementation is cloned at `docs/SPFresh-ref/`. Below maps each Rust module to its C++ counterpart with key line numbers.

### Core Implementation

| Rust Module | C++ File | Key Lines | What to Study |
|-------------|----------|-----------|---------------|
| `spfresh/mod.rs` | `AnnService/inc/Core/SPANN/Index.h` | L46-430 | `Index<T>` class: all fields, thread pools, async job classes (`SplitAsyncJob` L71, `ReassignAsyncJob` L417) |
| `spfresh/mod.rs` | `AnnService/src/Core/SPANN/SPANNIndex.cpp` | L1-1460 | Full implementation of all Index methods |
| `config.rs` | `AnnService/inc/Core/SPANN/Options.h` | entire file | All tunable parameters (`m_replicaCount`, `m_postingPageLimit`, `m_reassignK`, etc.) |
| `config.rs` | `AnnService/inc/Core/SPANN/ParameterDefinitionList.h` | entire file | X-macro parameter definitions with defaults |

### Data Structures

| Rust Module | C++ File | Key Lines | What to Study |
|-------------|----------|-----------|---------------|
| `version_map.rs` | `AnnService/inc/Core/Common/VersionLabel.h` | L14-136 | Version byte layout: `(version-1) & 0x7f \| 0x80`, CAS via `InterlockedCompareExchange` (L60,74), `IncVersion` (L66-78), `Delete` sets to 1 (L39-45), `GetVersion` returns `(*data+1) & 0x7f` (L48) |
| `head_index.rs` | `AnnService/inc/Core/BKT/` | entire dir | BKT (Balanced K-means Tree) — we simplify to flat scan initially |
| `posting.rs` | `AnnService/inc/Core/SPANN/IExtraSearcher.h` | L192+ | Posting entry format: `[VID:i32][Version:u8][Vector:T*dim]`, `AppendPosting`, `OverrideIndex`, `AddIndex`, `DeleteIndex` |
| `posting_store.rs` | `AnnService/inc/Core/SPANN/ExtraRocksDBController.h` | L206-600+ | RocksDB backend: `AnnMergeOperator` for atomic append (L206), `AppendPosting` (L600), BlobDB config, io_uring |
| (concurrency) | `AnnService/inc/Core/Common/FineGrainedLock.h` | L16-66 | `FineGrainedRWLock`: 32768 `shared_timed_mutex` slots, hash `(idx*99991 + rotl(idx,2) + 101) & 0x7FFF` |
| (posting sizes) | `AnnService/inc/Core/Common/PostingSizeRecord.h` | entire file | Tracks posting list sizes for split trigger |

### Algorithm Implementation (SPANNIndex.cpp)

| Rust Module | C++ Function | Line | What to Study |
|-------------|-------------|------|---------------|
| `search.rs` | `Index<T>::SearchIndex()` | L159 | Two-level search: head graph → posting lists → filter by version → return top-k |
| `updater.rs` | `Index<T>::AddIndex()` | L776 | Insert path: allocate VID → search heads → RNG filter → append to replica postings → trigger split if oversized |
| `updater.rs` | `Index<T>::Append()` | L1346 | Append to posting: check head deleted → shared lock → `AppendPosting()` → increment posting size → trigger split if over limit |
| `rebuilder.rs` | `Index<T>::Split()` | L952 | Split: exclusive lock → read posting → GC stale → if still over limit → k-means(k=2) → assign clusters → create/reuse heads → write postings → trigger reassign |
| `kmeans.rs` | `COMMON::KmeansClustering()` | L1031 (call site) | Binary k-means: 1000 max iters, tolerance 100.0F, `m_virtualHead` option for centroid-as-data vs centroid-as-mean |
| `rebuilder.rs` | `Index<T>::ReAssign()` | L1156 | Reassign check: find nearby heads → multi-get postings → check NPA conditions 1&2 → queue individual reassignments |
| `rebuilder.rs` | `Index<T>::ReAssignVectors()` | L1254 | Batch reassignment: iterate reassign candidates → call `ReAssignUpdate()` per vector |
| `rebuilder.rs` | `Index<T>::ReAssignUpdate()` | L1266 | Single vector reassign: search heads → RNG filter → if old head still best, skip → CAS version increment → append to new heads |

### Async Job Dispatch (Index.h)

| Job Type | C++ Code | Line | Pattern |
|----------|----------|------|---------|
| Split | `SplitAsync()` | Index.h L411 | `m_splitThreadPool->add(new SplitAsyncJob(...))` |
| Reassign | `ReassignAsync()` | Index.h L417 | `m_reassignThreadPool->add(new ReassignAsyncJob(...))` |

### Test Files

| Test | C++ File | Key Lines | What to Study |
|------|----------|-----------|---------------|
| Main test | `Test/src/SPFreshTest.cpp` | L639-770 | `UpdateSPFresh()`: build base index → iterate insert batches → measure recall after each batch → print stats |
| Recall calc | `Test/src/SPFreshTest.cpp` | L170 | `CalculateRecallSPFresh()`: match VIDs between results & truth with distance epsilon tolerance |
| Insert bench | `Test/src/SPFreshTest.cpp` | L572 | `InsertVectors()`: multi-threaded, atomic counter for VID allocation, calls `AddIndex()` per vector |
| Search bench | `Test/src/SPFreshTest.cpp` | L249 | `SearchSequential()`: multi-threaded search, measures QPS, latency percentiles (P50/90/95/99/99.9) |
| Stable search | `Test/src/SPFreshTest.cpp` | L520 | `StableSearch()`: repeated search rounds to measure recall stability during/after updates |
| Entry point | `Test/src/SPFreshTest.cpp` | L823 | `UpdateTest()`: loads config → creates index → builds → calls `UpdateSPFresh()` |
| Concurrency | `Test/src/ConcurrentTest.cpp` | entire file | Concurrent read/write correctness |
| Config | `ini/test.ini` | entire file | Sample configuration with all parameter defaults |

### Key Patterns to Preserve in Rust

1. **Version byte encoding** (VersionLabel.h L48-74): stored as `(version-1) & 0x7f | 0x80`, read as `(*data+1) & 0x7f`. Delete sets byte to `1`. The high bit `0x80` distinguishes versioned (active) from deleted.

2. **Fine-grained lock hash** (FineGrainedLock.h L36-39): `(idx * 99991 + rotl(idx, 2) + 101) & 0x7FFF` maps to 32768 mutex slots. Use `RwLock` variant — shared for appends, exclusive for splits.

3. **RNG diversity filter** (SPANNIndex.cpp within AddIndex): ensures each vector's replica heads are spatially diverse, not just the K-nearest. Critical for recall.

4. **GC-before-split** (SPANNIndex.cpp L952+): always garbage-collect stale/deleted entries before deciding to split. Often GC alone is sufficient (the `m_garbageNum` counter tracks this).

5. **Two NPA conditions** (paper §3.3, impl in ReAssign L1156): Condition 1 for vectors in the split posting, Condition 2 for vectors in neighboring postings. Both are *necessary* conditions — final NPA check confirms.

---

## 8. Success Criteria

1. **Recall@10 ≥ 0.85** on 10K random 128-dim vectors with `num_search_heads=64`
2. **Recall stability**: ≤ 10% degradation after 10 incremental batches (doubling dataset)
3. **No data loss**: all inserted vectors findable (version-filtered stale copies don't count)
4. **No panics** under concurrent insert + search load
5. **Clean separation**: SPFresh index works standalone without HTTP server or S3
