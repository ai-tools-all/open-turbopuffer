---
title: "S3 Conditional Write Guards Implementation"
date: 2026-03-17
depends_on:
  - docs/2026-02-12-18-58-08-s3-conditional-writes.md
  - docs/2026-03-17-spfresh-s3-persistence.md
beads_issue: ot-004
status: done
---

# S3 Conditional Write Guards — Progress

Add If-None-Match and If-Match conditional writes as safety guards.
Test against Cloudflare R2 (not local MinIO).

## Phases

### Phase 1: Add conditional write methods to ObjectStore
- [x] Add `WriteConflict` variant to `StorageError`
- [x] `write_wal()` now uses `if_not_exists(true)` — fails loudly on duplicate WAL entries
- [x] Add `write_index_manifest_cas()` — uses `if_match(etag)` or `if_not_exists` for first write
- [x] Add `read_index_manifest_with_etag()` — returns `(IndexManifest, String)` with ETag
- [x] Falls back to unconditional write if backend doesn't support conditionals (Memory)

### Phase 2: Wire CAS into persist_index
- [x] Add `manifest_etag: Option<String>` to `NamespaceState`
- [x] `persist_index()` uses CAS: first write uses `if_not_exists`, subsequent use `if_match(prev_etag)`
- [x] `try_load_index_from_s3()` captures ETag from manifest read
- [x] All 67 existing tests still pass ✓

### Phase 3: Integration tests against Cloudflare R2
- [x] Add `ObjectStore::new_r2()` constructor (builds R2 endpoint from account ID)
- [x] Test: `test_r2_wal_exclusive_write` — write succeeds, duplicate fails with WriteConflict ✓
- [x] Test: `test_r2_manifest_cas` — CAS update works, stale etag rejected ✓
- [x] Test: `test_r2_full_persist_roundtrip` — full persist + CAS re-persist on R2 ✓
- [x] Tests use nanos-based namespace prefix for isolation, cleanup after run

---

## Progress Log

### Phases 1-2 — DONE
- WAL writes use `if_not_exists(true)` to prevent duplicate sequence numbers
- Manifest writes use CAS via `if_match(etag)` / `if_not_exists(true)`
- Falls back gracefully on backends that don't support conditionals (Memory)
- ETag tracked in NamespaceState, loaded from S3 manifest on startup

### Phase 3 — DONE
- All 3 R2 integration tests pass against Cloudflare R2
- WAL If-None-Match: * correctly rejects duplicate writes (412 → WriteConflict)
- Manifest If-Match CAS correctly rejects stale etag (412 → WriteConflict)
- Full persist_index + CAS re-persist roundtrip works on R2
- Run with: `source .env.local.cf && cargo test -p tpuf-server -- --ignored test_r2 --nocapture`
