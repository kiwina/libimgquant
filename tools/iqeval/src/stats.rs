//! Paired statistics for comparing two runs over the same images.

/// Two-sided p-value of the Wilcoxon signed-rank test for paired differences.
///
/// Zero differences are dropped (Wilcoxon's original convention) and tied magnitudes get
/// their average rank. The p-value is exact: the null distribution of the positive rank
/// sum is enumerated over all 2^n sign assignments of the actual ranks (doubled to make
/// average ranks integers), so it stays correct with ties and small samples. Returns
/// `None` when every difference is zero.
#[must_use]
pub fn wilcoxon_signed_rank(diffs: &[f64]) -> Option<f64> {
    let mut d: Vec<f64> = diffs.iter().copied().filter(|v| *v != 0.0 && v.is_finite()).collect();
    if d.is_empty() {
        return None;
    }
    d.sort_by(|a, b| a.abs().total_cmp(&b.abs()));
    // doubled average ranks: tied group at 1-based positions i+1..=j gets (i+1+j)
    let n = d.len();
    let mut ranks2 = vec![0usize; n];
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n && d[j].abs() == d[i].abs() {
            j += 1;
        }
        for r in &mut ranks2[i..j] {
            *r = i + 1 + j;
        }
        i = j;
    }
    let w_plus: usize = d.iter().zip(&ranks2).filter(|(v, _)| **v > 0.0).map(|(_, r)| r).sum();
    let total: usize = ranks2.iter().sum();
    // dist[s] = probability that the doubled positive rank sum equals s under H0
    let mut dist = vec![0.0f64; total + 1];
    dist[0] = 1.0;
    let mut reach = 0;
    for &r in &ranks2 {
        reach += r;
        for s in (0..=reach).rev() {
            let with = if s >= r { dist[s - r] } else { 0.0 };
            dist[s] = 0.5 * (dist[s] + with);
        }
    }
    let lower: f64 = dist[..=w_plus].iter().sum();
    let upper: f64 = dist[w_plus..].iter().sum();
    Some((2.0 * lower.min(upper)).min(1.0))
}

/// Geometric mean of `new / old` over pairs where both are positive.
#[must_use]
pub fn geomean_ratio(pairs: &[(f64, f64)]) -> Option<f64> {
    let logs: Vec<f64> = pairs
        .iter()
        .filter(|(o, n)| *o > 0.0 && *n > 0.0 && o.is_finite() && n.is_finite())
        .map(|(o, n)| (n / o).ln())
        .collect();
    (!logs.is_empty()).then(|| (logs.iter().sum::<f64>() / logs.len() as f64).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wilcoxon_exact_small_samples() {
        // all 5 differences positive: W+ = 15 is the single most extreme of 32 outcomes
        let p = wilcoxon_signed_rank(&[1.0, 2.0, 3.0, 4.0, 5.0]).unwrap();
        assert!((p - 2.0 / 32.0).abs() < 1e-12);
        let p = wilcoxon_signed_rank(&[-1.0, -2.0, -3.0, -4.0, -5.0]).unwrap();
        assert!((p - 2.0 / 32.0).abs() < 1e-12);
        // n = 10, all positive: p = 2 / 1024
        let d: Vec<f64> = (1..=10).map(f64::from).collect();
        assert!((wilcoxon_signed_rank(&d).unwrap() - 2.0 / 1024.0).abs() < 1e-12);
        // W+ = 1 of n = 3 (ranks 1,2,3): sums <= 1 are {} and {1}, so p = 2 * 2/8
        assert!((wilcoxon_signed_rank(&[0.1, -0.2, -0.3]).unwrap() - 0.5).abs() < 1e-12);
    }

    #[test]
    fn wilcoxon_symmetric_and_degenerate() {
        assert_eq!(wilcoxon_signed_rank(&[0.0, 0.0]), None);
        // perfectly balanced signs give p = 1
        let p = wilcoxon_signed_rank(&[1.0, -1.0, 2.0, -2.0]).unwrap();
        assert!((p - 1.0).abs() < 1e-12);
        // zeros are dropped, ties averaged: [1, 1, 1] all positive -> p = 2/8
        let p = wilcoxon_signed_rank(&[0.0, 1.0, 1.0, 1.0]).unwrap();
        assert!((p - 0.25).abs() < 1e-12);
    }

    #[test]
    fn wilcoxon_matches_brute_force() {
        let d = [0.5, -1.5, 2.0, 2.0, -3.0, 4.5, 0.25, -0.25, 6.0];
        // brute force over all sign flips of the same magnitudes
        let mut mags: Vec<f64> = d.iter().map(|v: &f64| v.abs()).collect();
        mags.sort_by(f64::total_cmp);
        let rank = |m: f64| {
            let lo = mags.iter().filter(|&&x| x < m).count() as f64;
            let eq = mags.iter().filter(|&&x| x == m).count() as f64;
            lo + (eq + 1.0) / 2.0
        };
        let w = |signs: u32| -> f64 {
            (0..d.len())
                .filter(|i| signs >> i & 1 == 1)
                .map(|i| rank(d[i].abs()))
                .sum()
        };
        let observed: f64 = d.iter().filter(|v| **v > 0.0).map(|v| rank(v.abs())).sum();
        let total = 1u32 << d.len();
        let le = (0..total).filter(|&s| w(s) <= observed + 1e-9).count() as f64 / f64::from(total);
        let ge = (0..total).filter(|&s| w(s) >= observed - 1e-9).count() as f64 / f64::from(total);
        let expected = (2.0 * le.min(ge)).min(1.0);
        assert!((wilcoxon_signed_rank(&d).unwrap() - expected).abs() < 1e-12);
    }

    #[test]
    fn geomean() {
        let r = geomean_ratio(&[(100.0, 50.0), (10.0, 20.0)]).unwrap();
        assert!((r - 1.0).abs() < 1e-12);
        let r = geomean_ratio(&[(1.0, 1.1), (2.0, 2.2), (0.0, 5.0)]).unwrap();
        assert!((r - 1.1).abs() < 1e-12);
        assert_eq!(geomean_ratio(&[]), None);
    }
}
