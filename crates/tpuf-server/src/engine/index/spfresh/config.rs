use crate::types::DistanceMetric;

#[derive(Debug, Clone)]
pub struct SPFreshConfig {
    pub max_posting_size: usize,
    pub min_posting_size: usize,
    pub replica_count: usize,
    pub rng_factor: f32,
    pub num_search_heads: usize,
    pub reassign_range: usize,
    pub split_threads: usize,
    pub reassign_threads: usize,
    pub dimensions: usize,
    pub distance_metric: DistanceMetric,
}

impl Default for SPFreshConfig {
    fn default() -> Self {
        Self {
            max_posting_size: 256,
            min_posting_size: 32,
            replica_count: 4,
            rng_factor: 2.0,
            num_search_heads: 64,
            reassign_range: 64,
            split_threads: 2,
            reassign_threads: 2,
            dimensions: 128,
            distance_metric: DistanceMetric::EuclideanSquared,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = SPFreshConfig::default();
        assert_eq!(cfg.max_posting_size, 256);
        assert_eq!(cfg.min_posting_size, 32);
        assert_eq!(cfg.replica_count, 4);
        assert_eq!(cfg.dimensions, 128);
        assert_eq!(cfg.distance_metric, DistanceMetric::EuclideanSquared);
    }
}
