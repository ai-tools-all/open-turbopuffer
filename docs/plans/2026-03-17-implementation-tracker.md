# Implementation Tracker: Hybrid Query + Separate Indexer

## Phase 1: Hybrid Query in tpuf-server — DONE ✓

Commit: `07fa7d9` — "feat: hybrid query — ANN index + brute-force on WAL tail"

- [x] `merge_top_k` added to search.rs
- [x] `doc_sequences` + `indexed_up_to_seq` added to NamespaceState
- [x] `apply_wal_entry` tracks doc sequences
- [x] Inline indexing removed from `flush_batcher`
- [x] Hybrid query: ANN + brute-force tail + dedup + merge
- [x] Tests updated + 5 new hybrid query tests
- [x] 72 tests pass, clippy clean

---

## Phase 2: tpuf-indexer crate — DONE ✓

Commit: `294a7d1` — "feat: tpuf-indexer — separate crate for building SPFresh index from WAL"

- [x] `lib.rs` added to tpuf-server (re-exports types, storage, engine, index)
- [x] `replay_into_index` and `apply_wal_entry` made `pub`
- [x] `tpuf-indexer` crate created with CLI (clap) + core indexer logic
- [x] Indexer: reads WAL → builds/updates SPFresh index → writes to S3 (CAS manifest)
- [x] 4 indexer tests: from_scratch, incremental, noop, server_loads_output
- [x] `test-utils` feature on tpuf-server for `ObjectStore::in_memory()`

---

## Phase 3: Remove inline indexing from server — DONE ✓

Commit: `585d3bc` — "feat: server no longer builds index — loads from S3 manifest only"

- [x] `build_index_from_documents` removed
- [x] WAL catchup replay removed from `try_load_index_from_s3`
- [x] `indexed_up_to_seq` set from manifest's `wal_sequence`
- [x] Without manifest: `index: None`, all queries brute-force
- [x] Tests updated with `build_and_persist_test_index` helper
- [x] 76 total tests pass (72 server + 4 indexer), clippy clean
