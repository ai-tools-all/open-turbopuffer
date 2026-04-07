use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::info;

use crate::storage::{ObjectStore, CacheConfig, CachedPostingStore};
use crate::types::*;
use super::search::{brute_force_knn, merge_top_k};
use super::batcher::WriteBatcher;
use super::index::spfresh::{SPFreshIndex, SPFreshConfig, HeadIndex, VersionMap};
use super::index::spfresh::search::search_postings;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("storage: {0}")]
    Storage(#[from] crate::storage::StorageError),
    #[error("namespace '{0}' not found")]
    NotFound(String),
    #[error("namespace '{0}' already exists")]
    AlreadyExists(String),
    #[error("dimension mismatch: expected {expected}, got {got}")]
    DimensionMismatch { expected: usize, got: usize },
    #[error("no vector provided for search")]
    NoVector,
    #[error("{0}")]
    Validation(String),
}

struct IndexState {
    head_index: HeadIndex,
    version_map: VersionMap,
    config: SPFreshConfig,
    num_vectors: u64,
}

struct NamespaceState {
    metadata: NamespaceMetadata,
    documents: HashMap<u64, Document>,
    doc_sequences: HashMap<u64, u64>,
    index: Option<IndexState>,
    manifest_etag: Option<String>,
    indexed_up_to_seq: u64,
}

pub struct NamespaceManager {
    store: ObjectStore,
    posting_cache: CachedPostingStore,
    namespaces: RwLock<HashMap<String, Arc<RwLock<NamespaceState>>>>,
    batchers: RwLock<HashMap<String, Arc<WriteBatcher>>>,
}

