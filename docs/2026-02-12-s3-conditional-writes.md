# S3 Conditional Write Guards for WAL and Index Safety

## Goal
Add S3 conditional writes (`If-None-Match`, `If-Match` headers) as safety guards for write operations on WAL and index files.

## Context
The current architecture assumes single-writer-per-namespace:
- 1 ingestor writes WAL entries (`{ns}/wal/{seq}.wal`)
- 1 indexer writes index files (`{ns}/index/*`)
- Query servers are read-only

With this constraint, no write conflicts occur. However, conditional writes provide fail-loud safety belts against:
1. Accidental double-indexer running on the same namespace
2. WAL sequence counter bugs silently overwriting entries
3. Indexer clobbering a newer manifest it didn't see

## Where to Add Guards

| Operation | Guard | Header | Effect |
|-----------|-------|--------|--------|
| WAL write (`wal/{seq}.wal`) | Prevent overwrite | `If-None-Match: *` | Fails if file already exists — catches sequence counter bugs |
| Index manifest write | CAS update | `If-Match: <etag>` | Fails if manifest changed since last read — catches double-indexer |
| Posting file write | Prevent overwrite | `If-None-Match: *` | Optional — posting IDs are unique per split |

## Implementation Notes
- OpenDAL supports conditional writes via `OpWrite::with_if_none_match()` and `OpWrite::with_if_match()`
- AWS S3 added conditional writes (If-None-Match) in August 2024
- MinIO supports conditional writes
- These should be **assertions** (fail loudly) not retry logic — single-writer is the design invariant
- On conflict: log error + abort operation, don't silently retry

## Relationship to Existing Work
- Depends on: SPFresh integration plan (docs/2026-02-12-spfresh-integration-plan.md)
- Affects: `crates/tpuf-core/src/storage/mod.rs` (once extracted), or current `crates/tpuf-server/src/storage/mod.rs`
- Related: OT-002 (SPFresh tests), v0 plan

## S3 Layout Reference
```
{ns}/wal/{seq:020}.wal           — WAL entries (ingestor writes, If-None-Match: *)
{ns}/index/manifest.bin          — index manifest (indexer writes, If-Match: <etag>)
{ns}/index/centroids.bin         — centroid data (indexer writes)
{ns}/index/postings/{hid}.bin    — posting lists (indexer writes)
{ns}/index/version_map.bin       — version snapshot (indexer writes)
```

## Single-Writer Architecture
```
Ingestor (1 per ns)  ──writes──→  wal/{seq}.wal     [If-None-Match: *]
Indexer  (1 per ns)  ──reads───→  wal/{seq}.wal
                     ──writes──→  index/manifest.bin [If-Match: <etag>]
                     ──writes──→  index/postings/*
                     ──writes──→  index/centroids.bin
                     ──writes──→  index/version_map.bin
Query Server (N)     ──reads───→  index/*            [read-only]
```
