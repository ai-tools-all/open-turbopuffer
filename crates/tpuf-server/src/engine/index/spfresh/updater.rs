use crate::engine::search::distance;
use crate::types::DistanceMetric;
use super::config::SPFreshConfig;
use super::head_index::HeadIndex;
use super::posting::PostingEntry;
use super::posting_store::PostingStore;
use super::version_map::VersionMap;

pub fn rng_filter(
    candidates: &[(u32, f32)],
    head_index: &HeadIndex,
    metric: DistanceMetric,
    rng_factor: f32,
    max_replicas: usize,
) -> Vec<u32> {
    if candidates.is_empty() {
        return vec![];
    }

    let mut selected: Vec<u32> = vec![candidates[0].0];

    for &(head_id, dist_to_query) in &candidates[1..] {
        if selected.len() >= max_replicas {
            break;
        }

        let candidate_centroid = match head_index.get_centroid(head_id) {
            Some(c) => c,
            None => continue,
        };

        let dominated = selected.iter().any(|&sel_id| {
            if let Some(sel_centroid) = head_index.get_centroid(sel_id) {
                distance(candidate_centroid, sel_centroid, metric) < rng_factor * dist_to_query
            } else {
                false
            }
        });

        if !dominated {
            selected.push(head_id);
        }
    }

    selected
}

pub fn insert_vector(
    vid: u64,
    vector: &[f32],
    head_index: &HeadIndex,
    posting_store: &dyn PostingStore,
    version_map: &VersionMap,
    config: &SPFreshConfig,
) -> Vec<u32> {
    let search_k = config.replica_count * 2;
    let candidates = head_index.search(vector, search_k, config.distance_metric);

    let head_ids = if candidates.is_empty() {
        vec![]
    } else {
        rng_filter(
            &candidates,
            head_index,
            config.distance_metric,
            config.rng_factor,
            config.replica_count,
        )
    };

    let entry = PostingEntry {
        vector_id: vid,
        version: version_map.get_version(vid),
        vector: vector.to_vec(),
    };

    let mut oversized = Vec::new();
    for &hid in &head_ids {
        posting_store.append(hid, &[entry.clone()]);
        if posting_store.get_size(hid) > config.max_posting_size {
            oversized.push(hid);
        }
    }

    oversized
}

pub fn delete_vector(vid: u64, version_map: &VersionMap) {
    version_map.mark_deleted(vid);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rng_filter_selects_first() {
        let mut hi = HeadIndex::new(2);
        hi.add_centroid(&[0.0, 0.0]);
        hi.add_centroid(&[10.0, 10.0]);

        let candidates = vec![(0, 1.0), (1, 5.0)];
        let result = rng_filter(&candidates, &hi, DistanceMetric::EuclideanSquared, 2.0, 4);
        assert!(result.contains(&0));
    }

    #[test]
    fn test_rng_filter_respects_max() {
        let mut hi = HeadIndex::new(2);
        for i in 0..10 {
            hi.add_centroid(&[i as f32 * 100.0, 0.0]);
        }
        let candidates: Vec<(u32, f32)> = (0..10).map(|i| (i, (i + 1) as f32)).collect();
        let result = rng_filter(&candidates, &hi, DistanceMetric::EuclideanSquared, 0.0, 3);
        assert!(result.len() <= 3);
    }

    #[test]
    fn test_rng_filter_empty() {
        let hi = HeadIndex::new(2);
        let result = rng_filter(&[], &hi, DistanceMetric::EuclideanSquared, 2.0, 4);
        assert!(result.is_empty());
    }

    #[test]
    fn test_delete_vector() {
        let vm = VersionMap::new(10);
        vm.initialize(5);
        assert!(!vm.is_deleted(5));
        delete_vector(5, &vm);
        assert!(vm.is_deleted(5));
    }
}
