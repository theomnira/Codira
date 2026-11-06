//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - The statistics behind `codira bench`: robust location and spread (median,
//!   median absolute deviation), a distribution-free confidence interval for
//!   the median, Tukey's outlier fences, and the Mann-Whitney U test used to
//!   decide whether two sets of samples differ.

/// The `q`-quantile of sorted `xs` (linear interpolation between ranks).
pub(crate) fn quantile(sorted: &[f64], q: f64) -> f64 {
    assert!(!sorted.is_empty());
    let pos = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

fn sorted(xs: &[f64]) -> Vec<f64> {
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    v
}

pub(crate) fn median(xs: &[f64]) -> f64 {
    quantile(&sorted(xs), 0.5)
}

/// The median absolute deviation, scaled by 1.4826 so that it estimates the
/// standard deviation of normally distributed samples.
pub(crate) fn mad(xs: &[f64]) -> f64 {
    let m = median(xs);
    let deviations: Vec<f64> = xs.iter().map(|x| (x - m).abs()).collect();
    1.4826 * median(&deviations)
}

/// A 95% confidence interval for the median that assumes nothing about the
/// distribution: the order statistics whose ranks bracket the median with
/// probability 0.95 under the binomial(n, 1/2) law of the number of samples
/// below it (normal approximation). With fewer than 6 samples it is the
/// whole range.
pub(crate) fn median_ci(xs: &[f64]) -> (f64, f64) {
    let s = sorted(xs);
    let n = s.len();
    if n < 6 {
        return (s[0], s[n - 1]);
    }
    let half = 1.96 * (n as f64).sqrt() / 2.0;
    let center = n as f64 / 2.0;
    let lo = (center - half).floor().max(0.0) as usize;
    let hi = ((center + half).ceil() as usize).min(n - 1);
    (s[lo], s[hi])
}

/// How many samples lie outside Tukey's fences: (mild, severe) for 1.5 and 3
/// interquartile ranges beyond the quartiles.
pub(crate) fn outliers(xs: &[f64]) -> (usize, usize) {
    let s = sorted(xs);
    let q1 = quantile(&s, 0.25);
    let q3 = quantile(&s, 0.75);
    let iqr = q3 - q1;
    let mut mild = 0;
    let mut severe = 0;
    for &x in &s {
        if x < q1 - 3.0 * iqr || x > q3 + 3.0 * iqr {
            severe += 1;
        } else if x < q1 - 1.5 * iqr || x > q3 + 1.5 * iqr {
            mild += 1;
        }
    }
    (mild, severe)
}

/// The mean of the samples within the severe-outlier fences.
pub(crate) fn trimmed_mean(xs: &[f64]) -> f64 {
    let s = sorted(xs);
    let q1 = quantile(&s, 0.25);
    let q3 = quantile(&s, 0.75);
    let iqr = q3 - q1;
    let kept: Vec<f64> = s
        .into_iter()
        .filter(|&x| x >= q1 - 3.0 * iqr && x <= q3 + 3.0 * iqr)
        .collect();
    kept.iter().sum::<f64>() / kept.len().max(1) as f64
}

/// The two-sided p-value of the Mann-Whitney U test that `a` and `b` come
/// from the same distribution (normal approximation with a correction for
/// ties and for continuity). Meaningful from about 8 samples each.
pub(crate) fn mann_whitney_p(a: &[f64], b: &[f64]) -> f64 {
    let n1 = a.len() as f64;
    let n2 = b.len() as f64;
    if a.is_empty() || b.is_empty() {
        return 1.0;
    }
    // Rank the pooled samples, giving ties their average rank.
    let mut pooled: Vec<(f64, bool)> = a
        .iter()
        .map(|&x| (x, true))
        .chain(b.iter().map(|&x| (x, false)))
        .collect();
    pooled.sort_by(|x, y| x.0.total_cmp(&y.0));
    let n = pooled.len();
    let mut rank_sum_a = 0.0;
    let mut tie_term = 0.0;
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && pooled[j + 1].0 == pooled[i].0 {
            j += 1;
        }
        let rank = (i + j) as f64 / 2.0 + 1.0;
        let t = (j - i + 1) as f64;
        tie_term += t * t * t - t;
        for item in &pooled[i..=j] {
            if item.1 {
                rank_sum_a += rank;
            }
        }
        i = j + 1;
    }
    let u = rank_sum_a - n1 * (n1 + 1.0) / 2.0;
    let mean = n1 * n2 / 2.0;
    let nn = n1 + n2;
    let variance = n1 * n2 / 12.0 * ((nn + 1.0) - tie_term / (nn * (nn - 1.0)));
    if variance <= 0.0 {
        return 1.0;
    }
    let z = ((u - mean).abs() - 0.5).max(0.0) / variance.sqrt();
    (2.0 * (1.0 - normal_cdf(z))).clamp(0.0, 1.0)
}