impl NamespaceManager {
    pub async fn with_cache(store: ObjectStore, cache_config: &CacheConfig) -> anyhow::Result<Self> {
        let posting_cache = CachedPostingStore::new(store.clone(), cache_config).await?;
        Ok(Self {
            store,
            posting_cache,
            namespaces: RwLock::new(HashMap::new()),
            batchers: RwLock::new(HashMap::new()),
        })
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub async fn new(store: ObjectStore) -> Self {
        let posting_cache = CachedPostingStore::new_memory_only(store.clone(), 64 * 1024 * 1024)
            .await
            .expect("failed to create memory-only posting cache");
        Self {
            store,
            posting_cache,
            namespaces: RwLock::new(HashMap::new()),
            batchers: RwLock::new(HashMap::new()),
        }
    }

    pub async fn init(&self) -> Result<(), EngineError> {
        let ns_names = self.store.list_namespaces().await?;
        for name in ns_names {
            info!(namespace = %name, "replaying WAL");
            self.load_namespace(&name).await?;
        }
        Ok(())
    }

    async fn load_namespace(&self, name: &str) -> Result<(), EngineError> {
        let meta = self.store.read_metadata(name).await?
            .ok_or_else(|| EngineError::NotFound(name.to_string()))?;

        let mut documents = HashMap::new();
        let mut doc_sequences = HashMap::new();
        let sequences = self.store.list_wal_sequences(name).await?;
        for seq in &sequences {
            if let Some(entry) = self.store.read_wal(name, *seq).await? {
                apply_wal_entry(&mut documents, &mut doc_sequences, &entry);
            }
        }

        let doc_count = documents.len() as u64;
        let wal_sequence = sequences.last().copied().unwrap_or(0);

        let (index, manifest_etag, indexed_up_to_seq) = match self.try_load_index_from_s3(name).await {
            Ok(Some((idx_state, etag, manifest_seq))) => {
                info!(namespace = %name, manifest_seq, num_vectors = idx_state.num_vectors, "loaded index from S3 manifest");
                (Some(idx_state), Some(etag), manifest_seq)
            }
            Ok(None) => {
                info!(namespace = %name, "no manifest found — running without index");
                (None, None, 0)
            }
            Err(e) => {
                info!(namespace = %name, error = %e, "failed to load index — running without index");
                (None, None, 0)
            }
        };

        let state = NamespaceState {
            metadata: NamespaceMetadata {
                doc_count,
                wal_sequence,
                ..meta
            },
            documents,
            doc_sequences,
            index,
            manifest_etag,
            indexed_up_to_seq,
        };

        info!(
            namespace = %name,
            docs = doc_count,
            wal_entries = sequences.len(),
            index_vectors = state.index.as_ref().map_or(0, |i| i.num_vectors),
            "namespace loaded"
        );

        let arc_state = Arc::new(RwLock::new(state));
        self.namespaces.write().await.insert(name.to_string(), arc_state);

        let batcher = Arc::new(WriteBatcher::new(name.to_string()));
        self.batchers.write().await.insert(name.to_string(), batcher);

        Ok(())
    }

    async fn try_load_index_from_s3(
        &self,
        name: &str,
    ) -> Result<Option<(IndexState, String, u64)>, EngineError> {
        let (manifest, etag) = match self.store.read_index_manifest_with_etag(name).await? {
            Some((m, e)) => (m, e),
            None => return Ok(None),
        };

        let centroids_file = match self.store.read_centroids(name).await? {
            Some(cf) => cf,
            None => return Ok(None),
        };
        let vmap_file = match self.store.read_version_map(name).await? {
            Some(vf) => vf,
            None => return Ok(None),
        };

        let hi = HeadIndex::from_file(centroids_file);
        let vm = VersionMap::from_file(vmap_file);
        let manifest_seq = manifest.wal_sequence;

        let index_state = IndexState {
            head_index: hi,
            version_map: vm,
            config: manifest.config,
            num_vectors: manifest.num_vectors,
        };

        Ok(Some((index_state, etag, manifest_seq)))
    }

    pub async fn create_namespace(
        &self,
        name: String,
        distance_metric: DistanceMetric,
    ) -> Result<(), EngineError> {
        {
            let ns = self.namespaces.read().await;
            if ns.contains_key(&name) {
                return Err(EngineError::AlreadyExists(name));
            }
        }

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let meta = NamespaceMetadata {
            name: name.clone(),
            dimensions: None,
            distance_metric,
            wal_sequence: 0,
            doc_count: 0,
            created_at: now,
        };
        self.store.write_metadata(&meta).await?;

        let state = NamespaceState {
            metadata: meta,
            documents: HashMap::new(),
            doc_sequences: HashMap::new(),
            index: None,
            manifest_etag: None,
            indexed_up_to_seq: 0,
        };

        let arc_state = Arc::new(RwLock::new(state));
        self.namespaces.write().await.insert(name.clone(), arc_state);

        let batcher = Arc::new(WriteBatcher::new(name.clone()));
        self.batchers.write().await.insert(name, batcher);

        Ok(())
    }

    pub async fn list_namespaces(&self) -> Vec<NamespaceMetadata> {
        let ns = self.namespaces.read().await;
        let mut result = Vec::new();
        for state in ns.values() {
            let s = state.read().await;
            result.push(s.metadata.clone());
        }
        result
    }

    pub async fn delete_namespace(&self, name: &str) -> Result<(), EngineError> {
        {
            let ns = self.namespaces.read().await;
            if !ns.contains_key(name) {
                return Err(EngineError::NotFound(name.to_string()));
            }
        }

        self.store.delete_namespace(name).await?;
        self.namespaces.write().await.remove(name);
        self.batchers.write().await.remove(name);
        Ok(())
    }

    pub async fn upsert(
        &self,
        ns: &str,
        documents: Vec<Document>,
    ) -> Result<(), EngineError> {
        if documents.is_empty() {
            return Ok(());
        }

        let batcher = {
            let batchers = self.batchers.read().await;
            batchers.get(ns)
                .ok_or_else(|| EngineError::NotFound(ns.to_string()))?
                .clone()
        };

        let ops = vec![WriteOp::Upsert(documents)];
        batcher.submit(ops).await;

        self.flush_batcher(ns, &batcher).await
    }

    pub async fn delete_docs(
        &self,
        ns: &str,
        ids: Vec<u64>,
    ) -> Result<(), EngineError> {
        if ids.is_empty() {
            return Ok(());
        }

        let batcher = {
            let batchers = self.batchers.read().await;
            batchers.get(ns)
                .ok_or_else(|| EngineError::NotFound(ns.to_string()))?
                .clone()
        };

        let ops = vec![WriteOp::Delete(ids)];
        batcher.submit(ops).await;

        self.flush_batcher(ns, &batcher).await
    }

    async fn flush_batcher(&self, ns: &str, batcher: &WriteBatcher) -> Result<(), EngineError> {
        let ops = batcher.flush().await;
        if ops.is_empty() {
            return Ok(());
        }

        let ns_state = {
            let namespaces = self.namespaces.read().await;
            namespaces.get(ns)
                .ok_or_else(|| EngineError::NotFound(ns.to_string()))?
                .clone()
        };

        let mut state = ns_state.write().await;

        for op in &ops {
            if let WriteOp::Upsert(docs) = op {
                for doc in docs {
                    if let Some(vec) = &doc.vector {
                        match state.metadata.dimensions {
                            None => {
                                state.metadata.dimensions = Some(vec.len());
                            }
                            Some(dim) if dim != vec.len() => {
                                return Err(EngineError::DimensionMismatch {
                                    expected: dim,
                                    got: vec.len(),
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        let seq = state.metadata.wal_sequence + 1;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;

        let entry = WalEntry {
            sequence: seq,
            timestamp_ms: now,
            operations: ops,
        };

        self.store.write_wal(ns, &entry).await?;
        {
            let s = &mut *state;
            apply_wal_entry(&mut s.documents, &mut s.doc_sequences, &entry);
        }

        state.metadata.wal_sequence = seq;
        state.metadata.doc_count = state.documents.len() as u64;
        self.store.write_metadata(&state.metadata).await?;

        Ok(())
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub async fn persist_index(&self, ns: &str) -> Result<(), EngineError> {
        use super::index::VectorIndex;
        use super::index::spfresh::PostingStore;

        let ns_state = {
            let namespaces = self.namespaces.read().await;
            namespaces.get(ns)
                .ok_or_else(|| EngineError::NotFound(ns.to_string()))?
                .clone()
        };
        let state = ns_state.read().await;

        let dims = state.metadata.dimensions.unwrap_or(0);
        let config = state.index.as_ref()
            .map(|idx| idx.config.clone())
            .unwrap_or_else(|| SPFreshConfig {
                dimensions: dims,
                distance_metric: state.metadata.distance_metric,
                ..Default::default()
            });

        let idx = SPFreshIndex::new(config);
        for doc in state.documents.values() {
            if let Some(vec) = &doc.vector {
                let _ = idx.insert(doc.id, vec);
            }
        }

        let hi = idx.head_index().read().unwrap();
        for hid in 0..hi.next_id() {
            if hi.is_active(hid) {
                if let Some(posting) = idx.posting_store().get(hid) {
                    self.store.write_posting(ns, hid, &posting).await?;
                }
            }
        }
        self.store.write_centroids(ns, &hi.to_file()).await?;
        let vm = idx.version_map().read().unwrap();
        self.store.write_version_map(ns, &vm.to_file()).await?;

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let manifest = IndexManifest {
            version: 1,
            wal_sequence: state.metadata.wal_sequence,
            config: idx.config().clone(),
            active_centroids: hi.len() as u32,
            num_vectors: idx.len() as u64,
            created_at: now,
        };
        drop(hi);
        drop(vm);
        drop(state);

        let mut state = ns_state.write().await;
        let prev_etag = state.manifest_etag.clone();
        let new_etag = self.store.write_index_manifest_cas(ns, &manifest, prev_etag.as_deref()).await?;
        state.manifest_etag = Some(new_etag);

        info!(
            namespace = %ns,
            vectors = manifest.num_vectors,
            centroids = manifest.active_centroids,
            wal_seq = manifest.wal_sequence,
            "index persisted to S3"
        );

        Ok(())
    }

    pub async fn index_len(&self, ns: &str) -> Result<usize, EngineError> {
        let namespaces = self.namespaces.read().await;
        let ns_state = namespaces.get(ns)
            .ok_or_else(|| EngineError::NotFound(ns.to_string()))?;
        let state = ns_state.read().await;
        Ok(state.index.as_ref().map_or(0, |idx| idx.num_vectors as usize))
    }

    pub async fn get_document(&self, ns: &str, id: u64) -> Result<Option<Document>, EngineError> {
        let namespaces = self.namespaces.read().await;
        let ns_state = namespaces.get(ns)
            .ok_or_else(|| EngineError::NotFound(ns.to_string()))?;
        let state = ns_state.read().await;
        Ok(state.documents.get(&id).cloned())
    }

    pub async fn query(
        &self,
        ns: &str,
        vector: Vec<f32>,
        top_k: usize,
        distance_metric: Option<DistanceMetric>,
        include_vectors: bool,
    ) -> Result<Vec<QueryResult>, EngineError> {
        let namespaces = self.namespaces.read().await;
        let ns_state = namespaces.get(ns)
            .ok_or_else(|| EngineError::NotFound(ns.to_string()))?;
        let state = ns_state.read().await;

        if let Some(dim) = state.metadata.dimensions {
            if vector.len() != dim {
                return Err(EngineError::DimensionMismatch {
                    expected: dim,
                    got: vector.len(),
                });
            }
        }

        let metric = distance_metric.unwrap_or(state.metadata.distance_metric);

        let scored = if let Some(idx_state) = &state.index {
            let ann_results = if idx_state.num_vectors > 0 {
                let head_ids: Vec<u32> = idx_state.head_index
                    .search(&vector, idx_state.config.num_search_heads, idx_state.config.distance_metric)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();

                let postings = self.posting_cache.get_multi(ns, &head_ids).await;
                search_postings(&vector, &postings, &idx_state.version_map, idx_state.config.distance_metric, top_k)
            } else {
                Vec::new()
            };

            let tail_vectors: Vec<(u64, &[f32])> = state.documents.iter()
                .filter(|(id, _)| {
                    state.doc_sequences.get(id)
                        .map_or(false, |&seq| seq > state.indexed_up_to_seq)
                })
                .filter_map(|(id, doc)| doc.vector.as_ref().map(|v| (*id, v.as_slice())))
                .collect();

            if tail_vectors.is_empty() {
                info!(
                    namespace = %ns,
                    ann_hits = ann_results.len(),
                    tail_size = 0,
                    "query: index only (no tail)"
                );
                ann_results
            } else {
                let tail_ids: std::collections::HashSet<u64> =
                    tail_vectors.iter().map(|(id, _)| *id).collect();
                let ann_filtered: Vec<(u64, f32)> = ann_results.into_iter()
                    .filter(|(id, _)| !tail_ids.contains(id))
                    .collect();
                let tail_results = brute_force_knn(&vector, &tail_vectors, metric, top_k);
                let merged = merge_top_k(&ann_filtered, &tail_results, top_k);

                let from_ann: Vec<u64> = merged.iter()
                    .filter(|(id, _)| !tail_ids.contains(id))
                    .map(|(id, _)| *id)
                    .collect();
                let from_tail: Vec<u64> = merged.iter()
                    .filter(|(id, _)| tail_ids.contains(id))
                    .map(|(id, _)| *id)
                    .collect();
                info!(
                    namespace = %ns,
                    tail_size = tail_vectors.len(),
                    from_index = from_ann.len(),
                    from_tail = from_tail.len(),
                    index_ids = ?from_ann,
                    tail_ids = ?from_tail,
                    "query: hybrid (ANN + brute-force tail)"
                );
                merged
            }
        } else {
            let vectors: Vec<(u64, &[f32])> = state.documents.iter()
                .filter_map(|(id, doc)| {
                    doc.vector.as_ref().map(|v| (*id, v.as_slice()))
                })
                .collect();
            info!(
                namespace = %ns,
                brute_force_size = vectors.len(),
                "query: full brute-force (no index)"
            );
            brute_force_knn(&vector, &vectors, metric, top_k)
        };

        let query_results: Vec<QueryResult> = scored.into_iter()
            .filter_map(|(id, dist)| {
                state.documents.get(&id).map(|doc| QueryResult {
                    id,
                    dist,
                    vector: if include_vectors { doc.vector.clone() } else { None },
                    attributes: doc.attributes.clone(),
                })
            })
            .collect();

        Ok(query_results)
    }

    pub async fn reload_indexes_if_changed(&self) {
        let ns_names: Vec<String> = {
            let ns = self.namespaces.read().await;
            ns.keys().cloned().collect()
        };

        for name in ns_names {
            if let Err(e) = self.try_reload_index(&name).await {
                info!(namespace = %name, error = %e, "index reload check failed");
            }
        }
    }

    async fn try_reload_index(&self, name: &str) -> Result<(), EngineError> {
        let current_etag = {
            let namespaces = self.namespaces.read().await;
            let ns_state = match namespaces.get(name) {
                Some(s) => s.clone(),
                None => return Ok(()),
            };
            let state = ns_state.read().await;
            state.manifest_etag.clone()
        };

        let s3_etag = match self.store.read_index_manifest_with_etag(name).await? {
            Some((_, etag)) => Some(etag),
            None => None,
        };

        let changed = match (&current_etag, &s3_etag) {
            (Some(cur), Some(s3)) => cur != s3,
            (None, Some(_)) => true,
            _ => false,
        };

        if !changed {
            return Ok(());
        }

        let (idx_state, etag, manifest_seq) = match self.try_load_index_from_s3(name).await? {
            Some(result) => result,
            None => return Ok(()),
        };

        let namespaces = self.namespaces.read().await;
        let ns_state = match namespaces.get(name) {
            Some(s) => s.clone(),
            None => return Ok(()),
        };
        let mut state = ns_state.write().await;

        let old_seq = state.indexed_up_to_seq;
        let num_vectors = idx_state.num_vectors;
        state.index = Some(idx_state);
        state.manifest_etag = Some(etag);
        state.indexed_up_to_seq = manifest_seq;

        info!(
            namespace = %name,
            old_seq,
            new_seq = manifest_seq,
            num_vectors,
            "hot-reloaded index from S3"
        );

        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct QueryResult {
    pub id: u64,
    pub dist: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    pub attributes: HashMap<String, AttributeValue>,
}

pub fn replay_into_index(index: &mut SPFreshIndex, entry: &WalEntry) {
    use super::index::VectorIndex;
    for op in &entry.operations {
        match op {
            WriteOp::Upsert(docs) => {
                for doc in docs {
                    if let Some(vec) = &doc.vector {
                        let _ = index.insert(doc.id, vec);
                    }
                }
            }
            WriteOp::Delete(ids) => {
                for &id in ids {
                    let _ = index.delete(id);
                }
            }
        }
    }
}

pub fn apply_wal_entry(
    documents: &mut HashMap<u64, Document>,
    doc_sequences: &mut HashMap<u64, u64>,
    entry: &WalEntry,
) {
    for op in &entry.operations {
        match op {
            WriteOp::Upsert(docs) => {
                for doc in docs {
                    documents.insert(doc.id, doc.clone());
                    doc_sequences.insert(doc.id, entry.sequence);
                }
            }
            WriteOp::Delete(ids) => {
                for id in ids {
                    documents.remove(id);
                    doc_sequences.remove(id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_doc(id: u64, vector: Vec<f32>) -> Document {
        Document {
            id,
            vector: Some(vector),
            attributes: HashMap::new(),
        }
    }

    fn make_docs(start: u64, count: u64, dims: usize) -> Vec<Document> {
        use rand::Rng;
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(start);
        (start..start + count)
            .map(|id| {
                let v: Vec<f32> = (0..dims).map(|_| rng.gen::<f32>() - 0.5).collect();
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                let v = if norm > 0.0 { v.iter().map(|x| x / norm).collect() } else { v };
                make_doc(id, v)
            })
            .collect()
    }

    async fn test_manager() -> NamespaceManager {
        let store = crate::storage::ObjectStore::in_memory();
        let mgr = NamespaceManager::new(store).await;
        mgr.create_namespace("test".into(), DistanceMetric::EuclideanSquared).await.unwrap();
        mgr
    }

    async fn build_and_persist_test_index(store: &crate::storage::ObjectStore, ns: &str) {
        use crate::engine::index::VectorIndex;
        use crate::engine::index::spfresh::PostingStore;
        use std::time::{SystemTime, UNIX_EPOCH};

        let meta = store.read_metadata(ns).await.unwrap().unwrap();
        let sequences = store.list_wal_sequences(ns).await.unwrap();

        let dims = meta.dimensions.unwrap();
        let config = SPFreshConfig {
            dimensions: dims,
            distance_metric: meta.distance_metric,
            ..Default::default()
        };
        let mut idx = SPFreshIndex::new(config);

        let mut wal_seq = 0u64;
        for seq in &sequences {
            if let Some(entry) = store.read_wal(ns, *seq).await.unwrap() {
                replay_into_index(&mut idx, &entry);
                wal_seq = entry.sequence;
            }
        }

        let hi = idx.head_index().read().unwrap();
        for hid in 0..hi.next_id() {
            if hi.is_active(hid) {
                if let Some(posting) = idx.posting_store().get(hid) {
                    store.write_posting(ns, hid, &posting).await.unwrap();
                }
            }
        }
        store.write_centroids(ns, &hi.to_file()).await.unwrap();
        let vm = idx.version_map().read().unwrap();
        store.write_version_map(ns, &vm.to_file()).await.unwrap();

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let manifest = IndexManifest {
            version: 1,
            wal_sequence: wal_seq,
            config: idx.config().clone(),
            active_centroids: hi.len() as u32,
            num_vectors: idx.len() as u64,
            created_at: now,
        };
        drop(hi);
        drop(vm);

        store.write_index_manifest(ns, &manifest).await.unwrap();
    }

    #[tokio::test]
    async fn test_upsert_queries_via_brute_force() {
        let mgr = test_manager().await;
        let docs = make_docs(0, 100, 32);
        let query_vec = docs[0].vector.clone().unwrap();
        mgr.upsert("test", docs).await.unwrap();

        let idx_len = mgr.index_len("test").await.unwrap();
        assert_eq!(idx_len, 0, "no inline indexing — index should be empty");

        let results = mgr.query("test", query_vec, 10, None, false).await.unwrap();
        assert!(!results.is_empty(), "brute-force should return results");
        assert_eq!(results[0].id, 0, "nearest neighbor of doc 0 should be itself");
    }

    #[tokio::test]
    async fn test_delete_excluded_from_queries() {
        let mgr = test_manager().await;
        let docs = make_docs(0, 100, 32);
        mgr.upsert("test", docs.clone()).await.unwrap();

        let ids_to_delete: Vec<u64> = (0..50).collect();
        mgr.delete_docs("test", ids_to_delete.clone()).await.unwrap();

        for &id in &[0u64, 25, 49] {
            let qv = docs[id as usize].vector.clone().unwrap();
            let results = mgr.query("test", qv, 10, None, false).await.unwrap();
            let result_ids: Vec<u64> = results.iter().map(|r| r.id).collect();
            assert!(!result_ids.contains(&id), "deleted doc {id} should not appear in results");
        }
    }

    #[tokio::test]
    async fn test_query_uses_index() {
        let mgr = test_manager().await;
        let docs = make_docs(0, 200, 32);
        let query_vec = docs[0].vector.clone().unwrap();
        mgr.upsert("test", docs).await.unwrap();

        let results = mgr.query("test", query_vec, 10, None, false).await.unwrap();
        assert!(!results.is_empty(), "query should return results");
        assert_eq!(results[0].id, 0, "nearest neighbor of doc 0 should be doc 0 itself");
    }

    #[tokio::test]
    async fn test_query_deleted_not_returned() {
        let mgr = test_manager().await;
        let docs = make_docs(0, 100, 32);
        let query_vec = docs[0].vector.clone().unwrap();
        mgr.upsert("test", docs).await.unwrap();

        mgr.delete_docs("test", vec![0]).await.unwrap();

        let results = mgr.query("test", query_vec, 10, None, false).await.unwrap();
        let ids: Vec<u64> = results.iter().map(|r| r.id).collect();
        assert!(!ids.contains(&0), "deleted doc 0 should not appear in results");
    }

    #[tokio::test]
    async fn test_fallback_to_brute_force_empty() {
        let store = crate::storage::ObjectStore::in_memory();
        let mgr = NamespaceManager::new(store).await;
        mgr.create_namespace("empty".into(), DistanceMetric::EuclideanSquared).await.unwrap();

        let q = vec![0.0f32; 32];
        let results = mgr.query("empty", q, 10, None, false).await.unwrap();
        assert!(results.is_empty(), "empty namespace should return no results");
    }

    #[tokio::test]
    async fn test_recall_through_namespace_manager() {
        use crate::engine::search::brute_force_knn;

        let dims = 128;
        let n = 5000u64;
        let k = 10;
        let n_queries = 50;

        let mgr = test_manager().await;
        let docs = make_docs(0, n, dims);

        let vectors: Vec<Vec<f32>> = docs.iter()
            .map(|d| d.vector.clone().unwrap())
            .collect();

        for chunk in docs.chunks(500) {
            mgr.upsert("test", chunk.to_vec()).await.unwrap();
        }

        let queries = make_docs(n, n_queries, dims);
        let indexed: Vec<(u64, &[f32])> = vectors.iter()
            .enumerate()
            .map(|(i, v)| (i as u64, v.as_slice()))
            .collect();

        let mut total_recall = 0.0;
        for q_doc in &queries {
            let qv = q_doc.vector.as_ref().unwrap();
            let gt = brute_force_knn(qv, &indexed, DistanceMetric::EuclideanSquared, k);
            let gt_ids: std::collections::HashSet<u64> = gt.iter().map(|(id, _)| *id).collect();

            let results = mgr.query("test", qv.clone(), k, None, false).await.unwrap();
            let result_ids: std::collections::HashSet<u64> = results.iter().map(|r| r.id).collect();

            let overlap = gt_ids.intersection(&result_ids).count();
            total_recall += overlap as f64 / k as f64;
        }

        let avg_recall = total_recall / n_queries as f64;
        println!("E2E recall@{k}: {avg_recall:.4} (n={n}, queries={n_queries}, dims={dims})");
        assert!(avg_recall > 0.85, "E2E recall@{k} = {avg_recall:.4}, expected > 0.85");
    }

    #[tokio::test]
    async fn test_mixed_ops_recall() {
        use crate::engine::search::brute_force_knn;

        let dims = 32;
        let k = 10;

        let mgr = test_manager().await;

        let initial_docs = make_docs(0, 500, dims);
        mgr.upsert("test", initial_docs.clone()).await.unwrap();

        let delete_ids: Vec<u64> = (0..100).collect();
        mgr.delete_docs("test", delete_ids).await.unwrap();

        let new_docs = make_docs(500, 200, dims);
        mgr.upsert("test", new_docs.clone()).await.unwrap();

        let remaining: Vec<(u64, Vec<f32>)> = initial_docs.iter()
            .filter(|d| d.id >= 100)
            .chain(new_docs.iter())
            .map(|d| (d.id, d.vector.clone().unwrap()))
            .collect();
        let indexed: Vec<(u64, &[f32])> = remaining.iter()
            .map(|(id, v)| (*id, v.as_slice()))
            .collect();

        let queries = make_docs(1000, 20, dims);
        let mut total_recall = 0.0;
        for q_doc in &queries {
            let qv = q_doc.vector.as_ref().unwrap();
            let gt = brute_force_knn(qv, &indexed, DistanceMetric::EuclideanSquared, k);
            let gt_ids: std::collections::HashSet<u64> = gt.iter().map(|(id, _)| *id).collect();

            let results = mgr.query("test", qv.clone(), k, None, false).await.unwrap();
            let result_ids: std::collections::HashSet<u64> = results.iter().map(|r| r.id).collect();

            let overlap = gt_ids.intersection(&result_ids).count();
            total_recall += overlap as f64 / k as f64;
        }

        let avg_recall = total_recall / 20.0;
        println!("Mixed ops recall@{k}: {avg_recall:.4}");
        assert!(avg_recall > 0.80, "Mixed ops recall@{k} = {avg_recall:.4}, expected > 0.80");
    }

    #[tokio::test]
    async fn test_persist_index() {
        let store = crate::storage::ObjectStore::in_memory();

        {
            let mgr = NamespaceManager::new(store.clone()).await;
            mgr.create_namespace("test".into(), DistanceMetric::EuclideanSquared).await.unwrap();
            let docs = make_docs(0, 200, 32);
            mgr.upsert("test", docs).await.unwrap();
        }

        build_and_persist_test_index(&store, "test").await;

        let manifest = store.read_index_manifest("test").await.unwrap();
        assert!(manifest.is_some(), "manifest should exist after persist");
        let manifest = manifest.unwrap();
        assert_eq!(manifest.num_vectors, 200);
        assert!(manifest.active_centroids > 0);

        let centroids = store.read_centroids("test").await.unwrap();
        assert!(centroids.is_some(), "centroids file should exist");

        let vmap = store.read_version_map("test").await.unwrap();
        assert!(vmap.is_some(), "version_map file should exist");
    }

    #[tokio::test]
    async fn test_persist_and_load_roundtrip() {
        use crate::engine::search::brute_force_knn;

        let dims = 32;
        let k = 10;
        let store = crate::storage::ObjectStore::in_memory();

        let docs = make_docs(0, 1000, dims);
        let vectors: Vec<Vec<f32>> = docs.iter()
            .map(|d| d.vector.clone().unwrap())
            .collect();

        {
            let mgr = NamespaceManager::new(store.clone()).await;
            mgr.create_namespace("test".into(), DistanceMetric::EuclideanSquared).await.unwrap();
            mgr.upsert("test", docs).await.unwrap();
        }

        build_and_persist_test_index(&store, "test").await;

        {
            let mgr2 = NamespaceManager::new(store.clone()).await;
            mgr2.init().await.unwrap();

            let idx_len = mgr2.index_len("test").await.unwrap();
            assert_eq!(idx_len, 1000, "loaded index should have 1000 vectors, got {idx_len}");

            let queries = make_docs(2000, 20, dims);
            let indexed: Vec<(u64, &[f32])> = vectors.iter()
                .enumerate()
                .map(|(i, v)| (i as u64, v.as_slice()))
                .collect();

            let mut total_recall = 0.0;
            for q_doc in &queries {
                let qv = q_doc.vector.as_ref().unwrap();
                let gt = brute_force_knn(qv, &indexed, DistanceMetric::EuclideanSquared, k);
                let gt_ids: std::collections::HashSet<u64> = gt.iter().map(|(id, _)| *id).collect();

                let results = mgr2.query("test", qv.clone(), k, None, false).await.unwrap();
                let result_ids: std::collections::HashSet<u64> = results.iter().map(|r| r.id).collect();

                let overlap = gt_ids.intersection(&result_ids).count();
                total_recall += overlap as f64 / k as f64;
            }

            let avg_recall = total_recall / 20.0;
            println!("Persist-load recall@{k}: {avg_recall:.4}");
            assert!(avg_recall > 0.85, "Persist-load recall@{k} = {avg_recall:.4}, expected > 0.85");
        }
    }

    #[tokio::test]
    async fn test_indexer_then_more_writes_hybrid() {
        let store = crate::storage::ObjectStore::in_memory();

        {
            let mgr = NamespaceManager::new(store.clone()).await;
            mgr.create_namespace("test".into(), DistanceMetric::EuclideanSquared).await.unwrap();
            let docs = make_docs(0, 500, 32);
            mgr.upsert("test", docs).await.unwrap();
        }

        build_and_persist_test_index(&store, "test").await;

        let mgr = NamespaceManager::new(store.clone()).await;
        mgr.init().await.unwrap();

        let idx_len = mgr.index_len("test").await.unwrap();
        assert_eq!(idx_len, 500, "index should have 500 vectors from manifest");

        let more_docs = make_docs(500, 200, 32);
        let tail_query = more_docs[0].vector.clone().unwrap();
        mgr.upsert("test", more_docs).await.unwrap();

        let results = mgr.query("test", tail_query, 10, None, false).await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].id, 500, "tail doc 500 found via hybrid query");
    }

    #[tokio::test]
    async fn test_no_manifest_runs_without_index() {
        let store = crate::storage::ObjectStore::in_memory();

        {
            let mgr = NamespaceManager::new(store.clone()).await;
            mgr.create_namespace("test".into(), DistanceMetric::EuclideanSquared).await.unwrap();
            let docs = make_docs(0, 300, 32);
            mgr.upsert("test", docs).await.unwrap();
        }

        {
            let mgr2 = NamespaceManager::new(store.clone()).await;
            mgr2.init().await.unwrap();

            let idx_len = mgr2.index_len("test").await.unwrap();
            assert_eq!(idx_len, 0, "no manifest means no index");

            let query = make_docs(0, 1, 32)[0].vector.clone().unwrap();
            let results = mgr2.query("test", query, 10, None, false).await.unwrap();
            assert!(!results.is_empty(), "brute-force should still return results");
            assert_eq!(results[0].id, 0);
        }
    }

    #[tokio::test]
    async fn test_hybrid_query_finds_unindexed() {
        let store = crate::storage::ObjectStore::in_memory();
        let dims = 32;

        {
            let mgr = NamespaceManager::new(store.clone()).await;
            mgr.create_namespace("test".into(), DistanceMetric::EuclideanSquared).await.unwrap();
            let docs = make_docs(0, 100, dims);
            mgr.upsert("test", docs).await.unwrap();
        }

        build_and_persist_test_index(&store, "test").await;

        let mgr = NamespaceManager::new(store.clone()).await;
        mgr.init().await.unwrap();

        let tail_docs = make_docs(100, 50, dims);
        let tail_query = tail_docs[0].vector.clone().unwrap();
        mgr.upsert("test", tail_docs).await.unwrap();

        let results = mgr.query("test", tail_query, 10, None, false).await.unwrap();
        assert!(!results.is_empty(), "should return results");
        assert_eq!(results[0].id, 100, "nearest neighbor of tail doc 100 should be itself");

        let indexed_query = make_docs(0, 1, dims)[0].vector.clone().unwrap();
        let results = mgr.query("test", indexed_query, 10, None, false).await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].id, 0, "should find indexed doc 0");
    }

    #[tokio::test]
    async fn test_hybrid_query_recall() {
        use crate::engine::search::brute_force_knn;

        let store = crate::storage::ObjectStore::in_memory();
        let dims = 32;
        let k = 10;

        let all_docs = make_docs(0, 1200, dims);
        let indexed_docs: Vec<Document> = all_docs[..1000].to_vec();
        let tail_docs: Vec<Document> = all_docs[1000..].to_vec();

        {
            let mgr = NamespaceManager::new(store.clone()).await;
            mgr.create_namespace("test".into(), DistanceMetric::EuclideanSquared).await.unwrap();
            mgr.upsert("test", indexed_docs).await.unwrap();
        }

        build_and_persist_test_index(&store, "test").await;

        let mgr = NamespaceManager::new(store.clone()).await;
        mgr.init().await.unwrap();
        mgr.upsert("test", tail_docs).await.unwrap();

        let all_vectors: Vec<(u64, Vec<f32>)> = all_docs.iter()
            .map(|d| (d.id, d.vector.clone().unwrap()))
            .collect();
        let indexed: Vec<(u64, &[f32])> = all_vectors.iter()
            .map(|(id, v)| (*id, v.as_slice()))
            .collect();

        let queries = make_docs(5000, 50, dims);
        let mut total_recall = 0.0;
        for q_doc in &queries {
            let qv = q_doc.vector.as_ref().unwrap();
            let gt = brute_force_knn(qv, &indexed, DistanceMetric::EuclideanSquared, k);
            let gt_ids: std::collections::HashSet<u64> = gt.iter().map(|(id, _)| *id).collect();

            let results = mgr.query("test", qv.clone(), k, None, false).await.unwrap();
            let result_ids: std::collections::HashSet<u64> = results.iter().map(|r| r.id).collect();

            let overlap = gt_ids.intersection(&result_ids).count();
            total_recall += overlap as f64 / k as f64;
        }

        let avg_recall = total_recall / 50.0;
        println!("Hybrid recall@{k}: {avg_recall:.4} (1000 indexed + 200 tail)");
        assert!(avg_recall > 0.85, "Hybrid recall@{k} = {avg_recall:.4}, expected > 0.85");
    }

    #[tokio::test]
    async fn test_fully_indexed_no_tail_scan() {
        let store = crate::storage::ObjectStore::in_memory();
        let dims = 32;

        {
            let mgr = NamespaceManager::new(store.clone()).await;
            mgr.create_namespace("test".into(), DistanceMetric::EuclideanSquared).await.unwrap();
            let docs = make_docs(0, 200, dims);
            mgr.upsert("test", docs).await.unwrap();
        }

        build_and_persist_test_index(&store, "test").await;

        let mgr = NamespaceManager::new(store.clone()).await;
        mgr.init().await.unwrap();

        let idx_len = mgr.index_len("test").await.unwrap();
        assert_eq!(idx_len, 200, "all docs should be in index from manifest");

        let query = make_docs(0, 1, dims)[0].vector.clone().unwrap();
        let results = mgr.query("test", query, 10, None, false).await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].id, 0);
    }

    #[tokio::test]
    async fn test_delete_in_tail() {
        let store = crate::storage::ObjectStore::in_memory();
        let dims = 32;

        {
            let mgr = NamespaceManager::new(store.clone()).await;
            mgr.create_namespace("test".into(), DistanceMetric::EuclideanSquared).await.unwrap();
            let docs = make_docs(0, 100, dims);
            mgr.upsert("test", docs).await.unwrap();
        }

        build_and_persist_test_index(&store, "test").await;

        let mgr = NamespaceManager::new(store.clone()).await;
        mgr.init().await.unwrap();

        let tail_docs = make_docs(100, 50, dims);
        let tail_query = tail_docs[0].vector.clone().unwrap();
        mgr.upsert("test", tail_docs).await.unwrap();

        mgr.delete_docs("test", vec![100]).await.unwrap();

        let results = mgr.query("test", tail_query, 10, None, false).await.unwrap();
        let ids: Vec<u64> = results.iter().map(|r| r.id).collect();
        assert!(!ids.contains(&100), "deleted tail doc 100 should not appear in results");
    }

    #[tokio::test]
    async fn test_no_index_full_brute_force() {
        let mgr = test_manager().await;
        let docs = make_docs(0, 100, 32);
        let query_vec = docs[42].vector.clone().unwrap();
        mgr.upsert("test", docs).await.unwrap();

        let idx_len = mgr.index_len("test").await.unwrap();
        assert_eq!(idx_len, 0, "fresh namespace should have no index");

        let results = mgr.query("test", query_vec, 10, None, false).await.unwrap();
        assert!(!results.is_empty(), "brute-force should return results");
        assert_eq!(results[0].id, 42, "nearest neighbor should be exact match");
    }
}
