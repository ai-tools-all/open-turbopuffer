use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::info;

use crate::storage::ObjectStore;
use crate::types::*;
use super::search::brute_force_knn;
use super::batcher::WriteBatcher;
use super::index::VectorIndex;
use super::index::spfresh::{SPFreshIndex, SPFreshConfig};

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

struct NamespaceState {
    metadata: NamespaceMetadata,
    documents: HashMap<u64, Document>,
    index: Option<SPFreshIndex>,
}

pub struct NamespaceManager {
    store: ObjectStore,
    namespaces: RwLock<HashMap<String, Arc<RwLock<NamespaceState>>>>,
    batchers: RwLock<HashMap<String, Arc<WriteBatcher>>>,
}

impl NamespaceManager {
    pub fn new(store: ObjectStore) -> Self {
        Self {
            store,
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
        let sequences = self.store.list_wal_sequences(name).await?;
        for seq in &sequences {
            if let Some(entry) = self.store.read_wal(name, *seq).await? {
                apply_wal_entry(&mut documents, &entry);
            }
        }

        let doc_count = documents.len() as u64;
        let wal_sequence = sequences.last().copied().unwrap_or(0);

        let index = if let Some(dims) = meta.dimensions {
            let config = SPFreshConfig {
                dimensions: dims,
                distance_metric: meta.distance_metric,
                ..Default::default()
            };
            let idx = SPFreshIndex::new(config);
            for doc in documents.values() {
                if let Some(vec) = &doc.vector {
                    idx.insert(doc.id, vec)
                        .map_err(|e| EngineError::Validation(e.to_string()))?;
                }
            }
            Some(idx)
        } else {
            None
        };

        let state = NamespaceState {
            metadata: NamespaceMetadata {
                doc_count,
                wal_sequence,
                ..meta
            },
            documents,
            index,
        };

        info!(
            namespace = %name,
            docs = doc_count,
            wal_entries = sequences.len(),
            "namespace loaded"
        );

        let arc_state = Arc::new(RwLock::new(state));
        self.namespaces.write().await.insert(name.to_string(), arc_state);

        let batcher = Arc::new(WriteBatcher::new(name.to_string()));
        self.batchers.write().await.insert(name.to_string(), batcher);

        Ok(())
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
            index: None,
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
        apply_wal_entry(&mut state.documents, &entry);

        for op in &entry.operations {
            match op {
                WriteOp::Upsert(docs) => {
                    for doc in docs {
                        if let Some(vec) = &doc.vector {
                            if state.index.is_none() {
                                let dims = state.metadata.dimensions.unwrap();
                                let metric = state.metadata.distance_metric;
                                let config = SPFreshConfig {
                                    dimensions: dims,
                                    distance_metric: metric,
                                    ..Default::default()
                                };
                                state.index = Some(SPFreshIndex::new(config));
                            }
                            state.index.as_ref().unwrap().insert(doc.id, vec)
                                .map_err(|e| EngineError::Validation(e.to_string()))?;
                        }
                    }
                }
                WriteOp::Delete(ids) => {
                    if let Some(index) = &state.index {
                        for &id in ids {
                            index.delete(id)
                                .map_err(|e| EngineError::Validation(e.to_string()))?;
                        }
                    }
                }
            }
        }

        state.metadata.wal_sequence = seq;
        state.metadata.doc_count = state.documents.len() as u64;
        self.store.write_metadata(&state.metadata).await?;

        Ok(())
    }

    pub async fn index_len(&self, ns: &str) -> Result<usize, EngineError> {
        let namespaces = self.namespaces.read().await;
        let ns_state = namespaces.get(ns)
            .ok_or_else(|| EngineError::NotFound(ns.to_string()))?;
        let state = ns_state.read().await;
        Ok(state.index.as_ref().map_or(0, |idx| idx.len()))
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

        let scored = if let Some(index) = &state.index {
            if index.len() > 0 {
                index.search(&vector, top_k)
                    .map_err(|e| EngineError::Validation(e.to_string()))?
            } else {
                Vec::new()
            }
        } else {
            let vectors: Vec<(u64, &[f32])> = state.documents.iter()
                .filter_map(|(id, doc)| {
                    doc.vector.as_ref().map(|v| (*id, v.as_slice()))
                })
                .collect();
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
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct QueryResult {
    pub id: u64,
    pub dist: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    pub attributes: HashMap<String, AttributeValue>,
}

pub(crate) fn apply_wal_entry(documents: &mut HashMap<u64, Document>, entry: &WalEntry) {
    for op in &entry.operations {
        match op {
            WriteOp::Upsert(docs) => {
                for doc in docs {
                    documents.insert(doc.id, doc.clone());
                }
            }
            WriteOp::Delete(ids) => {
                for id in ids {
                    documents.remove(id);
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
        let mgr = NamespaceManager::new(store);
        mgr.create_namespace("test".into(), DistanceMetric::EuclideanSquared).await.unwrap();
        mgr
    }

    #[tokio::test]
    async fn test_index_built_on_upsert() {
        let mgr = test_manager().await;
        let docs = make_docs(0, 100, 32);
        mgr.upsert("test", docs).await.unwrap();

        let idx_len = mgr.index_len("test").await.unwrap();
        assert_eq!(idx_len, 100, "index should have 100 vectors, got {idx_len}");
    }

    #[tokio::test]
    async fn test_delete_removes_from_index() {
        let mgr = test_manager().await;
        let docs = make_docs(0, 100, 32);
        mgr.upsert("test", docs).await.unwrap();

        let ids_to_delete: Vec<u64> = (0..50).collect();
        mgr.delete_docs("test", ids_to_delete).await.unwrap();

        let idx_len = mgr.index_len("test").await.unwrap();
        assert_eq!(idx_len, 50, "index should have 50 vectors after deleting 50, got {idx_len}");
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
        let mgr = NamespaceManager::new(store);
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

        let idx_len = mgr.index_len("test").await.unwrap();
        assert_eq!(idx_len, 600, "expected 600 vectors (500 - 100 + 200), got {idx_len}");

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
}
