use serde::{Serialize, Deserialize};
use crate::engine::search::distance;
use crate::types::DistanceMetric;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CentroidsFile {
    pub dims: usize,
    pub next_id: u32,
    pub centroids: Vec<f32>,
    pub active: Vec<bool>,
}

pub struct HeadIndex {
    centroids: Vec<f32>,
    active: Vec<bool>,
    dims: usize,
    next_id: u32,
    count: usize,
}

impl HeadIndex {
    pub fn new(dims: usize) -> Self {
        Self {
            centroids: Vec::new(),
            active: Vec::new(),
            dims,
            next_id: 0,
            count: 0,
        }
    }

    pub fn add_centroid(&mut self, vector: &[f32]) -> u32 {
        let id = self.next_id;
        self.centroids.extend_from_slice(vector);
        self.active.push(true);
        self.next_id += 1;
        self.count += 1;
        id
    }

    pub fn remove_centroid(&mut self, head_id: u32) {
        let idx = head_id as usize;
        if idx < self.active.len() && self.active[idx] {
            self.active[idx] = false;
            self.count -= 1;
        }
    }

    pub fn update_centroid(&mut self, head_id: u32, vector: &[f32]) {
        let idx = head_id as usize;
        if idx < self.active.len() && self.active[idx] {
            let start = idx * self.dims;
            self.centroids[start..start + self.dims].copy_from_slice(vector);
        }
    }

    pub fn get_centroid(&self, head_id: u32) -> Option<&[f32]> {
        let idx = head_id as usize;
        if idx < self.active.len() && self.active[idx] {
            let start = idx * self.dims;
            Some(&self.centroids[start..start + self.dims])
        } else {
            None
        }
    }

    pub fn search(&self, query: &[f32], k: usize, metric: DistanceMetric) -> Vec<(u32, f32)> {
        let mut scored: Vec<(u32, f32)> = (0..self.next_id)
            .filter(|&id| self.active[id as usize])
            .map(|id| {
                let start = id as usize * self.dims;
                let centroid = &self.centroids[start..start + self.dims];
                (id, distance(query, centroid, metric))
            })
            .collect();

        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn next_id(&self) -> u32 {
        self.next_id
    }

    pub fn is_active(&self, head_id: u32) -> bool {
        let idx = head_id as usize;
        idx < self.active.len() && self.active[idx]
    }

    pub fn to_file(&self) -> CentroidsFile {
        CentroidsFile {
            dims: self.dims,
            next_id: self.next_id,
            centroids: self.centroids.clone(),
            active: self.active.clone(),
        }
    }

    pub fn from_file(file: CentroidsFile) -> Self {
        let count = file.active.iter().filter(|&&a| a).count();
        Self {
            dims: file.dims,
            next_id: file.next_id,
            centroids: file.centroids,
            active: file.active,
            count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_search() {
        let mut idx = HeadIndex::new(2);
        idx.add_centroid(&[0.0, 0.0]);
        idx.add_centroid(&[10.0, 10.0]);
        idx.add_centroid(&[5.0, 5.0]);

        let results = idx.search(&[0.1, 0.1], 2, DistanceMetric::EuclideanSquared);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 0);
        assert_eq!(results[1].0, 2);
    }

    #[test]
    fn test_remove_centroid() {
        let mut idx = HeadIndex::new(2);
        idx.add_centroid(&[0.0, 0.0]);
        idx.add_centroid(&[1.0, 1.0]);
        assert_eq!(idx.len(), 2);

        idx.remove_centroid(0);
        assert_eq!(idx.len(), 1);

        let results = idx.search(&[0.0, 0.0], 10, DistanceMetric::EuclideanSquared);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn test_update_centroid() {
        let mut idx = HeadIndex::new(2);
        idx.add_centroid(&[0.0, 0.0]);

        idx.update_centroid(0, &[5.0, 5.0]);
        let c = idx.get_centroid(0).unwrap();
        assert_eq!(c, &[5.0, 5.0]);
    }

    #[test]
    fn test_get_centroid_inactive() {
        let mut idx = HeadIndex::new(2);
        idx.add_centroid(&[1.0, 1.0]);
        idx.remove_centroid(0);
        assert!(idx.get_centroid(0).is_none());
    }

    #[test]
    fn test_empty_index() {
        let idx = HeadIndex::new(3);
        assert!(idx.is_empty());
        let results = idx.search(&[1.0, 2.0, 3.0], 5, DistanceMetric::CosineDistance);
        assert!(results.is_empty());
    }

    #[test]
    fn test_k_larger_than_count() {
        let mut idx = HeadIndex::new(2);
        idx.add_centroid(&[1.0, 0.0]);
        idx.add_centroid(&[0.0, 1.0]);

        let results = idx.search(&[0.0, 0.0], 100, DistanceMetric::EuclideanSquared);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_centroids_roundtrip() {
        let mut idx = HeadIndex::new(3);
        idx.add_centroid(&[1.0, 0.0, 0.0]);
        idx.add_centroid(&[0.0, 1.0, 0.0]);
        idx.add_centroid(&[0.0, 0.0, 1.0]);
        idx.remove_centroid(1);

        let file = idx.to_file();
        let bytes = bincode::serialize(&file).unwrap();
        let decoded: CentroidsFile = bincode::deserialize(&bytes).unwrap();
        let restored = HeadIndex::from_file(decoded);

        assert_eq!(restored.len(), 2);
        assert!(restored.is_active(0));
        assert!(!restored.is_active(1));
        assert!(restored.is_active(2));
        assert_eq!(restored.next_id(), 3);

        let orig_results = idx.search(&[1.0, 0.0, 0.0], 3, DistanceMetric::EuclideanSquared);
        let rest_results = restored.search(&[1.0, 0.0, 0.0], 3, DistanceMetric::EuclideanSquared);
        assert_eq!(orig_results, rest_results);
    }
}
