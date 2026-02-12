pub struct KMeansResult {
    pub cluster_a: Vec<usize>,
    pub cluster_b: Vec<usize>,
    pub centroid_a: Vec<f32>,
    pub centroid_b: Vec<f32>,
}

pub fn binary_kmeans(
    vectors: &[&[f32]],
    dims: usize,
    max_iters: usize,
    tolerance: f32,
) -> KMeansResult {
    if vectors.is_empty() {
        return KMeansResult {
            cluster_a: vec![],
            cluster_b: vec![],
            centroid_a: vec![0.0; dims],
            centroid_b: vec![0.0; dims],
        };
    }

    if vectors.len() == 1 {
        return KMeansResult {
            cluster_a: vec![0],
            cluster_b: vec![],
            centroid_a: vectors[0].to_vec(),
            centroid_b: vec![0.0; dims],
        };
    }

    let mut centroid_a = vectors[0].to_vec();
    let mut centroid_b = vectors[vectors.len() - 1].to_vec();

    for _ in 0..max_iters {
        let mut assign_a = Vec::new();
        let mut assign_b = Vec::new();

        for (i, v) in vectors.iter().enumerate() {
            let da = euclidean_sq(v, &centroid_a);
            let db = euclidean_sq(v, &centroid_b);
            if da <= db {
                assign_a.push(i);
            } else {
                assign_b.push(i);
            }
        }

        if assign_a.is_empty() {
            let farthest = assign_b
                .iter()
                .max_by(|&&i, &&j| {
                    euclidean_sq(vectors[i], &centroid_b)
                        .partial_cmp(&euclidean_sq(vectors[j], &centroid_b))
                        .unwrap()
                })
                .copied()
                .unwrap();
            assign_b.retain(|&x| x != farthest);
            assign_a.push(farthest);
        } else if assign_b.is_empty() {
            let farthest = assign_a
                .iter()
                .max_by(|&&i, &&j| {
                    euclidean_sq(vectors[i], &centroid_a)
                        .partial_cmp(&euclidean_sq(vectors[j], &centroid_a))
                        .unwrap()
                })
                .copied()
                .unwrap();
            assign_a.retain(|&x| x != farthest);
            assign_b.push(farthest);
        }

        let new_a = compute_centroid(vectors, &assign_a, dims);
        let new_b = compute_centroid(vectors, &assign_b, dims);

        let shift = euclidean_sq(&new_a, &centroid_a) + euclidean_sq(&new_b, &centroid_b);

        centroid_a = new_a;
        centroid_b = new_b;

        if shift < tolerance * tolerance {
            return KMeansResult {
                cluster_a: assign_a,
                cluster_b: assign_b,
                centroid_a,
                centroid_b,
            };
        }
    }

    let mut assign_a = Vec::new();
    let mut assign_b = Vec::new();
    for (i, v) in vectors.iter().enumerate() {
        if euclidean_sq(v, &centroid_a) <= euclidean_sq(v, &centroid_b) {
            assign_a.push(i);
        } else {
            assign_b.push(i);
        }
    }

    KMeansResult {
        cluster_a: assign_a,
        cluster_b: assign_b,
        centroid_a,
        centroid_b,
    }
}

fn euclidean_sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y) * (x - y))
        .sum()
}

fn compute_centroid(vectors: &[&[f32]], indices: &[usize], dims: usize) -> Vec<f32> {
    if indices.is_empty() {
        return vec![0.0; dims];
    }
    let mut sum = vec![0.0f32; dims];
    for &i in indices {
        for (d, val) in vectors[i].iter().enumerate() {
            sum[d] += val;
        }
    }
    let n = indices.len() as f32;
    sum.iter_mut().for_each(|v| *v /= n);
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_two_clusters() {
        let a1 = [0.0f32, 0.0];
        let a2 = [0.1, 0.1];
        let b1 = [10.0, 10.0];
        let b2 = [10.1, 10.1];
        let vecs: Vec<&[f32]> = vec![&a1, &a2, &b1, &b2];

        let result = binary_kmeans(&vecs, 2, 100, 1e-6);
        let mut ca: Vec<usize> = result.cluster_a.clone();
        let mut cb: Vec<usize> = result.cluster_b.clone();
        ca.sort();
        cb.sort();

        assert!(
            (ca == vec![0, 1] && cb == vec![2, 3])
                || (ca == vec![2, 3] && cb == vec![0, 1])
        );
    }

    #[test]
    fn test_identical_vectors() {
        let v = [1.0f32, 1.0];
        let vecs: Vec<&[f32]> = vec![&v, &v, &v, &v];
        let result = binary_kmeans(&vecs, 2, 100, 1e-6);
        assert_eq!(result.cluster_a.len() + result.cluster_b.len(), 4);
    }

    #[test]
    fn test_single_vector() {
        let v = [5.0f32, 3.0];
        let vecs: Vec<&[f32]> = vec![&v];
        let result = binary_kmeans(&vecs, 2, 100, 1e-6);
        assert_eq!(result.cluster_a, vec![0]);
        assert!(result.cluster_b.is_empty());
    }

    #[test]
    fn test_empty() {
        let vecs: Vec<&[f32]> = vec![];
        let result = binary_kmeans(&vecs, 2, 100, 1e-6);
        assert!(result.cluster_a.is_empty());
        assert!(result.cluster_b.is_empty());
    }

    #[test]
    fn test_convergence() {
        let v1 = [0.0f32, 0.0];
        let v2 = [100.0, 100.0];
        let vecs: Vec<&[f32]> = vec![&v1, &v2];
        let result = binary_kmeans(&vecs, 2, 1, 1e-6);
        assert_eq!(result.cluster_a.len() + result.cluster_b.len(), 2);
    }
}
