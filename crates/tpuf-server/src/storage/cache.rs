use std::path::PathBuf;
use std::sync::Arc;
use foyer::{
    BlockEngineConfig, DeviceBuilder, FsDeviceBuilder, HybridCache, HybridCacheBuilder,
};
use tracing::info;

use crate::engine::index::spfresh::PostingList;
use super::ObjectStore;

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub dir: PathBuf,
    pub memory_capacity: usize,
    pub disk_capacity: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("/tmp/tpuf-cache"),
            memory_capacity: 64 * 1024 * 1024,
            disk_capacity: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone)]
pub struct CachedPostingStore {
    cache: HybridCache<(String, u32), PostingList>,
    store: ObjectStore,
}

impl CachedPostingStore {
    pub async fn new(store: ObjectStore, config: &CacheConfig) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&config.dir)?;

        let device = FsDeviceBuilder::new(&config.dir)
            .with_capacity(config.disk_capacity)
            .build()?;

        let cache = HybridCacheBuilder::new()
            .with_name("tpuf-postings")
            .memory(config.memory_capacity)
            .with_weighter(|_key: &(String, u32), value: &PostingList| {
                let entry_size = value.entries.len() * 80;
                entry_size + 64
            })
            .storage()
            .with_engine_config(BlockEngineConfig::new(device))
            .build()
            .await?;

        info!(
            memory_mb = config.memory_capacity / (1024 * 1024),
            disk_mb = config.disk_capacity / (1024 * 1024),
            dir = %config.dir.display(),
            "posting cache initialized"
        );

        Ok(Self { cache, store })
    }

    pub async fn new_memory_only(store: ObjectStore, capacity: usize) -> anyhow::Result<Self> {
        let cache = HybridCacheBuilder::new()
            .with_name("tpuf-postings")
            .memory(capacity)
            .with_weighter(|_key: &(String, u32), value: &PostingList| {
                let entry_size = value.entries.len() * 80;
                entry_size + 64
            })
            .storage()
            .build()
            .await?;

        Ok(Self { cache, store })
    }

    pub async fn get(&self, ns: &str, head_id: u32) -> Option<PostingList> {
        let key = (ns.to_string(), head_id);
        let ns_owned = ns.to_string();
        let store = self.store.clone();

        let entry = self.cache.get_or_fetch(
            &key,
            || {
                let ns = ns_owned;
                let store = store;
                async move {
                    match store.read_posting(&ns, head_id).await {
                        Ok(Some(pl)) => Ok::<_, anyhow::Error>(pl),
                        Ok(None) => Err(anyhow::anyhow!("posting not found")),
                        Err(e) => Err(e.into()),
                    }
                }
            },
        ).await;

        match entry {
            Ok(entry) => Some(entry.value().clone()),
            Err(_) => None,
        }
    }

    pub async fn get_multi(&self, ns: &str, head_ids: &[u32]) -> Vec<(u32, PostingList)> {
        let mut results = Vec::with_capacity(head_ids.len());
        for &hid in head_ids {
            if let Some(pl) = self.get(ns, hid).await {
                results.push((hid, pl));
            }
        }
        results
    }

    pub fn insert(&self, ns: &str, head_id: u32, posting: PostingList) {
        let key = (ns.to_string(), head_id);
        self.cache.insert(key, posting);
    }

    pub fn invalidate_namespace(&self, ns: &str) {
        let _ = ns;
        // foyer doesn't support prefix invalidation — entries will naturally evict.
        // On hot-reload we create a fresh cache or let stale entries age out.
    }
}

pub type SharedPostingCache = Arc<CachedPostingStore>;
