# SPFresh Redesigned for S3 — Disaggregated Storage

**Date:** 2026-02-19

---

## Framing

SPFresh makes specific hardware assumptions throughout its design. Before redesigning, we map each of the 5 core pieces to the assumptions it requires from the storage layer — and identify which S3 violates, which it relaxes, and which it actually improves.

---

## S3 Advantages Worth Exploiting

Before listing what breaks, what S3 gives us that NVMe cannot:

| Property | S3 | NVMe/SPDK |
|----------|----|-----------|
| Parallel I/O | Effectively unlimited (fire 1000 GETs simultaneously) | Bounded by single-drive IOPS (~1–4M) |
| Storage capacity | Infinite | Fixed SSD capacity |
| Durability | 11 nines, no RAID needed | Requires RAID, backups |
| Disaggregation | Compute and storage are separate services | Co-located; scale together |
| CAS (atomic single-object) | `If-None-Match` / `If-Match` on ETag | Requires OS-level locks |
| Byte-range GET | Read arbitrary byte ranges of an object | Block-granular random access |
| Cost at scale | Cheap per GB, per request | Expensive per GB, finite IOPS |
| Multi-writer | Multiple compute nodes can write to different keys simultaneously | Requires shared-nothing or careful locking |

**The core opportunity:** S3 turns the single-node IOPS bottleneck into a parallelism opportunity. SPFresh was designed to saturate one NVMe drive. On S3, we can saturate a cluster of compute workers — the storage layer scales independently.

---

## Piece-by-Piece Assumption Analysis & Redesign

---

### Piece 1: Cluster-Based Index (SPANN Foundation)

#### SSD Assumptions
- Posting lists are stored as contiguous blocks on a locally attached disk
- Random read of any posting list takes microseconds
- Block-addressable: can seek to a specific offset within a posting without reading the whole file
- Storage is co-located with compute (same machine)

#### S3 Properties
- Objects addressed by key, not block offset
- Byte-range GET allows sub-object reads (approximate equivalent of seek)
- Latency is 10–50ms per GET, but GETs are cheap and parallelizable
- Storage is fully decoupled from compute

#### S3 Violations
- No microsecond random access → single-node search with cold cache is much slower
- Sub-object appends require read-modify-write (or multipart upload)

#### Redesign: Immutable Generation Objects

Replace mutable on-disk posting lists with **immutable versioned objects**:

```
{ns}/postings/{head_id}/base-{gen}.bin   # immutable compacted posting list
{ns}/postings/{head_id}/delta-{seq}.bin  # append-only write deltas (WAL-style)
```

- The **base** is the last fully compacted posting list. Never mutated.
- **Deltas** are append-only flush events from the write buffer. Multiple small deltas accumulate.
- Search reads base + all deltas for a head_id, merges in memory. Deltas are small (one flush worth).
- Background compactor merges base + deltas → new base, deletes old. Triggered when delta count is high.

This is an **LSM-tree structure applied per posting list**, matching S3's write-once semantics exactly.

Head index is a single manifest object:

```
{ns}/meta/manifest.json   # { head_index_gen, version_map_gen, generation_seqs }
{ns}/meta/head-{gen}.bin  # centroid graph snapshot
{ns}/meta/vmap-{gen}.bin  # version map snapshot
```

---

### Piece 2: LIRE Protocol (Lightweight Incremental Rebalancing)

#### SSD Assumptions
- Splits and merges require reading neighboring posting lists quickly (local random I/O)
- Reassignment scans many neighbors — fast because I/O is fast on local NVMe
- Writes during reassignment (appends to neighbor postings) are cheap
- Split state changes are local and can be made consistent via memory barriers

#### S3 Properties
- Reading N neighbor posting lists = N parallel GETs (expensive in latency, free in throughput)
- CAS via `If-Match` on ETag gives optimistic single-object locking
- No atomic multi-object write — must sequence carefully
- Write parallelism: can PUT multiple new objects simultaneously

#### S3 Violations
- No atomic update spanning the split itself (new postings + new head index)
- Reassignment neighbor scan latency is higher (each neighbor = one or more GETs)

#### S3 Advantages
- Reassignment neighbor GETs can be fully parallel — fire all N at once, S3 handles it
- On NVMe, you're queueing behind one drive's IOPS; on S3, all N requests fly simultaneously
- This potentially makes LIRE **faster for large reassignment windows on S3 than on NVMe**

#### Redesign: CAS-Gated Splits with Parallel Reassignment

**Split sequence:**

