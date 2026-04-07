use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;

use tpuf_server::engine::index::VectorIndex;
use tpuf_server::engine::index::spfresh::{
    SPFreshIndex, SPFreshConfig, HeadIndex, MemoryPostingStore, VersionMap, PostingStore,
};
use tpuf_server::engine::replay_into_index;
use tpuf_server::storage::ObjectStore;
use tpuf_server::types::IndexManifest;

pub async fn run_indexer(store: &ObjectStore, namespace: &str) -> anyhow::Result<()> {
    let meta = store.read_metadata(namespace).await?
        .ok_or_else(|| anyhow::anyhow!("namespace '{}' not found", namespace))?;

    let (manifest, prev_etag) = match store.read_index_manifest_with_etag(namespace).await? {
        Some((m, e)) => (Some(m), Some(e)),
        None => (None, None),
    };

    let manifest_seq = manifest.as_ref().map_or(0, |m| m.wal_sequence);

    let all_sequences = store.list_wal_sequences(namespace).await?;
    let new_seqs: Vec<u64> = all_sequences.iter()
        .filter(|&&s| s > manifest_seq)
        .copied()
        .collect();

    if new_seqs.is_empty() {
        info!(namespace, manifest_seq, "no new WAL entries — nothing to do");
        return Ok(());
    }

    info!(
        namespace,
        manifest_seq,
        new_entries = new_seqs.len(),
        "indexing WAL entries"
    );

    let dims = meta.dimensions
        .ok_or_else(|| anyhow::anyhow!("namespace '{}' has no dimensions set", namespace))?;
    let metric = meta.distance_metric;

    let mut index = if let Some(ref m) = manifest {
        load_index_from_s3(store, namespace, m).await?
    } else {
        let config = SPFreshConfig {
            dimensions: dims,
            distance_metric: metric,
            ..Default::default()
        };
        SPFreshIndex::new(config)
    };

    for seq in &new_seqs {
        if let Some(entry) = store.read_wal(namespace, *seq).await? {
            replay_into_index(&mut index, &entry);
        }
    }

    let new_seq = *new_seqs.last().unwrap();
    persist_index(store, namespace, &index, new_seq, prev_etag.as_deref()).await?;

    info!(
        namespace,
        old_seq = manifest_seq,
        new_seq,
        vectors = index.len(),
        "indexing complete"
    );

    Ok(())
}

async fn load_index_from_s3(
    store: &ObjectStore,
    namespace: &str,
    manifest: &IndexManifest,
) -> anyhow::Result<SPFreshIndex> {
    let centroids_file = store.read_centroids(namespace).await?
        .ok_or_else(|| anyhow::anyhow!("missing centroids for namespace '{}'", namespace))?;
    let vmap_file = store.read_version_map(namespace).await?
        .ok_or_else(|| anyhow::anyhow!("missing version_map for namespace '{}'", namespace))?;

    let hi = HeadIndex::from_file(centroids_file);
    let vm = VersionMap::from_file(vmap_file);
    let posting_store = MemoryPostingStore::new();

    for hid in 0..hi.next_id() {
        if hi.is_active(hid) {
            if let Some(pl) = store.read_posting(namespace, hid).await? {
                posting_store.put(hid, pl);
            }
        }
    }

    let index = SPFreshIndex::from_parts(
        manifest.config.clone(),
        hi,
        posting_store,
        vm,
        manifest.num_vectors as usize,
    );

    Ok(index)
}

