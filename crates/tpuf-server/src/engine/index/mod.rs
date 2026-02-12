pub mod spfresh;

pub trait VectorIndex: Send + Sync {
    fn insert(&self, id: u64, vector: &[f32]) -> anyhow::Result<()>;
    fn delete(&self, id: u64) -> anyhow::Result<()>;
    fn search(&self, query: &[f32], k: usize) -> anyhow::Result<Vec<(u64, f32)>>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
