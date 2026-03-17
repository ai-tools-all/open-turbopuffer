---
title: "Gap Analysis — turbopuffer vs open-turbopuffer"
date: 2026-03-17
sources:
  - docs/tpuf-docs/001.md
  - docs/tpuf-docs/002.md
  - docs/high-level-summaries/2026-03-17-project-status.md
---

# Gap Analysis: turbopuffer vs open-turbopuffer

Features extracted from turbopuffer's public docs (001.md, 002.md), categorized as Essential (needed for a working product) vs Optimization (improves performance/cost/scale but not blocking).

---

## Feature Inventory

### A. Storage & Durability

| # | Feature | Category | We Have It? | Notes |
|---|---------|----------|-------------|-------|
| A1 | Object storage as source of truth | Essential | Yes | S3/R2 via OpenDAL |
| A2 | WAL on S3 per namespace prefix | Essential | Yes | `{ns}/wal/{seq}.wal` |
| A3 | Write batching (concurrent writes coalesced) | Essential | Yes | 500ms window via WriteBatcher |
| A4 | Conditional writes (If-None-Match for WAL) | Essential | Yes | Prevents duplicate sequences |
| A5 | LSM-style compaction | Essential | **No** | WAL grows unbounded, no merge/compact |
| A6 | NVMe SSD cache tier | Optimization | **No** | Everything in memory or S3, no disk cache |
| A7 | Memory LRU cache for hot data | Optimization | **No** | All postings loaded into memory |

### B. Indexing & Search

| # | Feature | Category | We Have It? | Notes |
|---|---------|----------|-------------|-------|
| B1 | SPFresh centroid-based ANN index | Essential | Yes | 10 modules, ~90% recall@10 |
| B2 | Index persistence to S3 | Essential | Yes | Manifest-last protocol, CAS |
| B3 | Index loading from S3 on startup | Essential | Yes | With WAL catch-up |
| B4 | Brute-force fallback for unindexed data | Essential | Partial | Falls back when index empty, but **not** for WAL entries newer than index |
| B5 | Async indexing (separate indexer process) | Essential | **No** | Indexing is inline in server process |
| B6 | BM25 full-text search | Essential | **No** | Vector-only |
| B7 | Metadata attribute filtering | Essential | **No** | No filter predicates |
| B8 | Exact indexes for metadata fields | Optimization | **No** | No attribute indexes |
| B9 | 3-roundtrip cold query design | Optimization | **No** | All data loaded into memory |
| B10 | Centroid index download → cluster fetch pipeline | Optimization | **No** | Centroids + all postings loaded at startup, not on-demand |

### C. Consistency & Correctness

| # | Feature | Category | We Have It? | Notes |
|---|---------|----------|-------------|-------|
| C1 | Strong consistency (write → immediate read) | Essential | Yes | Inserts go to WAL + index before response |
| C2 | Eventually consistent query option | Optimization | **No** | No consistency parameter |
| C3 | CAS on index manifest (double-indexer protection) | Essential | Yes | `If-Match: <etag>` |
| C4 | Unindexed WAL searchable via exhaustive scan | Essential | **No** | WAL entries after index are invisible until re-index |

### D. Multi-tenancy & Operations

| # | Feature | Category | We Have It? | Notes |
|---|---------|----------|-------------|-------|
| D1 | Namespace CRUD (create/list/delete) | Essential | Yes | REST API |
| D2 | Per-namespace S3 prefix isolation | Essential | Yes | `{ns}/` prefix |
| D3 | Any node can serve any namespace | Essential | Partial | Single process, no multi-node |
| D4 | Load balancer with cache-locality routing | Optimization | **No** | Single process |
| D5 | Separate query vs indexer binaries | Optimization | **No** | Single `tpuf-server` binary |
| D6 | Indexing queue on object storage | Optimization | **No** | No async queue |
| D7 | Pre-flight cache warming | Optimization | **No** | No `/warm-cache` endpoint |

---

## Gap Summary

### Essential Gaps (blocking for production use)

| Priority | Gap | Impact | Effort |
|----------|-----|--------|--------|
| **P0** | **A5: WAL compaction** | WAL grows unbounded → startup gets slower, S3 costs grow | Medium |
| **P0** | **B6: BM25 full-text search** | turbopuffer's core value prop is vector + full-text combined | Large |
| **P0** | **B7: Metadata attribute filtering** | Can't filter search results by attributes (e.g. `category = "news"`) | Medium |
| **P1** | **B5: Async indexing** | Indexing inline blocks write path; can't scale indexing independently | Medium |
| **P1** | **B4: Search unindexed WAL tail** | WAL entries after last index persist are invisible to queries until re-persist | Small |
| **P1** | **C4: Exhaustive scan of unindexed WAL** | Same as B4 — turbopuffer searches both index AND unindexed WAL | Small |

### Optimization Gaps (improve perf/cost/scale)

**Cache & Latency**

| Gap | Impact |
|-----|--------|
| A6: NVMe SSD cache tier | No disk cache → cold queries always go to S3, warm queries limited by RAM |
| A7: Memory LRU for postings | Can't handle datasets > RAM without OOM |
| B9: 3-roundtrip cold query | We load everything at startup, not on-demand per query |
| B10: On-demand cluster fetch | Centroids + postings all in memory, not fetched per query |
| D7: Pre-flight cache warming | No way to warm cache before latency-sensitive queries |

**Scale & Architecture**

| Gap | Impact |
|-----|--------|
| D4: LB with cache-locality routing | Single process, no cluster routing |
| D5: Separate query/indexer binaries | Can't scale query and indexing independently |
| D6: Indexing queue | No async pipeline for background indexing |
| C2: Eventually consistent option | Can't trade consistency for latency |
| B8: Exact attribute indexes | Filtering requires full scan, no pre-built indexes |

---

## What We Have That Works Well

| Area | Status |
|------|--------|
| S3-native WAL + index persistence | Solid — manifest-last, CAS, WAL catch-up |
| SPFresh ANN index | Solid — recall@10 = 1.0 through full stack |
| Conditional writes | Solid — tested against Cloudflare R2 |
| Write batching | Working — 500ms coalescing |
| REST API | Working — namespace CRUD, upsert, delete, query, get |
| Test coverage | 67 unit + 3 R2 integration tests |

---

## Recommended Next Steps

**Minimal viable product path (P0s):**
1. **WAL compaction** — merge old WAL entries so startup doesn't degrade
2. **Attribute filtering** — add filter predicates to query path
3. **Search unindexed WAL tail** — combine index results with brute-force scan of WAL entries after last persist

**Scale path (after MVP):**
4. BM25 full-text search
5. Async indexing (separate indexer)
6. LRU posting cache (for datasets > RAM)
7. NVMe SSD cache tier
