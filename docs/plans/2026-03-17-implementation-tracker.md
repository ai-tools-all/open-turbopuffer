# Implementation Tracker: Hybrid Query + Separate Indexer

## Phase 1: Hybrid Query in tpuf-server

### 1A: Add merge_top_k to search.rs
- [ ] Add `merge_top_k(a, b, top_k) -> Vec<(u64, f32)>` to `search.rs`
- **Verify:** `cargo test -p tpuf-server search` (existing search tests pass)

### 1B: Extend NamespaceState + WAL replay tracking
- [ ] Add `doc_sequences: HashMap<u64, u64>` to `NamespaceState`
- [ ] Add `indexed_up_to_seq: u64` to `NamespaceState`
- [ ] Update `apply_wal_entry` to accept `&mut HashMap<u64, u64>` and populate doc_sequences
- [ ] Update `load_namespace` WAL replay to pass doc_sequences
- [ ] Update `load_namespace` state construction to set `indexed_up_to_seq = wal_sequence`
- [ ] Update `create_namespace` to init new fields (doc_sequences: empty, indexed_up_to_seq: 0)
- **Verify:** `cargo build -p tpuf-server` compiles

### 1C: Remove inline indexing from flush_batcher
- [ ] Change `apply_wal_entry` call in flush_batcher to pass `&mut state.doc_sequences`
- [ ] Remove inline `index.insert()` / `index.delete()` block (lines 356-385)
- **Verify:** `cargo build -p tpuf-server` compiles

### 1D: Hybrid query logic
- [ ] Import `merge_top_k` in namespace.rs
- [ ] Replace `query()` body with hybrid logic:
  - If index exists: ANN search + brute-force on tail (docs where doc_seq > indexed_up_to_seq)
  - Filter ANN results to exclude IDs in tail (stale vectors)
  - Merge results
  - If no index: full brute-force (existing behavior)
- **Verify:** `cargo build -p tpuf-server` compiles

### 1E: Update existing tests + add new Phase 1 tests
- [ ] Update `test_index_built_on_upsert` → verify query returns results (not index_len)
- [ ] Update `test_delete_removes_from_index` → verify query excludes deleted docs
- [ ] Update `test_persist_index` → restart cycle to build index before persisting
- [ ] Update `test_persist_and_load_roundtrip` → restart cycle
- [ ] Update `test_incremental_wal_replay` → restart cycle
- [ ] Add `test_hybrid_query_finds_unindexed` — persist at seq=N, add more, query finds both
- [ ] Add `test_hybrid_query_recall` — 1000 indexed + 200 unindexed, recall@10 > 0.85
- [ ] Add `test_fully_indexed_no_tail_scan` — indexed_up_to == wal_seq → no brute-force needed
- [ ] Add `test_delete_in_tail` — delete doc from tail, query doesn't return it
- [ ] Add `test_no_index_full_brute_force` — fresh namespace, all brute-force works
- **Verify:** `cargo test -p tpuf-server` — ALL tests pass

### Phase 1 commit
- `cargo clippy --workspace` clean
- `cargo test -p tpuf-server` all pass
- Commit: "feat: hybrid query — ANN index + brute-force on WAL tail"

---

## Phase 2: tpuf-indexer crate

### 2A: Make tpuf-server a library
- [ ] Add `lib.rs` to tpuf-server that re-exports types, storage, engine, and index modules
- [ ] Add `[[bin]]` + `[lib]` sections to tpuf-server's Cargo.toml
- **Verify:** `cargo build -p tpuf-server` still compiles

### 2B: Create tpuf-indexer crate
- [ ] Add `crates/tpuf-indexer` to workspace Cargo.toml
- [ ] Create `crates/tpuf-indexer/Cargo.toml` with dep on `tpuf-server`
- [ ] Create `src/main.rs` — CLI with clap: --namespace, --s3-endpoint, --s3-bucket, etc.
- [ ] Create `src/indexer.rs` — core logic:
  1. Read namespace metadata from S3
  2. Read current manifest (if exists) → get wal_sequence marker
  3. List WAL sequences > manifest.wal_sequence
  4. If no new WAL → exit
  5. Load existing index from S3 OR create new
  6. Replay new WAL entries into index
  7. Write updated index to S3 (postings → centroids → version_map → manifest CAS)
- **Verify:** `cargo build -p tpuf-indexer` compiles

### 2C: Indexer tests
- [ ] `test_indexer_builds_from_scratch` — WAL entries → indexer → manifest + centroids + postings exist
- [ ] `test_indexer_incremental` — run at seq 5, add WAL, run again, manifest advances
- [ ] `test_indexer_noop_when_caught_up` — no new WAL → clean exit
- [ ] `test_server_loads_indexer_output` — indexer writes, server loads, queries work
- **Verify:** `cargo test -p tpuf-indexer` all pass

### Phase 2 commit
- `cargo clippy --workspace` clean
- `cargo test --workspace` all pass
- Commit: "feat: tpuf-indexer — separate crate for building SPFresh index from WAL"

---

## Phase 3: Remove inline indexing from server

### 3A: Remove build_index_from_documents
- [ ] Remove `build_index_from_documents` method
- [ ] In `load_namespace`: if no manifest, set `index: None` (no fallback rebuild)
- [ ] Set `indexed_up_to_seq` from manifest.wal_sequence when loading from S3 manifest
- [ ] Remove WAL catchup replay from `try_load_index_from_s3`
- **Verify:** `cargo build -p tpuf-server` compiles

### 3B: Update tests for no-fallback behavior
- [ ] Update `test_no_manifest_falls_back_to_wal_rebuild` → now expects index: None, brute-force
- [ ] Add `test_server_without_index` — no manifest, queries via brute-force
- [ ] Add `test_server_after_indexer` — indexer writes, server reloads, ANN + tail
- **Verify:** `cargo test --workspace` all pass

### Phase 3 commit
- `cargo clippy --workspace` clean
- `cargo test --workspace` all pass
- Commit: "feat: server no longer builds index — loads from S3 manifest only"
