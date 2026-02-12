use crate::engine::search::distance;
use crate::types::DistanceMetric;
use super::config::SPFreshConfig;
use super::head_index::HeadIndex;
use super::kmeans::binary_kmeans;
use super::posting::{PostingEntry, PostingList};
use super::posting_store::PostingStore;
use super::updater::rng_filter;
use super::version_map::VersionMap;

const MAX_SPLIT_DEPTH: usize = 10;

pub fn split_posting(
    head_id: u32,
    head_index: &mut HeadIndex,
    posting_store: &dyn PostingStore,
    version_map: &VersionMap,
    config: &SPFreshConfig,
) -> Vec<u32> {
    let posting = match posting_store.get(head_id) {
        Some(p) => p,
        None => return vec![],
    };

    let clean: Vec<&PostingEntry> = posting
        .entries
        .iter()
        .filter(|e| {
            !version_map.is_deleted(e.vector_id)
                && version_map.get_version(e.vector_id) == e.version
        })
        .collect();

    if clean.len() <= config.max_posting_size {
        let cleaned = PostingList {
            entries: clean.into_iter().cloned().collect(),
        };
        posting_store.put(head_id, cleaned);
        return vec![];
    }

    let vectors: Vec<&[f32]> = clean.iter().map(|e| e.vector.as_slice()).collect();
    let result = binary_kmeans(&vectors, config.dimensions, 50, 1e-6);

    let old_centroid = match head_index.get_centroid(head_id) {
        Some(c) => c.to_vec(),
        None => return vec![],
    };

    let dist_a = distance(&result.centroid_a, &old_centroid, DistanceMetric::EuclideanSquared);
    let dist_b = distance(&result.centroid_b, &old_centroid, DistanceMetric::EuclideanSquared);

    let mut new_head_ids = Vec::new();

    let (head_a, head_b) = if dist_a < 1e-6 {
        let hb = head_index.add_centroid(&result.centroid_b);
        head_index.update_centroid(head_id, &result.centroid_a);
        new_head_ids.push(hb);
        (head_id, hb)
    } else if dist_b < 1e-6 {
        let ha = head_index.add_centroid(&result.centroid_a);
        head_index.update_centroid(head_id, &result.centroid_b);
        new_head_ids.push(ha);
        (ha, head_id)
    } else {
        let ha = head_index.add_centroid(&result.centroid_a);
        let hb = head_index.add_centroid(&result.centroid_b);
        head_index.remove_centroid(head_id);
        posting_store.delete_posting(head_id);
        new_head_ids.push(ha);
        new_head_ids.push(hb);
        (ha, hb)
    };

    let list_a = PostingList {
        entries: result
            .cluster_a
            .iter()
            .map(|&i| clean[i].clone())
            .collect(),
    };
    let list_b = PostingList {
        entries: result
            .cluster_b
            .iter()
            .map(|&i| clean[i].clone())
            .collect(),
    };

    posting_store.put(head_a, list_a);
    posting_store.put(head_b, list_b);

    new_head_ids
}

pub fn reassign_after_split(
    old_centroid: &[f32],
    new_heads: &[u32],
    head_index: &mut HeadIndex,
    posting_store: &dyn PostingStore,
    version_map: &VersionMap,
    config: &SPFreshConfig,
) {
    let neighbors = head_index.search(
        old_centroid,
        config.reassign_range,
        config.distance_metric,
    );

    let new_centroids: Vec<(u32, Vec<f32>)> = new_heads
        .iter()
        .filter_map(|&hid| {
            head_index.get_centroid(hid).map(|c| (hid, c.to_vec()))
        })
        .collect();

    if new_centroids.is_empty() {
        return;
    }

    let mut candidates: Vec<(u64, Vec<f32>)> = Vec::new();

    for (neighbor_id, _) in &neighbors {
        if new_heads.contains(neighbor_id) {
            continue;
        }

        let posting = match posting_store.get(*neighbor_id) {
            Some(p) => p,
            None => continue,
        };

        for entry in &posting.entries {
            if version_map.is_deleted(entry.vector_id) {
                continue;
            }
            if version_map.get_version(entry.vector_id) != entry.version {
                continue;
            }

            let dist_old = distance(&entry.vector, old_centroid, config.distance_metric);
            let dominated_by_new = new_centroids.iter().any(|(_, nc)| {
                distance(&entry.vector, nc, config.distance_metric) <= dist_old
            });

            if dominated_by_new {
                candidates.push((entry.vector_id, entry.vector.clone()));
            }
        }
    }

    for (vid, vector) in candidates {
        let new_version = match version_map.increment_version(vid) {
            Some(v) => v,
            None => continue,
        };

        let search_k = config.replica_count * 2;
        let nearest = head_index.search(&vector, search_k, config.distance_metric);
        let selected = rng_filter(
            &nearest,
            head_index,
            config.distance_metric,
            config.rng_factor,
            config.replica_count,
        );

        let entry = PostingEntry {
            vector_id: vid,
            version: new_version,
            vector,
        };

        for &hid in &selected {
            posting_store.append(hid, &[entry.clone()]);
        }
    }
}

