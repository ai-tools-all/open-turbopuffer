pub mod config;
pub mod version_map;
pub mod kmeans;
pub mod posting;
pub mod posting_store;
pub mod head_index;
pub mod search;
pub mod updater;
pub mod rebuilder;

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

    pub fn process_pending_splits(&self, oversized: &[u32]) {
        if oversized.is_empty() {
            return;
        }
        let mut hi = self.head_index.write().unwrap();
        let vm = self.version_map.read().unwrap();
        rebuilder::process_splits(oversized, &mut hi, &self.posting_store, &vm, &self.config);
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

        let oversized = {
            let hi = self.head_index.read().unwrap();
            let vm = self.version_map.read().unwrap();
            updater::insert_vector(
                id,
                vector,
                &hi,
                &self.posting_store,
                &vm,
                &self.config,
            )
        };

        if !oversized.is_empty() {
            self.process_pending_splits(&oversized);
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

    fn generate_seeded_vectors(n: usize, dims: usize, seed: u64) -> Vec<Vec<f32>> {
        use rand::Rng;
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        (0..n).map(|_| {
            let v: Vec<f32> = (0..dims).map(|_| rng.gen::<f32>() - 0.5).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 { v.iter().map(|x| x / norm).collect() } else { v }
        }).collect()
    }

    fn compute_recall(
        idx: &SPFreshIndex,
        queries: &[Vec<f32>],
        indexed: &[(u64, &[f32])],
        metric: DistanceMetric,
        k: usize,
    ) -> f64 {
        let mut total = 0.0;
        for q in queries {
            let gt = brute_force_knn(q, indexed, metric, k);
            let gt_ids: std::collections::HashSet<u64> = gt.iter().map(|(id, _)| *id).collect();
            let results = idx.search(q, k).unwrap();
            let result_ids: std::collections::HashSet<u64> = results.iter().map(|(id, _)| *id).collect();
            let overlap = gt_ids.intersection(&result_ids).count();
            total += overlap as f64 / k as f64;
        }
        total / queries.len() as f64
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
            num_search_heads: 64,
            max_posting_size: 64,
            replica_count: 3,
            reassign_range: 64,
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

    #[test]
    fn test_split_triggers_and_preserves_vectors() {
        let dims = 8;
        let n = 200;
        let config = SPFreshConfig {
            dimensions: dims,
            num_search_heads: 16,
            max_posting_size: 16,
            replica_count: 2,
            rng_factor: 2.0,
            reassign_range: 16,
            distance_metric: DistanceMetric::EuclideanSquared,
            ..Default::default()
        };
        let idx = SPFreshIndex::new(config);

        let vectors = generate_seeded_vectors(n, dims, 42);
        for (i, v) in vectors.iter().enumerate() {
            idx.insert(i as u64, v).unwrap();
        }

        let hi = idx.head_index.read().unwrap();
        let num_centroids = hi.len();
        drop(hi);

        println!("split_triggers: {n} vectors, {num_centroids} centroids after splits");
        assert!(num_centroids > 1, "Expected splits to create multiple centroids, got {num_centroids}");

        let mut found = 0;
        for (i, v) in vectors.iter().enumerate() {
            let results = idx.search(v, 1).unwrap();
            if !results.is_empty() && results[0].0 == i as u64 {
                found += 1;
            }
        }

        let find_rate = found as f64 / n as f64;
        println!("split_triggers: find_rate={find_rate:.4} ({found}/{n})");
        assert!(
            find_rate > 0.85,
            "Expected >85% vectors findable after splits, got {find_rate:.4}"
        );
    }

    #[test]
    fn test_incremental_indexing_recall_stability() {
        let dims = 32;
        let base_size = 2000;
        let increment = 500;
        let num_increments = 6;
        let k = 10;

        let config = SPFreshConfig {
            dimensions: dims,
            num_search_heads: 48,
            max_posting_size: 32,
            replica_count: 3,
            rng_factor: 2.0,
            reassign_range: 48,
            distance_metric: DistanceMetric::EuclideanSquared,
            ..Default::default()
        };
        let idx = SPFreshIndex::new(config);

        let total = base_size + increment * num_increments;
        let all_vectors = generate_seeded_vectors(total, dims, 42);
        let queries = generate_seeded_vectors(50, dims, 123);

        for (i, v) in all_vectors[..base_size].iter().enumerate() {
            idx.insert(i as u64, v).unwrap();
        }

        let base_indexed: Vec<(u64, &[f32])> = all_vectors[..base_size]
            .iter()
            .enumerate()
            .map(|(i, v)| (i as u64, v.as_slice()))
            .collect();
        let baseline = compute_recall(&idx, &queries, &base_indexed, DistanceMetric::EuclideanSquared, k);
        println!("incremental: baseline recall@{k}={baseline:.4} (n={base_size})");

        let mut recalls = vec![baseline];

        for batch in 0..num_increments {
            let start = base_size + batch * increment;
            let end = start + increment;
            for (i, v) in all_vectors[start..end].iter().enumerate() {
                idx.insert((start + i) as u64, v).unwrap();
            }

            let current_indexed: Vec<(u64, &[f32])> = all_vectors[..end]
                .iter()
                .enumerate()
                .map(|(i, v)| (i as u64, v.as_slice()))
                .collect();
            let recall = compute_recall(&idx, &queries, &current_indexed, DistanceMetric::EuclideanSquared, k);
            println!("incremental: batch {} recall@{k}={recall:.4} (n={end})", batch + 1);
            recalls.push(recall);
        }

        println!("incremental: all recalls={recalls:?}");

        let max_degradation = baseline - recalls.iter().copied().fold(f64::INFINITY, f64::min);
        println!("incremental: max_degradation={max_degradation:.4}");
        assert!(
            max_degradation < 0.15,
            "Recall degraded by {max_degradation:.4}, expected < 0.15"
        );

        let final_recall = *recalls.last().unwrap();
        assert!(
            final_recall > 0.70,
            "Final recall={final_recall:.4}, expected > 0.70"
        );
    }

    #[test]
    fn test_concurrent_insert_and_search() {
        use std::sync::Arc;
        use std::thread;

        let dims = 16;
        let config = SPFreshConfig {
            dimensions: dims,
            num_search_heads: 16,
            max_posting_size: 32,
            replica_count: 2,
            rng_factor: 2.0,
            reassign_range: 16,
            distance_metric: DistanceMetric::EuclideanSquared,
            ..Default::default()
        };
        let idx = Arc::new(SPFreshIndex::new(config));

        let all_vectors = generate_seeded_vectors(400, dims, 42);
        let all_vectors = Arc::new(all_vectors);

        let mut handles = Vec::new();

        for t in 0..4u64 {
            let idx = Arc::clone(&idx);
            let vecs = Arc::clone(&all_vectors);
            handles.push(thread::spawn(move || {
                let base = (t * 100) as usize;
                for i in 0..100 {
                    let id = base + i;
                    idx.insert(id as u64, &vecs[id]).unwrap();
                }
            }));
        }

        let query_vectors = generate_seeded_vectors(100, dims, 99);
        for _ in 0..2 {
            let idx = Arc::clone(&idx);
            let qv = query_vectors.clone();
            handles.push(thread::spawn(move || {
                for q in &qv {
                    let results = idx.search(q, 5).unwrap();
                    assert!(results.len() <= 5);
                }
            }));
        }

        for h in handles {
            h.join().expect("Thread panicked");
        }

        assert_eq!(idx.len(), 400);

        let mut found = 0;
        for (i, v) in all_vectors.iter().enumerate() {
            let results = idx.search(v, 1).unwrap();
            if !results.is_empty() && results[0].0 == i as u64 {
                found += 1;
            }
        }

        let find_rate = found as f64 / 400.0;
        println!("concurrent: find_rate={find_rate:.4} ({found}/400)");
        assert!(
            find_rate > 0.80,
            "Expected >80% vectors findable after concurrent inserts, got {find_rate:.4}"
        );
    }
}