```
1. Detect oversized posting (head index tracks per-partition size)
2. GET base + deltas for head_id (parallel GETs)
3. Merge in memory, run binary k-means → cluster A, cluster B
4. PUT new base-{gen+1}.bin for cluster A key
5. PUT new base-{gen+1}.bin for cluster B key (can be parallel with step 4)
6. GET current manifest → record ETag
7. Compute new head index (add 2 centroids, remove old)
8. PUT new head-{gen+1}.bin
9. CAS PUT new manifest.json with If-Match: {ETag from step 6}
   → If 412 Precondition Failed: another node raced us → retry from step 6
   → If 200: split committed
```

**Step 9 is the commit point.** Before the manifest is updated, search is unaffected — old head index still points to old postings. After the manifest flip, new postings are live. Old posting objects are GC-eligible.

**Reassignment sequence (after split commit):**

```
1. Identify neighborhood (centroids within reassign_range of split centroid)
2. Fire parallel GETs for all neighbor postings
3. For each neighbor posting: evaluate NPA for each entry against new centroids
4. For violating entries: increment version in version map, write delta to their new posting
5. CAS update version_map (same pattern as manifest CAS)
```

S3's parallel GET is the key: on NVMe you queue, on S3 you fan out. Reassignment is embarrassingly parallel and S3 is built for this.

**Merge sequence (symmetric to split, but simpler):**

```
1. GET both undersized postings
2. Merge entries in memory
3. PUT merged base to surviving head_id key
4. CAS update manifest to remove dead centroid
```

---

### Piece 3: Decoupled Architecture (Foreground Updater + Background Rebuilder)

#### SSD Assumptions
- Foreground updater and background rebuilder share the same local disk
- Foreground appends go to a locally attached drive with microsecond commit
- Rebuilder is a background thread on the same machine reading the same local storage
- State handoff (oversized posting notifications) is in-process via channel or shared memory

#### S3 Properties
- Compute and storage are already separated
- Multiple compute nodes can access the same S3 bucket simultaneously
- Communication between components must go through S3 or a lightweight coordination layer
- No shared memory — state handoff must be explicit

#### S3 Violations
- No in-process channel for "this posting is oversized" — needs an external signal mechanism
- Foreground writes can't be microsecond commits — must buffer or accept higher write latency

#### S3 Advantages
- **Disaggregation is natural**: foreground updater and rebuilder can be separate services or even serverless functions
- Rebuilder can scale horizontally — run more workers for faster background processing
- Rebuild jobs survive compute node failure (state is in S3)

#### Redesign: True Disaggregated Compute Model

```
                     ┌─────────────────────────────┐
                     │           S3 Bucket          │
                     │  postings/ meta/ wal/ jobs/  │
                     └──────────────┬──────────────┘
                                    │
              ┌─────────────────────┼──────────────────────┐
              │                     │                      │
    ┌─────────▼────────┐  ┌────────▼────────┐  ┌─────────▼────────┐
    │  Ingestion Worker │  │  Query Worker   │  │ Rebuild Worker   │
    │  (stateless)      │  │  (stateless)    │  │  (stateless)     │
    │                   │  │                 │  │                  │
    │  Buffer writes    │  │  LRU cache of   │  │  Reads jobs/     │
    │  Flush WAL to S3  │  │  hot postings   │  │  Runs splits     │
    │  Write to jobs/   │  │  Reads from S3  │  │  Writes results  │
    │  when oversized   │  │  on cache miss  │  │  CAS manifest    │
    └───────────────────┘  └─────────────────┘  └──────────────────┘
```

**Oversized notification via S3 job queue:**

```
{ns}/jobs/split/{head_id}-{timestamp}.json   # created by ingestion worker
```

Rebuild workers poll (or are triggered via S3 Event Notifications / SQS) for split jobs. Multiple workers can compete — CAS on the manifest ensures only one wins. Losers discard their work.

**Foreground write path:**

```
1. Accept insert → append to in-memory buffer per head_id
2. Write WAL entry to S3 for durability (async, non-blocking to client)
3. When buffer exceeds threshold (e.g. 1000 vectors or 5s):
   → PUT delta-{seq}.bin for each modified head_id
   → If any head_id size > max_posting_size: PUT split job to S3
4. ACK to client after WAL write (step 2) for durability guarantee
```

**Rebuild worker is now a stateless job processor** — it can be a Lambda, a container, or a thread pool. It reads from S3, processes, writes back to S3. If it crashes mid-split, no state is lost; the job remains and another worker picks it up (idempotent operations throughout).

---

### Piece 4: Version Maps & Lazy Garbage Collection

