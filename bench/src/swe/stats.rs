//! The statistics the SWE gate rests on: an exact one-sided paired test
//! (McNemar's, as a sign test on discordant pairs) and Wilson intervals.

/// The most discordant pairs an exact tail is computed over; inputs are
/// bounded to it where they enter (the run's flags, manifests, aggregates).
pub const MAX_PAIRS: u32 = 1000;

/// P(X >= k) for X ~ Binomial(n, 1/2): the one-sided McNemar p-value of
/// `k` losses among `n` discordant pairs.
pub fn sign_test_upper_tail(k: u32, n: u32) -> f64 {
    assert!(k <= n, "losses ({k}) cannot exceed discordant pairs ({n})");
    assert!(
        n <= MAX_PAIRS,
        "an exact tail over {n} pairs is outside this bench's scale"
    );
    if k == 0 {
        return 1.0;
    }
    // Sum the pmf from the top down; pmf(n) = 2^-n, pmf(i-1) = pmf(i) * i / (n - i + 1).
    let mut pmf = 0.5f64.powi(n as i32);
    let mut tail = 0.0;
    for i in (k..=n).rev() {
        tail += pmf;
        pmf *= f64::from(i) / f64::from(n - i + 1);
    }
    let tail = tail.min(1.0);
    assert!(
        tail > 0.0 && tail <= 1.0,
        "a tail probability lies in (0, 1]"
    );
    tail
}

/// The fewest losses among `discordant` pairs that the one-sided test calls
/// significant at `alpha`, or `None` when even losing all of them is not.
pub fn critical_losses(discordant: u32, alpha: f64) -> Option<u32> {
    assert!(alpha > 0.0 && alpha < 1.0, "alpha {alpha} is a probability");
    (0..=discordant).find(|&k| sign_test_upper_tail(k, discordant) < alpha)
}

/// The 95% Wilson score interval of `passed` out of `n`, as fractions.
pub fn wilson_95(passed: u32, n: u32) -> (f64, f64) {
    assert!(passed <= n, "passed ({passed}) cannot exceed trials ({n})");
    if n == 0 {
        return (0.0, 1.0);
    }
    const Z: f64 = 1.959_963_984_540_054;
    let n = f64::from(n);
    let p = f64::from(passed) / n;
    let z2 = Z * Z;
    let centre = (p + z2 / (2.0 * n)) / (1.0 + z2 / n);
    let half = Z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / (1.0 + z2 / n);
    let (low, high) = ((centre - half).max(0.0), (centre + half).min(1.0));
    assert!(
        low <= p + 1e-12 && p <= high + 1e-12,
        "the interval holds the estimate"
    );
    (low, high)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn upper_tail_matches_hand_computed_binomials() {
        assert!(close(sign_test_upper_tail(0, 5), 1.0));
        assert!(close(sign_test_upper_tail(5, 5), 1.0 / 32.0));
        assert!(close(sign_test_upper_tail(4, 5), 6.0 / 32.0));
        assert!(close(sign_test_upper_tail(7, 8), 9.0 / 256.0));
        assert!(close(sign_test_upper_tail(1, 1), 0.5));
    }

    #[test]
    fn critical_losses_are_the_first_significant_count() {
        assert_eq!(critical_losses(4, 0.05), None, "4-0 has p = 1/16 > 0.05");
        assert_eq!(critical_losses(5, 0.05), Some(5));
        assert_eq!(critical_losses(8, 0.05), Some(7));
        assert_eq!(critical_losses(0, 0.05), None);
        let k = critical_losses(20, 0.05).unwrap();
        assert!(sign_test_upper_tail(k, 20) < 0.05 && sign_test_upper_tail(k - 1, 20) >= 0.05);
    }

    #[test]
    fn wilson_interval_matches_a_known_value_and_handles_the_edges() {
        let (low, high) = wilson_95(12, 25);
        assert!((low - 0.3002).abs() < 1e-3 && (high - 0.6650).abs() < 1e-3);
        assert_eq!(wilson_95(0, 0), (0.0, 1.0));
        let (low, high) = wilson_95(25, 25);
        assert!((high - 1.0).abs() < 1e-12 && (low - 0.8668).abs() < 1e-3);
        assert_eq!(wilson_95(0, 10).0, 0.0);
    }
}
