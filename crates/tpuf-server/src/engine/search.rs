use crate::types::DistanceMetric;

pub fn distance(a: &[f32], b: &[f32], metric: DistanceMetric) -> f32 {
    match metric {
        DistanceMetric::CosineDistance => cosine_distance(a, b),
        DistanceMetric::EuclideanSquared => euclidean_squared(a, b),
        DistanceMetric::DotProduct => dot_product_distance(a, b),
    }
}

fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        1.0
    } else {
        1.0 - (dot / denom)
    }
}

fn euclidean_squared(a: &[f32], b: &[f32]) -> f32 {
    let mut sum = 0.0f32;
    for i in 0..a.len() {
        let d = a[i] - b[i];
        sum += d * d;
    }
    sum
}

fn dot_product_distance(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
    }
    -dot
}

pub fn brute_force_knn(
    query: &[f32],
    vectors: &[(u64, &[f32])],
    metric: DistanceMetric,
    top_k: usize,
) -> Vec<(u64, f32)> {
    let mut scored: Vec<(u64, f32)> = vectors
        .iter()
        .map(|(id, v)| (*id, distance(query, v, metric)))
        .collect();
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);
    scored
}

pub fn merge_top_k(
    a: &[(u64, f32)],
    b: &[(u64, f32)],
    top_k: usize,
) -> Vec<(u64, f32)> {
    let mut merged = Vec::with_capacity(a.len() + b.len());
    merged.extend_from_slice(a);
    merged.extend_from_slice(b);
    merged.sort_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(std::cmp::Ordering::Equal));
    merged.truncate(top_k);
    merged
}
