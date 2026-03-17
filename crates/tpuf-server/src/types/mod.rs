use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: u64,
    pub vector: Option<Vec<f32>>,
    pub attributes: HashMap<String, AttributeValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AttributeValue {
    Null,
    Bool(bool),
    U64(u64),
    F64(f64),
    String(String),
}

impl AttributeValue {
    pub fn from_json(v: &serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(b) => Self::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(u) = n.as_u64() {
                    Self::U64(u)
                } else {
                    Self::F64(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => Self::String(s.clone()),
            _ => Self::Null,
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(b) => serde_json::Value::Bool(*b),
            Self::U64(n) => serde_json::json!(*n),
            Self::F64(n) => serde_json::json!(*n),
            Self::String(s) => serde_json::Value::String(s.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DistanceMetric {
    CosineDistance,
    EuclideanSquared,
    DotProduct,
}

impl Default for DistanceMetric {
    fn default() -> Self {
        Self::CosineDistance
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceMetadata {
    pub name: String,
    pub dimensions: Option<usize>,
    pub distance_metric: DistanceMetric,
    pub wal_sequence: u64,
    pub doc_count: u64,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalEntry {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub operations: Vec<WriteOp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WriteOp {
    Upsert(Vec<Document>),
    Delete(Vec<u64>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexManifest {
    pub version: u32,
    pub wal_sequence: u64,
    pub config: crate::engine::index::spfresh::SPFreshConfig,
    pub active_centroids: u32,
    pub num_vectors: u64,
    pub created_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_roundtrip() {
        let manifest = IndexManifest {
            version: 1,
            wal_sequence: 42,
            config: crate::engine::index::spfresh::SPFreshConfig::default(),
            active_centroids: 10,
            num_vectors: 5000,
            created_at: 1234567890,
        };
        let bytes = bincode::serialize(&manifest).unwrap();
        let decoded: IndexManifest = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.wal_sequence, 42);
        assert_eq!(decoded.active_centroids, 10);
        assert_eq!(decoded.num_vectors, 5000);
        assert_eq!(decoded.created_at, 1234567890);
    }
}
