pub mod config;
pub mod version_map;
pub mod kmeans;
pub mod posting;
pub mod posting_store;
pub mod head_index;
pub mod search;
pub mod updater;

pub use config::SPFreshConfig;
pub use version_map::VersionMap;
pub use kmeans::{KMeansResult, binary_kmeans};
pub use posting::{PostingEntry, PostingList};
pub use posting_store::{PostingStore, MemoryPostingStore};
pub use head_index::HeadIndex;

use std::sync::{RwLock, atomic::{AtomicUsize, Ordering}};

pub struct SPFreshIndex {
    config: SPFreshConfig,
    head_index: RwLock<HeadIndex>,
    posting_store: MemoryPostingStore,
    version_map: RwLock<VersionMap>,
    vector_count: AtomicUsize,
}

impl SPFreshIndex {
    pub fn new(config: SPFreshConfig) -> Self {
        let dims = config.dimensions;
        Self {
            config,
            head_index: RwLock::new(HeadIndex::new(dims)),
            posting_store: MemoryPostingStore::new(),
            version_map: RwLock::new(VersionMap::new(1024)),
            vector_count: AtomicUsize::new(0),
        }
    }
}

impl super::VectorIndex for SPFreshIndex {
    fn insert(&self, id: u64, vector: &[f32]) -> anyhow::Result<()> {
        {
            let mut vm = self.version_map.write().unwrap();
            vm.ensure_capacity(id);
        }
        {
            let vm = self.version_map.read().unwrap();
            vm.initialize(id);
        }

        let hi_empty = {
            let hi = self.head_index.read().unwrap();
            hi.is_empty()
        };

        if hi_empty {
            let mut hi = self.head_index.write().unwrap();
            if hi.is_empty() {
                hi.add_centroid(vector);
            }
        }

        {
            let hi = self.head_index.read().unwrap();
            let vm = self.version_map.read().unwrap();
            let _oversized = updater::insert_vector(
                id,
                vector,
                &hi,
                &self.posting_store,
                &vm,
                &self.config,
            );
        }

        self.vector_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn delete(&self, id: u64) -> anyhow::Result<()> {
        let vm = self.version_map.read().unwrap();
        updater::delete_vector(id, &vm);
        self.vector_count.fetch_sub(1, Ordering::Relaxed);
        Ok(())
    }

    fn search(&self, query: &[f32], k: usize) -> anyhow::Result<Vec<(u64, f32)>> {
        let head_ids: Vec<u32> = {
            let hi = self.head_index.read().unwrap();
            hi.search(query, self.config.num_search_heads, self.config.distance_metric)
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        };

        let postings = self.posting_store.get_multi(&head_ids);

        let vm = self.version_map.read().unwrap();
        let results = search::search_postings(query, &postings, &vm, self.config.distance_metric, k);
        Ok(results)
    }

    fn len(&self) -> usize {
        self.vector_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::engine::index::VectorIndex;
    use crate::engine::search::brute_force_knn;
    use crate::types::DistanceMetric;

    fn generate_random_vectors(n: usize, dims: usize) -> Vec<Vec<f32>> {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        (0..n).map(|_| {
            let v: Vec<f32> = (0..dims).map(|_| rng.gen::<f32>() - 0.5).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 { v.iter().map(|x| x / norm).collect() } else { v }
        }).collect()
    }

    #[test]
    fn test_basic_insert_search() {
        let config = SPFreshConfig {
            dimensions: 4,
            num_search_heads: 8,
            max_posting_size: 64,
            replica_count: 2,
            ..Default::default()
        };
        let idx = SPFreshIndex::new(config);

        idx.insert(0, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        idx.insert(1, &[0.0, 1.0, 0.0, 0.0]).unwrap();
        idx.insert(2, &[0.0, 0.0, 1.0, 0.0]).unwrap();

        assert_eq!(idx.len(), 3);

        let results = idx.search(&[1.0, 0.0, 0.0, 0.0], 2).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 0);
    }

    #[test]
    fn test_delete() {
        let config = SPFreshConfig {
            dimensions: 2,
            num_search_heads: 8,
            max_posting_size: 64,
            replica_count: 2,
            ..Default::default()
        };
        let idx = SPFreshIndex::new(config);

        idx.insert(0, &[1.0, 0.0]).unwrap();
        idx.insert(1, &[0.0, 1.0]).unwrap();
        idx.delete(0).unwrap();

        let results = idx.search(&[1.0, 0.0], 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn test_recall_at_10() {
        let dims = 32;
        let n = 5000;
        let n_queries = 50;
        let k = 10;

        let config = SPFreshConfig {
            dimensions: dims,
            num_search_heads: 32,
            max_posting_size: 64,
            replica_count: 2,
            distance_metric: DistanceMetric::EuclideanSquared,
            ..Default::default()
        };
        let idx = SPFreshIndex::new(config);

        let vectors = generate_random_vectors(n, dims);
        for (i, v) in vectors.iter().enumerate() {
            idx.insert(i as u64, v).unwrap();
        }

        let queries = generate_random_vectors(n_queries, dims);
        let indexed: Vec<(u64, &[f32])> = vectors.iter().enumerate()
            .map(|(i, v)| (i as u64, v.as_slice()))
            .collect();

        let mut total_recall = 0.0;
        for q in &queries {
            let gt = brute_force_knn(q, &indexed, DistanceMetric::EuclideanSquared, k);
            let gt_ids: std::collections::HashSet<u64> = gt.iter().map(|(id, _)| *id).collect();

            let results = idx.search(q, k).unwrap();
            let result_ids: std::collections::HashSet<u64> = results.iter().map(|(id, _)| *id).collect();

            let overlap = gt_ids.intersection(&result_ids).count();
            total_recall += overlap as f64 / k as f64;
        }

        let avg_recall = total_recall / n_queries as f64;
        println!("Recall@{k}: {avg_recall:.4} (n={n}, queries={n_queries}, dims={dims})");
        assert!(avg_recall > 0.90, "Recall@{k} = {avg_recall:.4}, expected > 0.90");
    }
}
