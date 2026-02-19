# SPFresh on S3 — Storage Design

**Date:** 2026-02-19

## Glossary

| Term | Definition |
|------|-----------|
| **Posting List** | A partition of vectors assigned to one centroid. Stored per `head_id`. The core data unit in SPFresh. |
| **Head Index** | The in-memory graph of centroid vectors. Routes queries to the nearest posting lists. |
| **Version Map** | Per-vector monotonic counter. Used to detect stale entries (tombstones) without physical deletion. |
| **Tombstone** | Logical delete — a vector's version is incremented, making all older posting entries invisible to search. |
| **NPA (Nearest Partition Assignment)** | The invariant that every vector must reside in the posting list of its nearest centroid. LIRE enforces this. |
| **LIRE** | SPFresh's update protocol. Maintains NPA during incremental inserts, deletes, and splits. |
| **Split** | When a posting list exceeds `max_posting_size`, it is divided into two via binary k-means, producing two new centroids. |
| **Reassignment** | After a split, neighboring vectors that now violate NPA are moved to the new nearest centroid. Version is incremented. |
| **Local Rebuilder** | Background process that handles splits and reassignments for a bounded neighborhood of partitions. |
| **Replica** | Each vector is written to `replica_count` posting lists (nearest centroids) to improve search recall. |
| **RNG Filter** | Relative Neighborhood Graph filter — selects diverse replicas to avoid placing a vector in clustered, redundant centroids. |
| **Write-Through Cache** | A pattern where all writes go to both an in-memory store (fast) and a durable backend (S3), in sync. |
| **Conditional Write** | An S3 operation that only succeeds if the object does not already exist (using `If-None-Match: *`). Used for optimistic locking. |
| **WAL (Write-Ahead Log)** | A sequential log of operations written to durable storage before they are applied. Enables crash recovery. |
| **Checkpoint / Snapshot** | A point-in-time serialization of in-memory state (head index, version map) flushed to S3. Reduces WAL replay on restart. |
| **LRU Cache** | Least-Recently-Used cache. Keeps hot posting lists in memory; evicts cold ones when capacity is exceeded. |
| **Read-Modify-Write** | The S3 append pattern: GET existing object → modify in memory → PUT back. Expensive; must be batched. |
| **OpenDAL** | The Rust storage abstraction library used in this project. Provides a unified API over S3, GCS, local disk, etc. |
| **SPDK** | Storage Performance Development Kit. Used in the SPFresh paper to bypass the OS filesystem for low-latency NVMe I/O. Not applicable to S3. |
| **Hot Tier** | In-memory posting store serving live reads and writes. |
| **Cold Tier** | S3-backed persistent store. Source of truth for durability and cross-restart recovery. |

---

## Background

SPFresh was designed for NVMe block storage with SPDK, optimizing for high-throughput random I/O. Moving to S3 changes the fundamental storage contract:

- **NVMe**: random read/write, microsecond latency, in-place append
- **S3**: object store, 10–50ms per request, immutable objects, no atomic multi-object writes

The adaptation requires reshaping SPFresh's access patterns from random I/O into batched sequential writes with an in-memory hot tier.

---

## Current State

The implementation today has the entire SPFresh index in RAM:

| Component | Type | Location |
|-----------|------|----------|
| `posting_store` | `MemoryPostingStore` | RAM only |
| `head_index` | `RwLock<HeadIndex>` | RAM only |
| `version_map` | `RwLock<VersionMap>` | RAM only |

S3 (via OpenDAL) currently stores only:
- `{ns}/meta.bin` — namespace metadata
- `{ns}/wal/{seq}.wal` — WAL entries (vectors, not index structure)

---

## Key Areas & Required Changes

### 1. PostingStore Trait — Sync/Async Mismatch

`PostingStore::get/put/append` are synchronous. S3 is async. Three options:

| Option | Mechanism | Tradeoff |
|--------|-----------|----------|
| Blocking wrapper | `block_on` inside trait methods | No trait change; wastes thread per S3 call during splits |
| Async trait | `#[async_trait]` + cascade `await` | Clean but cascading refactor across rebuilder, updater, search |
| **Write-through cache** | Memory stays primary; S3 is persistence layer | Zero trait change; S3 only touched on flush/load. Recommended. |

**Recommendation:** write-through cache. `MemoryPostingStore` remains the hot tier; an async flush task drains it to S3 periodically and after splits.

---

### 2. S3 Object Layout for Posting Lists

S3 has no append primitive. Today's `posting_store.append(head_id, entries)` is O(1) in memory. On S3 it becomes a read-modify-write cycle.

| Layout | Key Pattern | Problem |
|--------|-------------|---------|
| One object per posting | `{ns}/index/postings/{head_id}.bin` | Many small objects; append = GET + PUT |
| Batched shards | `{ns}/index/shards/{n}.bin` | Harder random access; shard routing index needed |
| Immutable segments + compaction | `{ns}/seg/{epoch}/{head_id}.bin` | Fits S3's write-once model; most efficient at scale |

**Recommendation:** one object per posting list for simplicity. Buffer appends in memory and flush in batches. Avoid per-insert S3 writes.

S3 key layout:

```
{ns}/index/head_index.bin          # centroid graph snapshot
{ns}/index/version_map.bin         # version/tombstone snapshot
{ns}/index/postings/{head_id}.bin  # one per partition
{ns}/wal/{seq}.wal                 # existing WAL (unchanged)
```

