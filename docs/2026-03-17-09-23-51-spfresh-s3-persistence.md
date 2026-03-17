---
title: "SPFresh S3 Index Persistence"
date: 2026-03-17
depends_on:
  - docs/2026-02-26-14-32-07-spfresh-wiring-and-s3-centroids.md
  - docs/2026-03-17-spfresh-wiring-implementation.md
status: done
---

# SPFresh S3 Index Persistence — Progress

Persist the SPFreshIndex (centroids, postings, version map) to S3 and load on startup.
Covers Steps 4-7 from the parent design doc.

## Phases

### Phase 4: Serialization roundtrip for index types
- [x] Add `CentroidsFile` struct + `to_file()` / `from_file()` on HeadIndex
- [x] Add `VersionMapFile` struct + `to_file()` / `from_file()` on VersionMap
- [x] Add `Serialize`/`Deserialize` to `SPFreshConfig`
- [x] Add `IndexManifest` type in `types/mod.rs`
- [x] Add `SPFreshIndex::from_parts()` constructor + accessor methods
- [x] Test: `test_centroids_roundtrip` — HeadIndex with inactive centroid survives roundtrip ✓
- [x] Test: `test_version_map_roundtrip` — versions, deletions survive roundtrip ✓
- [x] Test: `test_manifest_roundtrip` — IndexManifest bincode encode/decode ✓

### Phase 5: S3 storage methods for index files
- [x] Add key helpers (manifest, centroids, version_map, posting)
- [x] Add write/read methods for each index type (8 methods total)
- [x] Test: `test_s3_centroids_write_read` ✓
- [x] Test: `test_s3_centroids_missing` — returns None for missing ✓
- [x] Test: `test_s3_version_map_write_read` ✓
- [x] Test: `test_s3_posting_write_read` ✓
- [x] Test: `test_s3_posting_missing` — returns None ✓
- [x] Test: `test_s3_manifest_write_read` ✓

### Phase 6: persist_index (manifest-last protocol)
- [x] Implement `persist_index()` on NamespaceManager
- [x] Write postings → centroids → version_map → manifest (last)
- [x] Test: `test_persist_index` — upsert 200, persist, verify manifest/centroids/version_map exist ✓

### Phase 7: load_index from S3 on startup (with WAL catch-up)
- [x] Modify `load_namespace()` to try loading index from S3 manifest
- [x] Implement WAL catch-up for entries after manifest's wal_sequence
- [x] Fall back to WAL rebuild if manifest missing or corrupt
- [x] Test: `test_persist_and_load_roundtrip` — 1000 vecs, persist, reload, recall@10 = 1.0 ✓
- [x] Test: `test_incremental_wal_replay` — persist 500, add 200 more, reload has 700 ✓
- [x] Test: `test_no_manifest_falls_back_to_wal_rebuild` — no manifest, WAL rebuild gives 300 ✓

---

## Progress Log

### Phase 4 — DONE
- Added serialization types: `CentroidsFile`, `VersionMapFile`, `IndexManifest`
- HeadIndex/VersionMap get `to_file()`/`from_file()` for snapshot→restore
- SPFreshConfig now has `Serialize`/`Deserialize`
- SPFreshIndex gets `from_parts()` constructor + accessor methods
- 3 new roundtrip tests pass, all 57 tests green

### Phase 5 — DONE
- 8 new S3 methods (write/read for manifest, centroids, version_map, postings)
- Key format: `{ns}/index/{manifest|centroids|version_map}.bin`, `{ns}/index/postings/{hid:010}.bin`
- 6 new storage tests pass, all 63 tests green

### Phase 6 — DONE
- `persist_index()` writes postings → centroids → version_map → manifest (manifest-last)
- If crash before manifest write, startup ignores partial data
- 1 new test passes, all 64 tests green

### Phase 7 — DONE
- `load_namespace()` tries S3 manifest first, falls back to WAL rebuild
- `try_load_index_from_s3()` loads centroids, postings, version_map from S3
- WAL catch-up replays entries after manifest's `wal_sequence`
- `replay_into_index()` feeds WAL ops into a loaded index
- 3 new tests pass (roundtrip recall=1.0, incremental WAL, fallback), all 67 tests green