async fn persist_index(
    store: &ObjectStore,
    namespace: &str,
    index: &SPFreshIndex,
    wal_sequence: u64,
    prev_etag: Option<&str>,
) -> anyhow::Result<()> {
    let hi = index.head_index().read().unwrap();

    for hid in 0..hi.next_id() {
        if hi.is_active(hid) {
            if let Some(posting) = index.posting_store().get(hid) {
                store.write_posting(namespace, hid, &posting).await?;
            }
        }
    }

    store.write_centroids(namespace, &hi.to_file()).await?;

    let vm = index.version_map().read().unwrap();
    store.write_version_map(namespace, &vm.to_file()).await?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    let manifest = IndexManifest {
        version: 1,
        wal_sequence,
        config: index.config().clone(),
        active_centroids: hi.len() as u32,
        num_vectors: index.len() as u64,
        created_at: now,
    };

    store.write_index_manifest_cas(namespace, &manifest, prev_etag).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tpuf_server::types::{Document, WalEntry, WriteOp, NamespaceMetadata, DistanceMetric};
    use std::collections::HashMap;

    fn make_docs(start: u64, count: u64, dims: usize) -> Vec<Document> {
        use rand::Rng;
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(start);
        (start..start + count)
            .map(|id| {
                let v: Vec<f32> = (0..dims).map(|_| rng.gen::<f32>() - 0.5).collect();
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                let v = if norm > 0.0 { v.iter().map(|x| x / norm).collect() } else { v };
                Document { id, vector: Some(v), attributes: HashMap::new() }
            })
            .collect()
    }

    async fn setup_namespace(store: &ObjectStore, ns: &str, dims: usize) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let meta = NamespaceMetadata {
            name: ns.to_string(),
            dimensions: Some(dims),
            distance_metric: DistanceMetric::EuclideanSquared,
            wal_sequence: 0,
            doc_count: 0,
            created_at: now,
        };
        store.write_metadata(&meta).await.unwrap();
    }

    async fn write_wal_batch(store: &ObjectStore, ns: &str, seq: u64, docs: Vec<Document>) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let entry = WalEntry {
            sequence: seq,
            timestamp_ms: now,
            operations: vec![WriteOp::Upsert(docs)],
        };
        store.write_wal(ns, &entry).await.unwrap();
    }

    #[tokio::test]
    async fn test_indexer_builds_from_scratch() {
        let store = ObjectStore::in_memory();
        let ns = "scratch-test";
        let dims = 32;

        setup_namespace(&store, ns, dims).await;

        let docs = make_docs(0, 200, dims);
        write_wal_batch(&store, ns, 1, docs).await;

        run_indexer(&store, ns).await.unwrap();

        let manifest = store.read_index_manifest(ns).await.unwrap();
        assert!(manifest.is_some(), "manifest should exist");
        let manifest = manifest.unwrap();
        assert_eq!(manifest.wal_sequence, 1);
        assert_eq!(manifest.num_vectors, 200);
        assert!(manifest.active_centroids > 0);

        let centroids = store.read_centroids(ns).await.unwrap();
        assert!(centroids.is_some(), "centroids should exist");
    }

    #[tokio::test]
    async fn test_indexer_incremental() {
        let store = ObjectStore::in_memory();
        let ns = "incr-test";
        let dims = 32;

        setup_namespace(&store, ns, dims).await;

        let docs1 = make_docs(0, 100, dims);
        write_wal_batch(&store, ns, 1, docs1).await;

        run_indexer(&store, ns).await.unwrap();

        let m1 = store.read_index_manifest(ns).await.unwrap().unwrap();
        assert_eq!(m1.wal_sequence, 1);
        assert_eq!(m1.num_vectors, 100);

        let docs2 = make_docs(100, 50, dims);
        write_wal_batch(&store, ns, 2, docs2).await;

        run_indexer(&store, ns).await.unwrap();

        let m2 = store.read_index_manifest(ns).await.unwrap().unwrap();
        assert_eq!(m2.wal_sequence, 2);
        assert_eq!(m2.num_vectors, 150);
    }

    #[tokio::test]
    async fn test_indexer_noop_when_caught_up() {
        let store = ObjectStore::in_memory();
        let ns = "noop-test";
        let dims = 32;

        setup_namespace(&store, ns, dims).await;

        let docs = make_docs(0, 50, dims);
        write_wal_batch(&store, ns, 1, docs).await;

        run_indexer(&store, ns).await.unwrap();
        let m1 = store.read_index_manifest(ns).await.unwrap().unwrap();

        run_indexer(&store, ns).await.unwrap();
        let m2 = store.read_index_manifest(ns).await.unwrap().unwrap();

        assert_eq!(m1.wal_sequence, m2.wal_sequence);
        assert_eq!(m1.num_vectors, m2.num_vectors);
    }

    #[tokio::test]
    async fn test_server_loads_indexer_output() {
        use tpuf_server::engine::NamespaceManager;

        let store = ObjectStore::in_memory();
        let ns = "load-test";
        let dims = 32;

        let mgr = NamespaceManager::new(store.clone()).await;
        mgr.create_namespace(ns.to_string(), DistanceMetric::EuclideanSquared).await.unwrap();
        let docs = make_docs(0, 500, dims);
        let query_vec = docs[0].vector.clone().unwrap();
        mgr.upsert(ns, docs).await.unwrap();
        drop(mgr);

        run_indexer(&store, ns).await.unwrap();

        let mgr2 = NamespaceManager::new(store.clone()).await;
        mgr2.init().await.unwrap();

        let idx_len = mgr2.index_len(ns).await.unwrap();
        assert_eq!(idx_len, 500, "server should load indexer's 500-vector index");

        let results = mgr2.query(ns, query_vec, 10, None, false).await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].id, 0, "nearest neighbor of doc 0 should be doc 0");
    }
}
