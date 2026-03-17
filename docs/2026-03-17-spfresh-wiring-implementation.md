---
title: "SPFresh Wiring Implementation"
date: 2026-03-17
depends_on:
  - docs/2026-02-26-14-32-07-spfresh-wiring-and-s3-centroids.md
status: done
---

# SPFresh Wiring Implementation — Progress

Wiring the SPFreshIndex into the NamespaceManager query/insert/delete paths.
This covers Steps 1-3 from the parent design doc (in-memory only, no S3 persistence).

## Phases

### Phase 1: Add SPFreshIndex to NamespaceState + feed inserts
- [x] Add `index: Option<SPFreshIndex>` field to `NamespaceState`
- [x] Lazy-create index on first vector upsert in `flush_batcher()`
- [x] Feed upsert/delete ops into the index after WAL apply
- [x] Rebuild index from documents during WAL replay in `load_namespace()`
- [x] Add `ObjectStore::in_memory()` test helper (OpenDAL memory backend)
- [x] Test: `test_index_built_on_upsert` — upsert 100 docs, verify `index.len() == 100` ✓
- [x] Test: `test_delete_removes_from_index` — upsert 100, delete 50, verify index.len() == 50 ✓

### Phase 2: Switch query path to use index
- [x] Use `index.search()` when index exists and has vectors
- [x] Fall back to brute-force when no index or empty
- [x] Safely handle index returning IDs for deleted docs (filter_map instead of unwrap)
- [x] Test: `test_query_uses_index` — upsert 200 docs, query doc 0's vector, verify doc 0 is top result ✓
- [x] Test: `test_query_deleted_not_returned` — delete doc, verify it's absent from results ✓
- [x] Test: `test_fallback_to_brute_force_empty` — query empty ns returns empty ✓

### Phase 3: Recall test through full NamespaceManager path
- [x] Test: `test_recall_through_namespace_manager` — 5000 docs, 128-dim, 50 queries, recall@10 = 1.0 ✓
- [x] Test: `test_mixed_ops_recall` — insert 500, delete 100, insert 200, recall@10 = 1.0 ✓

---

## Progress Log

### Phase 1 — DONE
- Added `index: Option<SPFreshIndex>` to `NamespaceState`
- Inserts/deletes now feed into the SPFreshIndex in `flush_batcher()`
- WAL replay rebuilds index from documents in `load_namespace()`
- Added `ObjectStore::in_memory()` for testing without MinIO
- 2 new tests pass, all 49 tests green

### Phase 2 — DONE
- Query path now uses `index.search()` when SPFreshIndex exists
- Falls back to brute-force when no index (empty namespace)
- Used `filter_map` in result mapping to handle index returning stale IDs
- 3 new tests pass (query, deleted-not-returned, empty fallback), all 52 tests green

### Phase 3 — DONE
- E2E recall@10 = 1.0 through NamespaceManager with 5000 vectors (128-dim)
- Mixed ops (insert/delete/insert) recall@10 = 1.0
- All 54 tests green