---

### 3. HeadIndex & VersionMap Persistence

Both currently live only in RAM and are lost on restart.

- **HeadIndex**: Small (centroids × dimensions × 4 bytes). Snapshot as a single S3 object. Changes only on splits — write after each split completes. Use conditional write to prevent concurrent split races in multi-instance setups.
- **VersionMap**: Larger at scale (one `u64` per vector). Snapshot periodically. Must be consistent with posting list snapshots — snapshot all three structures together, or accept bounded replay from WAL.

---

### 4. Split Atomicity

A split today:
1. Read oversized posting list
2. Run binary k-means → two clusters
3. Write two new posting lists
4. Update head index (add 2 centroids, possibly remove 1)
5. Reassign neighboring vectors → append to new postings

Steps 3–5 are not atomic on S3. A crash mid-split leaves a partially-updated index.

| Strategy | Mechanism | Tradeoff |
|----------|-----------|----------|
| Pending splits log | Write split intent to S3 first; replay on recovery | Correct; adds recovery complexity |
| **Write new objects first, then update head index** | New postings invisible until head index points to them | Safe — head index is the routing table. Search still finds vectors via old postings until cutover. |
| Accept bounded inconsistency | Replicas cover missed vectors during split window | SPFresh replicas are designed for exactly this; acceptable for ANN |

**Recommendation:** write new posting objects to S3 first (safe, invisible to search), then update `head_index.bin` as the atomic cutover. Since the head index is a single object, one PUT is the commit point.

---

### 5. Read Latency — LRU Cache Required

Current search touches `MemoryPostingStore` with microsecond latency. On S3:

- Cold read per posting = 10–50ms
- Searching `num_search_heads = 64` postings in parallel = ~50ms total (dominated by first uncached request)

Without a cache, query latency degrades by 3–4 orders of magnitude.

**Required:** LRU posting cache in front of S3. Hot partitions stay cached; cold ones page in on first access.

Cache invalidation trigger: when a split occurs, evict the old `head_id` entries and warm the new ones.

---

### 6. Startup Recovery

Current behavior: restart = empty index, WAL replay rebuilds vector state.

With S3-backed index, startup should be:

```
1. Load head_index snapshot from S3
2. Load version_map snapshot from S3
3. Lazy-load posting lists on cache miss (no eager load)
4. Identify WAL entries after last snapshot sequence number
5. Replay delta WAL to catch up inserts/deletes
```

Snapshot cadence tradeoff:
- Frequent snapshots → faster restarts, more S3 writes, higher cost
- Sparse snapshots → slower restarts (longer WAL replay), cheaper

---

### 7. Distributed Consistency (Multi-Instance)

For single-instance, all of the above is sufficient. For multi-instance against a shared bucket:

| Component | Problem | Solution |
|-----------|---------|----------|
| Head index updates | Concurrent splits overwrite each other | Conditional write (`If-None-Match` on etag) |
| Posting list writes | Two instances append to same partition | Per-instance WAL segments, merged by compactor |
| Version map | Distributed increment requires coordination | Leader election for rebuilder; only one instance runs splits |

**Recommendation:** for the initial implementation, use a single-leader rebuilder model. Only one instance is permitted to run splits at a time (coordinator lock or leader lease).

---

## Proposed Architecture

```
Insert/Delete
    │
    ▼
MemoryPostingStore (hot tier)   ◄─── all live reads/writes
    │
    ├── async flush task ───────► S3 postings/{head_id}.bin
    │
    ├── after split ────────────► write new posting objects to S3
    │                          ► update head_index.bin (atomic cutover)
    │
    └── periodic checkpoint ───► head_index.bin + version_map.bin

Search
    │
    ▼
HeadIndex (always in RAM) ────► posting_store.get(head_ids)
                                      │
                                LRU cache hit? ──► return
                                      │
                                cache miss ─────► S3 GET ──► populate cache
```

---

## Change Summary

| Change | Complexity | Required |
|--------|-----------|---------|
| S3 key layout for posting lists (one object per `head_id`) | Low | Yes |
| Checkpoint: snapshot `head_index` + `version_map` to S3 | Medium | Yes |
| Write-through flush with batching (async flush task) | Medium | Yes |
| LRU cache in front of S3 posting reads | Medium | Yes |
| Split ordering: write new postings before updating head index | Medium | Yes |
| WAL delta replay after snapshot load on startup | Medium | Yes |
| Async trait or blocking wrapper for `PostingStore` | High | Only if dropping hot tier |
| Conditional writes for `head_index.bin` updates | Low | If multi-instance |
| Distributed coordination for version map | High | Only if multi-instance |

---

## Core Tradeoff

SPFresh was designed for SPDK + NVMe: high-throughput random I/O, microsecond seek latency. S3 inverts every assumption:

- No random append → must buffer and batch
- No atomic multi-object write → must sequence writes carefully (postings before head index)
- High per-request latency → must cache aggressively
- Cheap at scale, durable, shared → the actual reason to use it

The adaptation is: **treat S3 as a durable, shareable, cheaper NVMe substitute**, not as a filesystem. Keep the hot path in RAM; use S3 for durability, cold page-in, and cross-instance sharing.
