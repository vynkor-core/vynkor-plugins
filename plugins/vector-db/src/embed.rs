/// Deterministic fake embedder for v0.1 — hash-based, L2-normalized.
/// Replaced by real fastembed ONNX in v0.2. Keeps cosine meaningful for tests
/// without native deps or model files.
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub fn fake_embed(text: &str, dim: usize) -> Vec<f32> {
    if dim == 0 {
        return vec![];
    }
    let mut v = Vec::with_capacity(dim);
    for i in 0..dim {
        let mut hasher = DefaultHasher::new();
        // Mix text + index + length for better distribution
        text.hash(&mut hasher);
        i.hash(&mut hasher);
        dim.hash(&mut hasher);
        let h = hasher.finish();
        // Map u64 -> f32 in [-1, 1]
        let normalized = (h as f64 / u64::MAX as f64) * 2.0 - 1.0;
        v.push(normalized as f32);
    }
    // Add text-length bias so different lengths don't collide too much
    // Apply simple word-hash perturbation for semantic-ish behavior:
    // hash each word and add to vector cyclically
    for word in text.split_whitespace() {
        let mut hasher = DefaultHasher::new();
        word.hash(&mut hasher);
        let h = hasher.finish() as usize;
        let idx = h % dim;
        let mut wh = DefaultHasher::new();
        word.len().hash(&mut wh);
        let perturb = (wh.finish() % 1000) as f32 / 1000.0;
        v[idx] += perturb;
    }
    normalize(&mut v);
    v
}

pub fn normalize(v: &mut [f32]) -> bool {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm < 1e-9 {
        return false;
    }
    for x in v.iter_mut() {
        *x /= norm;
    }
    true
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    // Assume normalized -> dot product == cosine
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

pub fn validate_and_normalize_vector(
    mut v: Vec<f32>,
    expected_dim: Option<usize>,
) -> Result<Vec<f32>, String> {
    if v.is_empty() {
        return Err("vector must be non-empty".to_string());
    }
    if v.len() > 4096 {
        return Err(format!("vector dim too large: {} > 4096", v.len()));
    }
    if let Some(dim) = expected_dim {
        if v.len() != dim {
            return Err(format!(
                "dimension mismatch: expected {dim}, got {}",
                v.len()
            ));
        }
    }
    for (i, x) in v.iter().enumerate() {
        if !x.is_finite() {
            return Err(format!("vector[{i}] is not finite: {x}"));
        }
    }
    if !normalize(&mut v) {
        return Err("vector has zero norm (cannot normalize)".to_string());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_embed_is_deterministic() {
        let a = fake_embed("hello world", 8);
        let b = fake_embed("hello world", 8);
        assert_eq!(a, b);
    }

    #[test]
    fn fake_embed_different_text_different_vector() {
        let a = fake_embed("hello", 8);
        let b = fake_embed("world", 8);
        assert_ne!(a, b);
    }

    #[test]
    fn fake_embed_is_normalized() {
        let v = fake_embed("test", 16);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_identical_is_one() {
        let v = fake_embed("same", 32);
        let s = cosine_similarity(&v, &v);
        assert!((s - 1.0).abs() < 1e-5);
    }

    #[test]
    fn validate_rejects_mismatch() {
        let v = vec![0.5, 0.5, 0.5];
        assert!(validate_and_normalize_vector(v, Some(4)).is_err());
    }

    #[test]
    fn validate_normalizes() {
        let v = vec![3.0, 4.0];
        let out = validate_and_normalize_vector(v, None).unwrap();
        assert!((out[0] - 0.6).abs() < 1e-5);
        assert!((out[1] - 0.8).abs() < 1e-5);
    }

    #[test]
    fn validate_rejects_non_finite() {
        let v = vec![f32::INFINITY, 0.0];
        assert!(validate_and_normalize_vector(v, None).is_err());
    }
}