/// The standard normal distribution function (Abramowitz and Stegun
/// 26.2.17, absolute error below 7.5e-8).
pub(crate) fn normal_cdf(z: f64) -> f64 {
    if z < 0.0 {
        return 1.0 - normal_cdf(-z);
    }
    let t = 1.0 / (1.0 + 0.231_641_9 * z);
    let poly = t
        * (0.319_381_530
            + t * (-0.356_563_782
                + t * (1.781_477_937 + t * (-1.821_255_978 + t * 1.330_274_429))));
    1.0 - (-z * z / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt() * poly
}

/// The range of `xs` relative to their median: 0 for one value.
pub(crate) fn relative_spread(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = median(xs);
    if m <= 0.0 {
        return 0.0;
    }
    let s = sorted(xs);
    (s[s.len() - 1] - s[0]) / m
}

/// What a comparison with a baseline concludes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Regressed,
    Improved,
    NoChange,
}

/// Compares `new` samples with `old` ones: a change is reported only when
/// the Mann-Whitney test rejects "same distribution" at `alpha` *and* the
/// medians differ by more than `threshold` (a fraction, 0.02 = 2%). Returns
/// the verdict, the relative change of the median and the p-value.
pub(crate) fn compare(old: &[f64], new: &[f64], alpha: f64, threshold: f64) -> (Verdict, f64, f64) {
    let old_median = median(old);
    let new_median = median(new);
    let change = if old_median > 0.0 {
        (new_median - old_median) / old_median
    } else {
        0.0
    };
    let p = mann_whitney_p(old, new);
    let verdict = if p >= alpha || change.abs() <= threshold {
        Verdict::NoChange
    } else if change > 0.0 {
        Verdict::Regressed
    } else {
        Verdict::Improved
    };
    (verdict, change, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_and_spread() {
        let xs = [5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(median(&xs), 3.0);
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert!((mad(&xs) - 1.4826).abs() < 1e-9);
        assert_eq!(quantile(&[1.0, 2.0, 3.0, 4.0, 5.0], 0.25), 2.0);
    }

    #[test]
    fn median_ci_brackets_the_median() {
        let xs: Vec<f64> = (1..=100).map(f64::from).collect();
        let (lo, hi) = median_ci(&xs);
        assert!(lo < 50.5 && 50.5 < hi);
        // n = 100: ranks 50 -/+ 9.8 -> about 40 and 60.
        assert!((39.0..=42.0).contains(&lo), "{lo}");
        assert!((59.0..=62.0).contains(&hi), "{hi}");
        assert_eq!(median_ci(&[3.0, 1.0, 2.0]), (1.0, 3.0));
    }

    #[test]
    fn tukey_fences() {
        let mut xs: Vec<f64> = (0..20).map(|i| 10.0 + f64::from(i % 5) * 0.1).collect();
        xs.push(10.9);
        xs.push(100.0);
        assert_eq!(outliers(&xs), (1, 1));
        assert!((trimmed_mean(&xs) - 10.2).abs() < 0.1);
    }

    #[test]
    fn spread() {
        assert_eq!(relative_spread(&[10.0]), 0.0);
        assert!((relative_spread(&[9.0, 10.0, 11.0]) - 0.2).abs() < 1e-12);
    }

    #[test]
    fn normal_distribution_function() {
        assert!((normal_cdf(0.0) - 0.5).abs() < 1e-7);
        assert!((normal_cdf(1.96) - 0.975).abs() < 1e-4);
        assert!((normal_cdf(-1.0) - 0.158_655).abs() < 1e-5);
    }

    #[test]
    fn mann_whitney_separates_shifted_samples_only() {
        let a: Vec<f64> = (0..30).map(|i| 100.0 + f64::from(i % 7)).collect();
        let same: Vec<f64> = (0..30).map(|i| 100.0 + f64::from((i + 3) % 7)).collect();
        let slower: Vec<f64> = a.iter().map(|x| x * 1.10).collect();
        assert!(mann_whitney_p(&a, &same) > 0.5);
        assert!(mann_whitney_p(&a, &slower) < 0.001);
        // All ties: no evidence of a difference.
        assert_eq!(mann_whitney_p(&[1.0; 10], &[1.0; 10]), 1.0);
    }

    #[test]
    fn compare_needs_significance_and_size() {
        let base: Vec<f64> = (0..40).map(|i| 100.0 + f64::from(i % 9)).collect();
        let slower: Vec<f64> = base.iter().map(|x| x * 1.2).collect();
        let faster: Vec<f64> = base.iter().map(|x| x * 0.8).collect();
        let tiny: Vec<f64> = base.iter().map(|x| x * 1.005).collect();
        assert_eq!(compare(&base, &slower, 0.05, 0.02).0, Verdict::Regressed);
        assert_eq!(compare(&base, &faster, 0.05, 0.02).0, Verdict::Improved);
        // A 0.5% shift is below the threshold, whatever the p-value.
        assert_eq!(compare(&base, &tiny, 0.05, 0.02).0, Verdict::NoChange);
        assert_eq!(compare(&base, &base, 0.05, 0.02).0, Verdict::NoChange);
    }
}
