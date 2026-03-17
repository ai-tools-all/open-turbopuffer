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

        let vectors: Vec<(u64, &[f32])> = state.documents.iter()
            .filter_map(|(id, doc)| {
                doc.vector.as_ref().map(|v| (*id, v.as_slice()))
            })
            .collect();

        let results = brute_force_knn(&vector, &vectors, metric, top_k);

        let query_results: Vec<QueryResult> = results.into_iter().map(|(id, dist)| {
            let doc = state.documents.get(&id).unwrap();
            QueryResult {
                id,
                dist,
                vector: if include_vectors { doc.vector.clone() } else { None },
                attributes: doc.attributes.clone(),
            }
        }).collect();

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
}