pub fn process_splits(
    oversized: &[u32],
    head_index: &mut HeadIndex,
    posting_store: &dyn PostingStore,
    version_map: &VersionMap,
    config: &SPFreshConfig,
) {
    process_splits_recursive(oversized, head_index, posting_store, version_map, config, 0);
}

fn process_splits_recursive(
    oversized: &[u32],
    head_index: &mut HeadIndex,
    posting_store: &dyn PostingStore,
    version_map: &VersionMap,
    config: &SPFreshConfig,
    depth: usize,
) {
    if depth >= MAX_SPLIT_DEPTH {
        return;
    }

    for &head_id in oversized {
        if posting_store.get_size(head_id) <= config.max_posting_size {
            continue;
        }

        let old_centroid = match head_index.get_centroid(head_id) {
            Some(c) => c.to_vec(),
            None => continue,
        };

        let new_heads = split_posting(head_id, head_index, posting_store, version_map, config);

        if !new_heads.is_empty() {
            reassign_after_split(
                &old_centroid,
                &new_heads,
                head_index,
                posting_store,
                version_map,
                config,
            );

            let still_oversized: Vec<u32> = new_heads
                .iter()
                .copied()
                .filter(|&hid| posting_store.get_size(hid) > config.max_posting_size)
                .collect();

            if !still_oversized.is_empty() {
                process_splits_recursive(
                    &still_oversized,
                    head_index,
                    posting_store,
                    version_map,
                    config,
                    depth + 1,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::index::spfresh::posting_store::MemoryPostingStore;

    fn make_config(dims: usize, max_posting: usize) -> SPFreshConfig {
        SPFreshConfig {
            dimensions: dims,
            max_posting_size: max_posting,
            min_posting_size: 4,
            replica_count: 2,
            rng_factor: 2.0,
            num_search_heads: 16,
            reassign_range: 16,
            distance_metric: DistanceMetric::EuclideanSquared,
            ..Default::default()
        }
    }

    #[test]
    fn test_split_small_posting_no_split() {
        let config = make_config(2, 10);
        let mut hi = HeadIndex::new(2);
        hi.add_centroid(&[0.0, 0.0]);

        let store = MemoryPostingStore::new();
        let vm = VersionMap::new(5);
        for i in 0..5u64 {
            vm.initialize(i);
            store.append(
                0,
                &[PostingEntry {
                    vector_id: i,
                    version: 1,
                    vector: vec![i as f32, 0.0],
                }],
            );
        }

        let new_heads = split_posting(0, &mut hi, &store, &vm, &config);
        assert!(new_heads.is_empty());
        assert_eq!(hi.len(), 1);
    }

    #[test]
    fn test_split_oversized_posting() {
        let config = make_config(2, 5);
        let mut hi = HeadIndex::new(2);
        hi.add_centroid(&[5.0, 5.0]);

        let store = MemoryPostingStore::new();
        let vm = VersionMap::new(20);
        for i in 0..10u64 {
            vm.initialize(i);
            let v = if i < 5 {
                vec![0.0 + i as f32 * 0.1, 0.0]
            } else {
                vec![10.0 + i as f32 * 0.1, 10.0]
            };
            store.append(
                0,
                &[PostingEntry {
                    vector_id: i,
                    version: 1,
                    vector: v,
                }],
            );
        }

        let new_heads = split_posting(0, &mut hi, &store, &vm, &config);
        assert!(!new_heads.is_empty());
        assert!(hi.len() >= 2);
    }

    #[test]
    fn test_split_gc_resolves_oversized() {
        let config = make_config(2, 10);
        let mut hi = HeadIndex::new(2);
        hi.add_centroid(&[0.0, 0.0]);

        let store = MemoryPostingStore::new();
        let vm = VersionMap::new(20);

        for i in 0..15u64 {
            vm.initialize(i);
            store.append(
                0,
                &[PostingEntry {
                    vector_id: i,
                    version: 1,
                    vector: vec![i as f32, 0.0],
                }],
            );
        }
        for i in 0..10u64 {
            vm.mark_deleted(i);
        }

        let new_heads = split_posting(0, &mut hi, &store, &vm, &config);
        assert!(new_heads.is_empty());
        assert_eq!(store.get_size(0), 5);
    }

    #[test]
    fn test_process_splits_recursive() {
        let config = make_config(2, 4);
        let mut hi = HeadIndex::new(2);
        hi.add_centroid(&[5.0, 5.0]);

        let store = MemoryPostingStore::new();
        let vm = VersionMap::new(20);

        for i in 0..16u64 {
            vm.initialize(i);
            let v = vec![(i as f32) * 1.0, (i as f32) * 1.0];
            store.append(
                0,
                &[PostingEntry {
                    vector_id: i,
                    version: 1,
                    vector: v,
                }],
            );
        }

        process_splits(&[0], &mut hi, &store, &vm, &config);
        assert!(hi.len() >= 2);
    }

    #[test]
    fn test_split_produces_balanced_clusters() {
        let config = make_config(2, 10);
        let mut hi = HeadIndex::new(2);
        hi.add_centroid(&[5.0, 5.0]);

        let store = MemoryPostingStore::new();
        let vm = VersionMap::new(30);

        // Two well-separated groups: 10 near (0,0), 10 near (20,20)
        for i in 0..20u64 {
            vm.initialize(i);
            let v = if i < 10 {
                vec![i as f32 * 0.1, i as f32 * 0.1]
            } else {
                vec![20.0 + (i - 10) as f32 * 0.1, 20.0 + (i - 10) as f32 * 0.1]
            };
            store.append(0, &[PostingEntry {
                vector_id: i,
                version: 1,
                vector: v,
            }]);
        }

        let new_heads = split_posting(0, &mut hi, &store, &vm, &config);
        assert!(!new_heads.is_empty());

        // Collect sizes of all active postings created by split
        let mut sizes = Vec::new();
        for hid in 0..hi.len() as u32 + 5 {
            let sz = store.get_size(hid);
            if sz > 0 {
                sizes.push(sz);
            }
        }
        assert!(sizes.len() >= 2, "split should produce ≥2 postings");

        let min_sz = *sizes.iter().min().unwrap();
        let max_sz = *sizes.iter().max().unwrap();
        let total: usize = sizes.iter().sum();
        let min_ratio = min_sz as f64 / total as f64;

        // Neither cluster should be <10% of total
        assert!(
            min_ratio >= 0.10,
            "unbalanced split: sizes={sizes:?}, min_ratio={min_ratio:.2}"
        );
        // Sanity: total vectors preserved
        assert_eq!(total, 20, "all 20 vectors should survive split, got {total}");
    }

    #[test]
    fn test_gc_preserves_latest_version_entries() {
        // Simulate reassignment: vector 0 was version 1, then reassigned to version 2.
        // Old posting has stale version=1 entry. New posting has version=2 entry.
        // GC during split should remove the stale entry but keep the current one.
        let config = make_config(2, 5);
        let mut hi = HeadIndex::new(2);
        let h0 = hi.add_centroid(&[0.0, 0.0]);
        let h1 = hi.add_centroid(&[10.0, 10.0]);

        let store = MemoryPostingStore::new();
        let vm = VersionMap::new(20);

        // Vector 0: current version is 2 (was reassigned)
        vm.initialize(0);
        vm.increment_version(0); // now version 2

        // Stale entry in h0 (version 1, but current is 2)
        store.append(h0, &[PostingEntry {
            vector_id: 0,
            version: 1,
            vector: vec![0.5, 0.5],
        }]);
        // Current entry in h1 (version 2)
        store.append(h1, &[PostingEntry {
            vector_id: 0,
            version: 2,
            vector: vec![10.5, 10.5],
        }]);

        // Add enough vectors to h0 to make it oversized (need >5 after GC)
        for i in 1..10u64 {
            vm.initialize(i);
            store.append(h0, &[PostingEntry {
                vector_id: i,
                version: 1,
                vector: vec![i as f32 * 0.1, 0.0],
            }]);
        }

        // h0 has 10 entries (1 stale + 9 valid). After GC: 9 valid (still >5 → split)
        split_posting(h0, &mut hi, &store, &vm, &config);

        // The stale entry for vector 0 should be gone from all h0-derived postings
        // The current entry in h1 should be untouched
        let h1_posting = store.get(h1).unwrap();
        assert_eq!(h1_posting.len(), 1);
        assert_eq!(h1_posting.entries[0].vector_id, 0);
        assert_eq!(h1_posting.entries[0].version, 2);

        // Vector 0 should NOT appear in any posting derived from h0
        // (it was stale there — version 1 vs current version 2)
        let mut found_v0_in_h0_descendants = false;
        for hid in 0..10u32 {
            if hid == h1 {
                continue;
            }
            if let Some(pl) = store.get(hid) {
                for e in &pl.entries {
                    if e.vector_id == 0 {
                        found_v0_in_h0_descendants = true;
                    }
                }
            }
        }
        assert!(
            !found_v0_in_h0_descendants,
            "stale version of vector 0 should be GC'd from h0-derived postings"
        );
    }

    #[test]
    fn test_reassign_fixes_npa_violation() {
        // Scenario:
        // - Head A at (0,0) owns vectors near (0,0)
        // - Head B at (10,0) owns vectors near (10,0), plus a "boundary" vector at (4,0)
        // - Head A splits into A1(0,0) and A2(3,0)
        // - After split, the boundary vector (4,0) in B is closer to A2(3,0) than to B(10,0)
        //   → NPA violation → reassign should move it
        let config = SPFreshConfig {
            dimensions: 2,
            max_posting_size: 4,
            min_posting_size: 1,
            replica_count: 1,
            rng_factor: 0.0, // disable RNG filter (accept all)
            num_search_heads: 16,
            reassign_range: 16,
            distance_metric: DistanceMetric::EuclideanSquared,
            ..Default::default()
        };

        let mut hi = HeadIndex::new(2);
        let ha = hi.add_centroid(&[0.0, 0.0]); // head 0
        let hb = hi.add_centroid(&[10.0, 0.0]); // head 1

        let store = MemoryPostingStore::new();
        let vm = VersionMap::new(20);

        // Head A: vectors at (-1,0), (0,0), (1,0), (2,0), (3,0) → 5 entries, oversized (>4)
        for i in 0..5u64 {
            vm.initialize(i);
            store.append(ha, &[PostingEntry {
                vector_id: i,
                version: 1,
                vector: vec![(i as f32) - 1.0, 0.0],
            }]);
        }

        // Head B: boundary vector at (4,0) + vectors at (9,0), (10,0), (11,0)
        for i in 5..9u64 {
            vm.initialize(i);
            let x = if i == 5 { 4.0 } else { 7.0 + (i - 6) as f32 };
            store.append(hb, &[PostingEntry {
                vector_id: i,
                version: 1,
                vector: vec![x, 0.0],
            }]);
        }

        // Before: boundary vector 5 at (4,0) is in head B (centroid 10,0). Distance = 36.
        let old_centroid = hi.get_centroid(ha).unwrap().to_vec();

        // Split head A (oversized: 5 > 4)
        let new_heads = split_posting(ha, &mut hi, &store, &vm, &config);
        assert!(!new_heads.is_empty(), "split should produce new heads");

        // Run reassignment
        reassign_after_split(&old_centroid, &new_heads, &mut hi, &store, &vm, &config);

        // After reassign: vector 5 (at 4,0) should have been reassigned (version incremented)
        let v5_version = vm.get_version(5);
        assert!(
            v5_version > 1,
            "boundary vector 5 should be reassigned (version should be >1, got {v5_version})"
        );

        // Vector 5 should now appear in a posting whose centroid is closer to (4,0)
        // than head B's centroid (10,0). Find where it ended up.
        let mut found_closer = false;
        for hid in 0..hi.len() as u32 + 5 {
            if let Some(pl) = store.get(hid) {
                for e in &pl.entries {
                    if e.vector_id == 5 && e.version == v5_version {
                        if let Some(centroid) = hi.get_centroid(hid) {
                            let dist_new = distance(&e.vector, centroid, config.distance_metric);
                            let dist_b = distance(&e.vector, &[10.0, 0.0], config.distance_metric);
                            if dist_new < dist_b {
                                found_closer = true;
                            }
                        }
                    }
                }
            }
        }
        assert!(
            found_closer,
            "vector 5 at (4,0) should be reassigned to a head closer than B(10,0)"
        );
    }
}
