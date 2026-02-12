use opendal::{Operator, services::S3};
use std::sync::Arc;
use tracing::debug;
use crate::types::{NamespaceMetadata, WalEntry};

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
}

impl ObjectStore {
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
        let data = Self::encode(entry)?;
        self.op.write(&Self::wal_key(ns, entry.sequence), data).await?;
        Ok(())
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
