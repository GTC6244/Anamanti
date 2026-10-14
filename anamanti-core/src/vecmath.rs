//! Small dense-vector helpers shared by the speaker voiceprints
//! ([`crate::speaker`]) and the memory embedders ([`crate::memory::embed`]).

/// Cosine similarity between two equal-length vectors. Vectors from the embedders
/// are usually already L2-normalized, but this divides by the norms defensively so
/// a stray unnormalized input can't exceed 1.0. Mismatched lengths (e.g. a
/// model/dims change) and zero vectors score `0.0` rather than panicking.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = norm(a);
    let nb = norm(b);
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// The Euclidean (L2) norm of `v`.
pub fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

/// L2-normalize `v` (returns the normalized vector). A (near-)zero vector — e.g.
/// silence — is returned unchanged, so a cosine against it is a well-defined 0.
pub fn l2_normalize(mut v: Vec<f32>) -> Vec<f32> {
    l2_normalize_in_place(&mut v);
    v
}

/// In-place form of [`l2_normalize`]. Returns `false` (leaving `v` untouched) when
/// `v` is a (near-)zero vector that has no direction to normalize.
pub fn l2_normalize_in_place(v: &mut [f32]) -> bool {
    let n = norm(v);
    if n <= f32::EPSILON {
        return false;
    }
    for x in v.iter_mut() {
        *x /= n;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_is_scale_invariant_and_defensive() {
        assert!((cosine(&[1.0, 0.0], &[3.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 2.0]).abs() < 1e-6);
        assert_eq!(cosine(&[1.0, 0.0], &[1.0]), 0.0, "length mismatch");
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0, "zero vector");
    }

    #[test]
    fn normalize_yields_unit_length_and_keeps_zero() {
        let v = l2_normalize(vec![3.0, 4.0]);
        assert!((norm(&v) - 1.0).abs() < 1e-6);
        assert_eq!(l2_normalize(vec![0.0, 0.0]), vec![0.0, 0.0]);
        let mut z = [0.0f32; 3];
        assert!(!l2_normalize_in_place(&mut z));
    }
}
