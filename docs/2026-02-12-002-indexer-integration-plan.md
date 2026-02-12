# tpuf Indexer Integration Plan (WAL -> SPFresh on S3)

> Date: 2026-02-12 | Status: DRAFT

## Goal

Keep `tpuf-server` focused on ingest + query serving, and move SPFresh index building into a separate `indexer` CLI process.

Your proposed flow is correct and aligns with the earlier v0/v1 direction:

`tpuf-server ingest -> WAL on MinIO/S3 -> indexer reads WAL -> builds/updates SPFresh clusters -> writes index artifacts to {ns}/index/`

## Target Object Layout

For each namespace:

- `{ns}/wal/{sequence}.wal` - append-only source of truth
- `{ns}/index/manifests/{version}.json` - active index metadata + pointers
- `{ns}/index/centroids/{version}.bin` - centroid table for head search
- `{ns}/index/clusters/{cluster_id}.bin` - cluster postings (vector ids + vectors/refs)
- `{ns}/index/state/checkpoint.json` - last indexed WAL sequence

## Component Split

1. `tpuf-server`:
- Validates writes and appends WAL batches.
- Serves query API.
- During query: reads latest manifest, loads centroids, selects top clusters, fetches cluster files, reranks.
- Applies WAL tail merge for sequences newer than checkpoint (freshness).

2. `tpuf-indexer` (new CLI/binary mode):
- Polls/scans namespaces for new WAL sequences.
- Replays WAL entries after checkpoint.
- Runs SPFresh update logic (insert/delete, split/reassign, centroid recompute).
- Writes new immutable index artifacts + manifest atomically (manifest-last publish).

## Query-Time Contract

- Query server always uses one manifest version as a consistent snapshot.
- If index lags ingest, query server also scans WAL tail `(checkpoint+1 .. latest)` and merges those docs at query time.
- This gives near-real-time correctness without blocking ingest on indexing.

## Implementation Phases (Short)

1. Storage contract
- Define manifest schema, centroid file schema, cluster file schema.
- Add `{ns}/index/state/checkpoint.json` handling.

2. Indexer CLI skeleton
- Add `tpuf indexer run` mode (or separate `tpuf-indexer` crate).
- Implement WAL replay loop per namespace.

3. SPFresh persistence wiring
- Reuse `engine/index/spfresh` structures.
- Persist centroids separately from cluster postings.
- Publish new manifest only after all files succeed.

4. Query integration
- Add manifest loader + centroid search + cluster fetch path.
- Add WAL tail merge path for unindexed writes.

5. Reliability
- Idempotent WAL replay (safe on retry/crash).
- Atomic publish via manifest-last + versioned artifacts.
- Metrics: index lag (latest_wal - checkpoint), publish duration, query tail-merge hit rate.

## Why This Design Makes Sense

- Clean separation of concerns: ingest availability is decoupled from indexing cost.
- S3/MinIO-native: immutable files + manifest pointer match object-store semantics.
- Fits SPFresh model: centroids file + separate cluster files maps naturally to head search then cluster scan.

## Open Decisions

- Single binary with role switch (`tpuf server` / `tpuf indexer`) vs separate crates.
- Cluster file payload: full vectors vs vector refs + fetch indirection.
- Polling model: periodic scan vs queue/notification driven indexing.
