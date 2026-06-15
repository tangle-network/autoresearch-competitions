//! Lift estimation from two independent [`Measurement`]s.
//!
//! # What this computes
//!
//! Given a candidate measurement and a baseline measurement, this estimates the
//! improvement (`delta = candidate.value - baseline.value`) together with a 95%
//! confidence interval on that delta. The CI is what the promotion [`Gate`] keys
//! off — payment is gated on the *lower bound* of the lift, never the point
//! estimate, so noise alone cannot buy a payout.
//!
//! # M1 stand-in vs. production
//!
//! This is the **unpaired** estimator: it treats the candidate and baseline as two
//! independent samples and propagates their standard errors in quadrature
//! (`se_delta = sqrt(se_c^2 + se_b^2)`). That is correct but conservative — it
//! ignores the per-task correlation between baseline and candidate runs.
//!
//! The production refinement is **paired replay** (Improvement-Plane "Tier B"):
//! score baseline and candidate on the *same* held-out tasks and form per-task
//! differences, which cancels task-difficulty variance and tightens the CI
//! substantially. The orchestrator's seams are identical; only this function is
//! swapped for the paired estimator when paired evidence is available.

use autoresearch_runtime::types::{Lift, Measurement};

/// 1.96 standard normal z for a two-sided 95% interval.
const Z_95: f64 = 1.96;

/// Recover a standard error from a measurement's reported CI.
///
/// The normal-approximation CI has half-width `Z_95 * se`, so `se = width / (2 * Z_95)`.
/// If the reported width is non-positive (degenerate or missing CI), fall back to the
/// binomial/proportion SE `sqrt(p(1-p)/n)`, clamping `p` into `[0, 1]`. The fallback
/// assumes `value` is a proportion (the M1 scorer reports accuracy), which is the
/// only case a zero-width CI can sensibly arise from here.
fn standard_error(m: &Measurement) -> f64 {
    let width = m.ci_upper - m.ci_lower;
    if width > 0.0 {
        return width / (2.0 * Z_95);
    }
    if m.n == 0 {
        return f64::INFINITY;
    }
    let p = m.value.clamp(0.0, 1.0);
    (p * (1.0 - p) / f64::from(m.n)).sqrt()
}

/// Estimate the lift of `candidate` over `baseline`.
///
/// - `delta = candidate.value - baseline.value`
/// - `se_delta = sqrt(se_candidate^2 + se_baseline^2)` (independent-sample propagation)
/// - `ci = delta +/- Z_95 * se_delta`
/// - `n = min(candidate.n, baseline.n)` — the binding statistical power of the pair
///
/// See the module docs for why this is the conservative unpaired stand-in.
#[must_use]
pub fn estimate_lift(candidate: &Measurement, baseline: &Measurement) -> Lift {
    let delta = candidate.value - baseline.value;
    let se_c = standard_error(candidate);
    let se_b = standard_error(baseline);
    let se_delta = (se_c * se_c + se_b * se_b).sqrt();
    let half = Z_95 * se_delta;
    Lift {
        delta,
        ci_lower: delta - half,
        ci_upper: delta + half,
        n: candidate.n.min(baseline.n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn delta_is_value_difference() {
        let cand = Measurement {
            value: 0.85,
            ci_lower: 0.80,
            ci_upper: 0.90,
            n: 80,
            cost: 80.0,
        };
        let base = Measurement {
            value: 0.50,
            ci_lower: 0.45,
            ci_upper: 0.55,
            n: 80,
            cost: 80.0,
        };
        let lift = estimate_lift(&cand, &base);
        approx(lift.delta, 0.35);
        assert_eq!(lift.n, 80);
    }

    #[test]
    fn se_recovered_from_ci_width() {
        // width = 0.1 => se = 0.1 / (2*1.96).
        let m = Measurement {
            value: 0.5,
            ci_lower: 0.45,
            ci_upper: 0.55,
            n: 100,
            cost: 0.0,
        };
        approx(standard_error(&m), 0.1 / (2.0 * Z_95));
    }

    #[test]
    fn se_combines_in_quadrature() {
        // Two equal SEs of 0.05 combine to sqrt(2)*0.05.
        let half = Z_95 * 0.05;
        let cand = Measurement {
            value: 0.8,
            ci_lower: 0.8 - half,
            ci_upper: 0.8 + half,
            n: 50,
            cost: 0.0,
        };
        let base = Measurement {
            value: 0.5,
            ci_lower: 0.5 - half,
            ci_upper: 0.5 + half,
            n: 50,
            cost: 0.0,
        };
        let lift = estimate_lift(&cand, &base);
        let expected_se_delta = (0.05_f64 * 0.05 + 0.05 * 0.05).sqrt();
        approx(lift.delta, 0.3);
        approx(lift.ci_lower, 0.3 - Z_95 * expected_se_delta);
        approx(lift.ci_upper, 0.3 + Z_95 * expected_se_delta);
    }

    #[test]
    fn zero_width_ci_falls_back_to_proportion_se() {
        // No CI given; p=0.5, n=100 => se = sqrt(0.25/100) = 0.05.
        let m = Measurement {
            value: 0.5,
            ci_lower: 0.5,
            ci_upper: 0.5,
            n: 100,
            cost: 0.0,
        };
        approx(standard_error(&m), 0.05);
    }

    #[test]
    fn n_is_the_minimum_of_the_pair() {
        let cand = Measurement {
            value: 0.8,
            ci_lower: 0.7,
            ci_upper: 0.9,
            n: 40,
            cost: 0.0,
        };
        let base = Measurement {
            value: 0.5,
            ci_lower: 0.4,
            ci_upper: 0.6,
            n: 120,
            cost: 0.0,
        };
        assert_eq!(estimate_lift(&cand, &base).n, 40);
    }

    #[test]
    fn ci_lower_below_delta_above_zero_for_clear_win() {
        // A large, well-powered win should have a strictly positive lower bound.
        let cand = Measurement {
            value: 0.88,
            ci_lower: 0.81,
            ci_upper: 0.95,
            n: 80,
            cost: 80.0,
        };
        let base = Measurement {
            value: 0.50,
            ci_lower: 0.39,
            ci_upper: 0.61,
            n: 80,
            cost: 80.0,
        };
        let lift = estimate_lift(&cand, &base);
        assert!(
            lift.ci_lower > 0.0,
            "lower bound should clear zero: {lift:?}"
        );
        assert!(lift.ci_lower < lift.delta);
        assert!(lift.delta < lift.ci_upper);
    }
}
