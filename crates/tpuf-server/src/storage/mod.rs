use opendal::{Operator, services::S3};
use std::sync::Arc;
use tracing::debug;
use crate::types::{NamespaceMetadata, WalEntry, IndexManifest};
use crate::engine::index::spfresh::{CentroidsFile, VersionMapFile, PostingList};

#[derive(Clone)]
pub struct ObjectStore {
    op: Arc<Operator>,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("opendal: {0}")]
    OpenDal(#[from] opendal::Error),
    #[error("bincode: {0}")]
    Bincode(#[from] Box<bincode::ErrorKind>),
    #[error("zstd: {0}")]
    Zstd(#[from] std::io::Error),
    #[error("write conflict: {0}")]
    WriteConflict(String),
}

impl ObjectStore {
    #[cfg(any(test, feature = "test-utils"))]
    pub fn in_memory() -> Self {
        let op = Operator::new(opendal::services::Memory::default()).unwrap().finish();
        Self { op: Arc::new(op) }
    }

    pub fn new_r2(account_id: &str, bucket: &str, access_key: &str, secret_key: &str) -> Result<Self, StorageError> {
        let endpoint = format!("https://{account_id}.r2.cloudflarestorage.com");
        let builder = S3::default()
            .endpoint(&endpoint)
            .bucket(bucket)
            .access_key_id(access_key)
            .secret_access_key(secret_key)
            .region("auto");

        let op = Operator::new(builder)?.finish();
        Ok(Self { op: Arc::new(op) })
    }

    pub fn new(endpoint: &str, bucket: &str, access_key: &str, secret_key: &str, region: &str) -> Result<Self, StorageError> {
        let builder = S3::default()
            .endpoint(endpoint)
            .bucket(bucket)
            .access_key_id(access_key)
            .secret_access_key(secret_key)
            .region(region);

        let op = Operator::new(builder)?.finish();
        Ok(Self { op: Arc::new(op) })
    }

    fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, StorageError> {
        let raw = bincode::serialize(value)?;
        let compressed = zstd::encode_all(raw.as_slice(), 3)?;
        Ok(compressed)
    }

    fn decode<T: serde::de::DeserializeOwned>(data: &[u8]) -> Result<T, StorageError> {
        let decompressed = zstd::decode_all(data)?;
        let value = bincode::deserialize(&decompressed)?;
        Ok(value)
    }

    fn meta_key(ns: &str) -> String {
        format!("{}/meta.bin", ns)
    }

    fn wal_key(ns: &str, seq: u64) -> String {
        format!("{}/wal/{:020}.wal", ns, seq)
    }

    fn wal_prefix(ns: &str) -> String {
        format!("{}/wal/", ns)
    }

    fn ns_prefix(ns: &str) -> String {
        format!("{}/", ns)
    }

    pub async fn write_metadata(&self, meta: &NamespaceMetadata) -> Result<(), StorageError> {
        let data = Self::encode(meta)?;
        self.op.write(&Self::meta_key(&meta.name), data).await?;
        Ok(())
    }

    pub async fn read_metadata(&self, ns: &str) -> Result<Option<NamespaceMetadata>, StorageError> {
        match self.op.read(&Self::meta_key(ns)).await {
            Ok(data) => Ok(Some(Self::decode(&data.to_vec())?)),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn write_wal(&self, ns: &str, entry: &WalEntry) -> Result<(), StorageError> {
        let key = Self::wal_key(ns, entry.sequence);
        let data = Self::encode(entry)?;
        match self.op.write_with(&key, data).if_not_exists(true).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == opendal::ErrorKind::ConditionNotMatch => {
                Err(StorageError::WriteConflict(
                    format!("WAL entry {}/{} already exists", ns, entry.sequence)
                ))
            }
            Err(e) if e.kind() == opendal::ErrorKind::Unsupported => {
                let data = Self::encode(entry)?;
                self.op.write(&key, data).await?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn read_wal(&self, ns: &str, seq: u64) -> Result<Option<WalEntry>, StorageError> {
        match self.op.read(&Self::wal_key(ns, seq)).await {
            Ok(data) => Ok(Some(Self::decode(&data.to_vec())?)),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn list_wal_sequences(&self, ns: &str) -> Result<Vec<u64>, StorageError> {
        let prefix = Self::wal_prefix(ns);
        let entries = self.op.list(&prefix).await?;
        let mut seqs: Vec<u64> = entries
            .into_iter()
            .filter_map(|e| {
                let path = e.path();
                let name = e.name();
                debug!(path, name, "wal entry found");
                let stem = name.strip_suffix(".wal")?;
                stem.parse::<u64>().ok()
            })
            .collect();
        seqs.sort();
        Ok(seqs)
    }

    pub async fn list_namespaces(&self) -> Result<Vec<String>, StorageError> {
        let entries = self.op.list("").await?;
        let mut namespaces = Vec::new();
        for entry in entries {
            let path = entry.path();
            let name = entry.name();
            debug!(path, name, metadata = ?entry.metadata().mode(), "root entry");
            if path.ends_with('/') {
                let ns = path.trim_end_matches('/');
                if !ns.is_empty() {
                    if self.read_metadata(ns).await?.is_some() {
                        namespaces.push(ns.to_string());
                    }
                }
            }
        }
        Ok(namespaces)
    }

    fn index_manifest_key(ns: &str) -> String {
        format!("{ns}/index/manifest.bin")
    }

    fn centroids_key(ns: &str) -> String {
        format!("{ns}/index/centroids.bin")
    }

    fn version_map_key(ns: &str) -> String {
        format!("{ns}/index/version_map.bin")
    }

    fn posting_key(ns: &str, head_id: u32) -> String {
        format!("{ns}/index/postings/{head_id:010}.bin")
    }

    pub async fn write_index_manifest(&self, ns: &str, manifest: &IndexManifest) -> Result<(), StorageError> {
        let data = Self::encode(manifest)?;
        self.op.write(&Self::index_manifest_key(ns), data).await?;
        Ok(())
    }

    pub async fn write_index_manifest_cas(
        &self,
        ns: &str,
        manifest: &IndexManifest,
        prev_etag: Option<&str>,
    ) -> Result<String, StorageError> {
        let key = Self::index_manifest_key(ns);
        let data = Self::encode(manifest)?;

        let write_result = match prev_etag {
            Some(etag) => {
                self.op.write_with(&key, data).if_match(etag).await
            }
            None => {
                self.op.write_with(&key, data).if_not_exists(true).await
            }
        };

        match write_result {
            Ok(()) => {}
            Err(e) if e.kind() == opendal::ErrorKind::ConditionNotMatch => {
                return Err(StorageError::WriteConflict(
                    format!("manifest conflict for namespace '{}': another writer updated it", ns)
                ));
            }
            Err(e) if e.kind() == opendal::ErrorKind::Unsupported => {
                let data = Self::encode(manifest)?;
                self.op.write(&key, data).await?;
            }
            Err(e) => return Err(e.into()),
        }

        let meta = self.op.stat(&key).await?;
        let etag = meta.etag().unwrap_or("").to_string();
        Ok(etag)
    }

    pub async fn read_index_manifest(&self, ns: &str) -> Result<Option<IndexManifest>, StorageError> {
        match self.op.read(&Self::index_manifest_key(ns)).await {
            Ok(data) => Ok(Some(Self::decode(&data.to_vec())?)),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn read_index_manifest_with_etag(&self, ns: &str) -> Result<Option<(IndexManifest, String)>, StorageError> {
        let key = Self::index_manifest_key(ns);
        match self.op.stat(&key).await {
            Ok(meta) => {
                let etag = meta.etag().unwrap_or("").to_string();
                let data = self.op.read(&key).await?;
                let manifest: IndexManifest = Self::decode(&data.to_vec())?;
                Ok(Some((manifest, etag)))
            }
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn write_centroids(&self, ns: &str, file: &CentroidsFile) -> Result<(), StorageError> {
        let data = Self::encode(file)?;
        self.op.write(&Self::centroids_key(ns), data).await?;
        Ok(())
    }

    pub async fn read_centroids(&self, ns: &str) -> Result<Option<CentroidsFile>, StorageError> {
        match self.op.read(&Self::centroids_key(ns)).await {
            Ok(data) => Ok(Some(Self::decode(&data.to_vec())?)),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn write_version_map(&self, ns: &str, file: &VersionMapFile) -> Result<(), StorageError> {
        let data = Self::encode(file)?;
        self.op.write(&Self::version_map_key(ns), data).await?;
        Ok(())
    }

    pub async fn read_version_map(&self, ns: &str) -> Result<Option<VersionMapFile>, StorageError> {
        match self.op.read(&Self::version_map_key(ns)).await {
            Ok(data) => Ok(Some(Self::decode(&data.to_vec())?)),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn write_posting(&self, ns: &str, head_id: u32, posting: &PostingList) -> Result<(), StorageError> {
        let data = Self::encode(posting)?;
        self.op.write(&Self::posting_key(ns, head_id), data).await?;
        Ok(())
    }

    pub async fn read_posting(&self, ns: &str, head_id: u32) -> Result<Option<PostingList>, StorageError> {
        match self.op.read(&Self::posting_key(ns, head_id)).await {
            Ok(data) => Ok(Some(Self::decode(&data.to_vec())?)),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn delete_namespace(&self, ns: &str) -> Result<(), StorageError> {
        let prefix = Self::ns_prefix(ns);
        let entries = self.op.list(&prefix).await?;
        for entry in entries {
            let path = format!("{}{}", prefix, entry.name());
            self.op.delete(&path).await?;
        }
        self.op.delete(&Self::meta_key(ns)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::index::spfresh::{PostingEntry, SPFreshConfig};

    #[tokio::test]
    async fn test_s3_centroids_write_read() {
        let store = ObjectStore::in_memory();
        let file = CentroidsFile {
            dims: 3,
            next_id: 2,
            centroids: vec![1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
            active: vec![true, true],
        };
        store.write_centroids("ns1", &file).await.unwrap();
        let loaded = store.read_centroids("ns1").await.unwrap().unwrap();
        assert_eq!(file, loaded);
    }

    #[tokio::test]
    async fn test_s3_centroids_missing() {
        let store = ObjectStore::in_memory();
        let loaded = store.read_centroids("missing").await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_s3_version_map_write_read() {
        let store = ObjectStore::in_memory();
        let file = VersionMapFile {
            capacity: 4,
            data: vec![0, 1, 2, 0x83],
        };
        store.write_version_map("ns1", &file).await.unwrap();
        let loaded = store.read_version_map("ns1").await.unwrap().unwrap();
        assert_eq!(file, loaded);
    }

    #[tokio::test]
    async fn test_s3_posting_write_read() {
        let store = ObjectStore::in_memory();
        let posting = PostingList {
            entries: vec![
                PostingEntry { vector_id: 1, version: 1, vector: vec![1.0, 2.0] },
                PostingEntry { vector_id: 2, version: 3, vector: vec![3.0, 4.0] },
            ],
        };
        store.write_posting("ns1", 5, &posting).await.unwrap();
        let loaded = store.read_posting("ns1", 5).await.unwrap().unwrap();
        assert_eq!(posting, loaded);
    }

    #[tokio::test]
    async fn test_s3_posting_missing() {
        let store = ObjectStore::in_memory();
        let loaded = store.read_posting("ns1", 99).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_s3_manifest_write_read() {
        let store = ObjectStore::in_memory();
        let manifest = IndexManifest {
            version: 1,
            wal_sequence: 42,
            config: SPFreshConfig::default(),
            active_centroids: 10,
            num_vectors: 5000,
            created_at: 123456,
        };
        store.write_index_manifest("ns1", &manifest).await.unwrap();
        let loaded = store.read_index_manifest("ns1").await.unwrap().unwrap();
        assert_eq!(loaded.wal_sequence, 42);
        assert_eq!(loaded.num_vectors, 5000);
    }

    fn r2_store() -> Option<ObjectStore> {
        let account_id = std::env::var("R2_ACCOUNT_ID").ok()?;
        let access_key = std::env::var("R2_ACCESS_KEY_ID").ok()?;
        let secret_key = std::env::var("R2_SECRET_ACCESS_KEY").ok()?;
        let bucket = std::env::var("R2_DATA_BUCKET").unwrap_or_else(|_| "test".into());
        Some(ObjectStore::new_r2(&account_id, &bucket, &access_key, &secret_key).unwrap())
    }

    fn r2_test_ns() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        format!("tpuf-test/{ts}")
    }

    #[tokio::test]
    #[ignore]
    async fn test_r2_wal_exclusive_write() {
        let store = r2_store().expect("R2 env vars not set");
        let ns = r2_test_ns();

        let entry = WalEntry {
            sequence: 1,
            timestamp_ms: 1000,
            operations: vec![],
        };

        store.write_wal(&ns, &entry).await.unwrap();
        println!("first WAL write succeeded");

        let result = store.write_wal(&ns, &entry).await;
        match result {
            Err(StorageError::WriteConflict(msg)) => {
                println!("second WAL write correctly rejected: {msg}");
            }
            Ok(()) => panic!("expected WriteConflict, got Ok — If-None-Match not enforced"),
            Err(e) => panic!("expected WriteConflict, got: {e}"),
        }

        store.delete_namespace(&ns).await.unwrap();
        println!("cleanup done");
    }

    #[tokio::test]
    #[ignore]
    async fn test_r2_manifest_cas() {
        let store = r2_store().expect("R2 env vars not set");
        let ns = r2_test_ns();

        let manifest1 = IndexManifest {
            version: 1,
            wal_sequence: 1,
            config: SPFreshConfig::default(),
            active_centroids: 5,
            num_vectors: 100,
            created_at: 1000,
        };

        let etag1 = store.write_index_manifest_cas(&ns, &manifest1, None).await.unwrap();
        println!("first manifest write succeeded, etag={etag1}");

        let manifest2 = IndexManifest { wal_sequence: 2, num_vectors: 200, ..manifest1.clone() };
        let etag2 = store.write_index_manifest_cas(&ns, &manifest2, Some(&etag1)).await.unwrap();
        println!("CAS update succeeded, etag={etag2}");
        assert_ne!(etag1, etag2, "etag should change after update");

        let manifest3 = IndexManifest { wal_sequence: 3, num_vectors: 300, ..manifest1.clone() };
        let result = store.write_index_manifest_cas(&ns, &manifest3, Some(&etag1)).await;
        match result {
            Err(StorageError::WriteConflict(msg)) => {
                println!("stale etag correctly rejected: {msg}");
            }
            Ok(_) => panic!("expected WriteConflict for stale etag, got Ok"),
            Err(e) => panic!("expected WriteConflict, got: {e}"),
        }

        let (loaded, loaded_etag) = store.read_index_manifest_with_etag(&ns).await.unwrap().unwrap();
        assert_eq!(loaded.wal_sequence, 2, "manifest should still be at wal_sequence=2");
        assert_eq!(loaded_etag, etag2, "loaded etag should match last successful write");

        store.delete_namespace(&ns).await.unwrap();
        println!("cleanup done");
    }

    #[tokio::test]
    #[ignore]
    async fn test_r2_full_persist_roundtrip() {
        use crate::types::DistanceMetric;
        use crate::engine::NamespaceManager;

        let store = r2_store().expect("R2 env vars not set");
        let ns = r2_test_ns();
        let ns_name = ns.clone();

        let mgr = NamespaceManager::new(store.clone());
        mgr.create_namespace(ns_name.clone(), DistanceMetric::EuclideanSquared).await.unwrap();

        let docs: Vec<crate::types::Document> = (0..50u64).map(|i| {
            crate::types::Document {
                id: i,
                vector: Some(vec![i as f32; 8]),
                attributes: std::collections::HashMap::new(),
            }
        }).collect();
        mgr.upsert(&ns_name, docs).await.unwrap();

        mgr.persist_index(&ns_name).await.unwrap();
        println!("persist succeeded on R2");

        let (manifest, etag) = store.read_index_manifest_with_etag(&ns_name).await.unwrap().unwrap();
        println!("manifest: vectors={}, centroids={}, etag={etag}", manifest.num_vectors, manifest.active_centroids);
        assert_eq!(manifest.num_vectors, 50);

        mgr.persist_index(&ns_name).await.unwrap();
        println!("second persist (CAS update) succeeded");

        store.delete_namespace(&ns_name).await.unwrap();
        println!("cleanup done");
    }
}
