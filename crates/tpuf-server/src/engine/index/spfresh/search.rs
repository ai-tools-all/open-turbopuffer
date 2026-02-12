use crate::engine::search::distance;
use crate::types::DistanceMetric;
use super::posting::PostingList;
use super::version_map::VersionMap;
use std::collections::HashMap;

pub fn search_postings(
    query: &[f32],
    postings: &[(u32, PostingList)],
    version_map: &VersionMap,
    metric: DistanceMetric,
    k: usize,
) -> Vec<(u64, f32)> {
    let mut best: HashMap<u64, f32> = HashMap::new();

    for (_, pl) in postings {
        for entry in pl.iter() {
            let vid = entry.vector_id;
            if version_map.is_deleted(vid) {
                continue;
            }
            if version_map.get_version(vid) != entry.version {
                continue;
            }
            let dist = distance(query, &entry.vector, metric);
            best.entry(vid)
                .and_modify(|d| {
                    if dist < *d {
                        *d = dist;
                    }
                })
                .or_insert(dist);
        }
    }

    let mut results: Vec<(u64, f32)> = best.into_iter().collect();
    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(k);
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::index::spfresh::posting::PostingEntry;

    fn make_entry(vid: u64, version: u8, vec: Vec<f32>) -> PostingEntry {
        PostingEntry { vector_id: vid, version, vector: vec }
    }

    #[test]
    fn test_search_basic() {
        let vm = VersionMap::new(10);
        vm.initialize(0);
        vm.initialize(1);
        vm.initialize(2);

        let pl = PostingList {
            entries: vec![
                make_entry(0, 1, vec![0.0, 0.0]),
                make_entry(1, 1, vec![1.0, 0.0]),
                make_entry(2, 1, vec![10.0, 10.0]),
            ],
        };

        let results = search_postings(
            &[0.0, 0.0],
            &[(0, pl)],
            &vm,
            DistanceMetric::EuclideanSquared,
            2,
        );
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 0);
        assert_eq!(results[1].0, 1);
    }

    #[test]
    fn test_search_skips_deleted() {
        let vm = VersionMap::new(10);
        vm.initialize(0);
        vm.initialize(1);
        vm.mark_deleted(0);

        let pl = PostingList {
            entries: vec![
                make_entry(0, 1, vec![0.0, 0.0]),
                make_entry(1, 1, vec![1.0, 0.0]),
            ],
        };

        let results = search_postings(
            &[0.0, 0.0],
            &[(0, pl)],
            &vm,
            DistanceMetric::EuclideanSquared,
            10,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn test_search_skips_stale_version() {
        let vm = VersionMap::new(10);
        vm.initialize(0);
        vm.increment_version(0);

        let pl = PostingList {
            entries: vec![
                make_entry(0, 1, vec![0.0, 0.0]),
            ],
        };

        let results = search_postings(
            &[0.0, 0.0],
            &[(0, pl)],
            &vm,
            DistanceMetric::EuclideanSquared,
            10,
        );
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_search_deduplicates() {
        let vm = VersionMap::new(10);
        vm.initialize(0);

        let pl1 = PostingList {
            entries: vec![make_entry(0, 1, vec![1.0, 0.0])],
        };
        let pl2 = PostingList {
            entries: vec![make_entry(0, 1, vec![1.0, 0.0])],
        };

        let results = search_postings(
            &[0.0, 0.0],
            &[(0, pl1), (1, pl2)],
            &vm,
            DistanceMetric::EuclideanSquared,
            10,
        );
        assert_eq!(results.len(), 1);
    }
}