#### SSD Assumptions
- Version map lives in memory on the machine that holds the posting lists (1 byte per vector, 7-bit version + 1-bit tombstone)
- GC happens naturally during splits (stale entries pruned as a side effect)
- Stale entries on SSD are free — they cost no money, just space
- The version map is local state, consistent because it's on one machine

#### S3 Properties
- No shared in-memory state between compute nodes
- Stale objects on S3 cost money (per GB per month)
- S3 lifecycle rules can auto-delete objects older than N days
- Version map must be a durable S3 object, not just in-memory
- Multiple compute nodes need a consistent view of the version map

#### S3 Violations
- "Stale entries are free" is false — S3 charges for storage
- No shared in-process memory → version map must be serialized, with CAS for updates

#### S3 Advantages
- S3 Lifecycle Rules: auto-expire old-generation objects without application logic
- Version map itself becomes a durable, crash-safe structure on S3
- GC can run as a separate background process on any compute node

#### Redesign: Durable Version Map + Proactive GC

**Version map as a durable S3 object:**

```
{ns}/meta/vmap-{gen}.bin    # full snapshot (bincode + zstd, current approach)
{ns}/meta/vmap-delta-{seq}.bin  # incremental version changes since last snapshot
```

Update pattern:
- Increments and tombstone sets are buffered in memory
- Flushed as a delta object to S3 on each WAL flush cycle
- Periodic compaction: merge all deltas into new full snapshot, CAS update manifest to point to new gen

**GC is now explicit and cost-motivated (S3 charges for storage):**

```
GC Worker responsibilities:
1. Read manifest → get current head index generation, version map generation
2. List all objects in postings/ → find generation-tagged objects older than current
3. For each stale base/delta: DELETE (S3 lifecycle rules can also handle this)
4. For each generation-stale version map delta: DELETE after compaction
```

S3 lifecycle rules can auto-delete `{ns}/postings/{head_id}/base-{gen}.bin` objects where `gen < current_gen - 2` (keep one generation of safety margin). This requires tagging objects with their generation at write time.

**Version staleness check during search (unchanged from SPFresh paper logic):**

Search workers load the version map snapshot from S3 at startup (or cache it). Delta objects are read on cache miss. Version check is still `entry.version == vmap.get(entry.vector_id)` — same logic, now sourced from a durable S3-backed map.

---

### Piece 5: Block Controller (Custom SPDK Storage Engine)

#### SSD Assumptions
- OS filesystem adds read/write amplification — SPDK bypasses it entirely
- Appending to a posting list reads the **last block only**, appends the new vector, writes that block back (block-level append, not full-file rewrite)
- High IOPS is the bottleneck — SPDK saturates NVMe at ~1M IOPS
- Sequential layout on SSD means block-level append is truly cheap
- Batch reads (ParallelGET in the paper) overcome seek latency by pipelining

#### S3 Properties
- No SPDK needed — S3 abstracts all I/O and charges per request, not per IOPS
- No block-level append — objects are immutable once PUT; modification = new PUT
- Byte-range GET (`Range: bytes=X-Y`) allows partial object reads
- Unlimited parallel requests — S3 can serve thousands of requests/second per prefix
- Multipart Upload allows composing a large object from up to 10,000 parts

#### S3 Violations
- Block-level append is fundamentally impossible — every append is a new object
- There is no concept of "IOPS bottleneck" — the bottleneck is per-request cost and latency

#### S3 Advantages
- ParallelGET was SPFresh's clever trick to use NVMe bandwidth — S3 makes this the **default**. Firing 64 GETs simultaneously is trivially correct on S3.
- Object immutability makes caching perfect — ETags enable cache validation with no staleness risk.
- Byte-range GET allows fetching only the most recent delta (i.e., simulated append-read) without reading the full object.

#### Redesign: Write Buffer + Delta Objects Replaces Block Controller

The Block Controller's job was: **make appends cheap**. On S3 we replace this with a write buffer that batches multiple vector appends into a single PUT:

```
Block Controller (NVMe):             Write Buffer (S3):
  append(vector) →                     buffer.push(vector)
    read last block (1 block I/O)       if buffer.size >= threshold:
    append to block                       serialize buffer
    write block back (1 block I/O)        PUT delta-{seq}.bin to S3
                                          clear buffer

Cost: 2 block I/Os per vector           Cost: 1 PUT per N vectors
```

**Effective append cost on S3 = 1 PUT / N vectors**, amortized. Choose N based on S3 request cost vs write latency budget.

**ParallelGET becomes trivial:**

SPFresh's paper section on ParallelGET was an optimization for NVMe to overcome seek latency by batching reads. On S3:

```rust
// Just fire them all simultaneously — S3 handles it
let posting_futures: Vec<_> = head_ids.iter()
    .map(|id| object_store.get(posting_key(id)))
    .collect();
let postings = futures::future::join_all(posting_futures).await;
```

The S3 service scales to absorb all parallel requests. This is not an optimization to implement — it's free.

**Byte-range GET for delta reads:**

When a posting has a large base and a small recent delta, search only needs both. For the base, cache it. For the delta, read the whole delta object (it's small by design). No partial-read optimization needed when delta objects are bounded in size.

---

## New System Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        S3 Bucket Layout                          │
│                                                                   │
│  {ns}/meta/manifest.json          ← CAS-gated global pointer    │
│  {ns}/meta/head-{gen}.bin         ← centroid graph immutable    │
│  {ns}/meta/vmap-{gen}.bin         ← version map snapshot        │
│  {ns}/meta/vmap-delta-{seq}.bin   ← version map deltas          │
│                                                                   │
│  {ns}/postings/{hid}/base-{gen}.bin   ← compacted posting       │
│  {ns}/postings/{hid}/delta-{seq}.bin  ← write buffer flush      │
│                                                                   │
│  {ns}/wal/{seq}.wal               ← durability WAL (existing)   │
│  {ns}/jobs/split/{hid}-{ts}.json  ← rebuild job queue           │
└─────────────────────────────────────────────────────────────────┘
          ▲                  ▲                  ▲
          │                  │                  │
┌─────────┴────┐   ┌─────────┴────┐   ┌────────┴─────┐
│  Ingestion   │   │   Query      │   │   Rebuild    │
│  Workers     │   │   Workers    │   │   Workers    │
│  (stateless) │   │  (stateless) │   │  (stateless) │
│              │   │              │   │              │
│  write buf   │   │  LRU cache   │   │  reads jobs  │
│  WAL flush   │   │  parallel    │   │  splits/     │
│  delta PUT   │   │  GET search  │   │  merges      │
│  job emit    │   │              │   │  CAS commit  │
└──────────────┘   └──────────────┘   └──────────────┘
```

All three worker types are **stateless**. They can be added or removed without coordination. Storage state lives entirely in S3.

---

## How Each Piece Changed

| Piece | SSD Design | S3 Redesign | Key Insight |
|-------|-----------|-------------|-------------|
| **1. Cluster Index** | Mutable contiguous posting files | Immutable base + delta objects per partition | S3 is write-once; embrace it with LSM-style layering |
| **2. LIRE Protocol** | Local sequential reassignment scan | Parallel GET of all neighbors simultaneously | S3's unlimited parallelism makes large reassignment windows cheaper than NVMe |
| **3. Foreground/Background** | Co-located threads on one machine | Separate stateless services; job queue via S3 | True disaggregation — compute and storage scale independently |
| **4. Version Map + GC** | In-memory; GC is a side effect of splits | Durable S3 object; GC is explicit and cost-motivated | S3 charges for stale data → GC becomes a correctness AND cost concern |
| **5. Block Controller** | SPDK bypass; block-level append; ParallelGET optimization | Write buffer batching; ParallelGET is native; byte-range GET for deltas | The Block Controller's purpose was to make appends cheap — write buffering achieves the same at a higher level |

---

## Tradeoffs Introduced

| Tradeoff | S3 Design | NVMe Design |
|----------|-----------|-------------|
| Write latency | Higher (buffered; client waits for WAL PUT) | Lower (in-memory + block append) |
| Search latency (hot) | Same (LRU cache hit) | Same (in-memory) |
| Search latency (cold) | ~50ms (S3 GET on cache miss) | ~0.1ms (NVMe block read) |
| Split/rebuild throughput | Potentially higher (parallel GETs) | Bounded by single drive IOPS |
| Memory per node | Only hot posting LRU cache | Full posting store |
| Compute scaling | Independent of storage | Must scale together |
| Storage cost | Pay per byte | Pay for SSD capacity regardless |
| Durability | Built in (11 nines) | Requires RAID, backups |
| Multi-tenancy | Natural (isolated key prefixes) | Complex (disk partitioning) |
| Crash recovery | Stateless workers; just restart | Must recover local disk state |

---

## The Central Principle

> SPFresh solved "global rebuild" by making updates **local**. The S3 redesign extends this: by making all state **immutable and externalized**, we make compute **stateless**. Split a node, add a node, crash a node — the index is always recoverable from S3. The unit of work shifts from "memory-local operation" to "object-scoped operation with CAS coordination."
