//! Forecasting vertical: the autoresearch market drives a **statistical
//! time-series forecasting** competition on the generic [`GenericEngine`].
//!
//! Researchers submit a *forecaster* — a linear autoregressive model whose
//! coefficients predict the next value of a deterministic synthetic series from
//! its recent lags. The generic engine searches the coefficient vector
//! ([`GenericArtifact::params`]) to drive the dev (in-sample) forecast error down;
//! the market's Referee re-scores the produced model on a **held-out** window of
//! the same series, gates it, ranks, and pays. **Searching the dev signal never
//! buys the payment** — only the held-out re-score decides.
//!
//! # The metric the scorer models
//!
//! The ground-truth series is `y[t] = sum_k beta_true[k] * y[t-1-k] + noise[t]`,
//! a stationary AR(p) process generated **once** by a seeded LCG. A submitted
//! coefficient vector `beta` forecasts each in-window point from its true lags
//! and is scored by mean-squared error. `value = -rmse` so that **higher is
//! better** and the orchestrator computes a positive lift when a model forecasts
//! the series more accurately than the all-zeros baseline.
//!
//! - On [`Split::Dev`] the model is scored on the **in-sample** window (indices
//!   `[P, P+DEV_LEN)`) — the signal a researcher is allowed to hill-climb.
//! - On [`Split::HeldOut`] it is scored on a **later, disjoint** window (indices
//!   `[P+DEV_LEN+GAP, …)`) drawn from the *same* process but with fresh
//!   realizations of the noise — the out-of-sample signal that decides payment.
//!
//! Because the two windows share the AR structure but not the noise draws, a
//! model that *over-searches the dev window* — pushing coefficients large to
//! shave in-sample residual noise — inflates its held-out error. That is the
//! classic forecasting over-fit, and the held-out gate is exactly what catches
//! it: an over-budget search wins on dev and is refused on held-out, while a
//! moderate search that recovers the true coefficients generalizes and is paid.
//!
//! # Honest seam — a deterministic stand-in, not a live forecaster
//!
//! This module is a *closed-form model* of forecasting dynamics over a synthetic
//! series — no `rand`, no clock, no I/O — so every CI proof is byte-reproducible.
//! It is the marked stand-in for the real artifact: a live forecasting model built
//! and back-tested against real data. What it proves is the **market mechanism
//! around forecasting**: the generic engine searching a coefficient encoding,
//! held-out re-scoring of the produced model, and the promotion gate refusing an
//! over-fit that only looked good in-sample.

use std::future::Future;

use autoresearch_runtime::traits::{Scorer, ScorerError};
use autoresearch_runtime::types::{Measurement, Split};
use autoresearch_generic_engine::{ArtifactKind, GenericArtifact};

// --- Series + model geometry ------------------------------------------------

/// AR order: the number of lag coefficients a forecaster carries. This is the
/// dimensionality of the searchable [`GenericArtifact::params`] vector.
pub const ORDER: usize = 3;

/// The true AR(p) coefficients of the data-generating process. A perfect
/// forecaster recovers exactly these; the baseline (all-zeros) recovers none.
/// Persistent (sum ~0.95) so the series carries real autoregressive structure —
/// predicting from lags beats the zero baseline by a wide, gate-clearing margin.
const BETA_TRUE: [f64; ORDER] = [0.65, 0.20, 0.10];

/// Length of the in-sample (dev) forecast window — the signal researchers see.
const DEV_LEN: usize = 64;
/// Length of the held-out forecast window — the Referee-only payment signal.
const HELDOUT_LEN: usize = 64;
/// Gap between the dev window and the held-out window, so the two are disjoint
/// realizations of the same process (no leakage of in-sample noise draws).
const GAP: usize = 16;

/// Innovation (process-noise) magnitude of the synthetic series. Large enough
/// that an over-search can find spurious in-sample structure to fit, which is
/// precisely what generalizes worse on the held-out window.
const NOISE_STD: f64 = 0.40;

/// Per-coefficient L2 complexity penalty applied **only out-of-sample**. A model
/// that inflates its coefficients beyond the true process to chase in-sample
/// noise pays this on held-out — the generalization gap the gate exploits.
const COMPLEXITY_PEN: f64 = 0.015;

/// Std of per-eval-shard measurement noise, giving the held-out score a real CI.
const EVAL_NOISE: f64 = 0.01;
/// z for a two-sided 95% normal interval.
const Z_95: f64 = 1.96;

/// Total series length: a leading lag burn-in of [`ORDER`] points, then the dev
/// window, the gap, and the held-out window.
const SERIES_LEN: usize = ORDER + DEV_LEN + GAP + HELDOUT_LEN;

// --- Deterministic noise ----------------------------------------------------

/// A 64-bit LCG (Knuth MMIX constants) mapped to a uniform `f64` in `[-1, 1)`.
/// Deterministic from its seed — no `rand`, no clock — so the synthetic series
/// and every measurement are byte-reproducible, which is what lets the e2e test
/// assert a concrete, certified lift.
#[derive(Clone, Debug)]
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }

    /// A uniform `f64` in `[-1, 1)` from the well-distributed high bits.
    fn next_signed(&mut self) -> f64 {
        let bits = self.next_u64() >> 11;
        2.0 * ((bits as f64) / ((1u64 << 53) as f64)) - 1.0
    }
}

/// A splitmix64 finalizer mapped to a uniform `f64` in `[-1, 1)`, for the
/// per-shard eval jitter (independent of the series LCG).
fn jitter(mix: u64) -> f64 {
    let mut z = mix.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let bits = z >> 11;
    2.0 * ((bits as f64) / ((1u64 << 53) as f64)) - 1.0
}

// --- The synthetic series ---------------------------------------------------

/// A deterministic AR(p) realization: the ground-truth series a forecaster is
/// scored against. Generated **once** from a fixed seed so dev and held-out are
/// disjoint windows of the *same* process. Carries the series values plus the
/// per-step innovations, so a forecaster's residual against the true lags is
/// exactly the irreducible process noise plus its own coefficient error.
#[derive(Clone, Debug)]
struct Series {
    values: Vec<f64>,
}

impl Series {
    /// Generate the canonical series from the data-generating AR coefficients.
    fn generate() -> Self {
        let mut rng = Lcg::new(0xF0_4E_CA_57_5E_E1_ED_01);
        let mut values = vec![0.0_f64; SERIES_LEN];
        // Seed the first ORDER points with pure innovations (burn-in).
        for v in values.iter_mut().take(ORDER) {
            *v = NOISE_STD * rng.next_signed();
        }
        for t in ORDER..SERIES_LEN {
            let mut y = 0.0;
            for (k, &b) in BETA_TRUE.iter().enumerate() {
                y += b * values[t - 1 - k];
            }
            y += NOISE_STD * rng.next_signed();
            values[t] = y;
        }
        Self { values }
    }

    /// The `[start, start+len)` index range of a forecast window.
    fn window(split: Split) -> (usize, usize) {
        match split {
            Split::Dev => (ORDER, ORDER + DEV_LEN),
            Split::HeldOut => {
                let start = ORDER + DEV_LEN + GAP;
                (start, start + HELDOUT_LEN)
            }
        }
    }

    /// Mean-squared one-step forecast error of coefficient vector `beta` over the
    /// window for `split`. Each point `t` is forecast from its true lags
    /// `y[t-1..t-ORDER]`; the residual is the gap between forecast and actual.
    fn forecast_mse(&self, beta: &[f64], split: Split) -> f64 {
        let (start, end) = Self::window(split);
        let mut sse = 0.0;
        let mut n = 0.0;
        for t in start..end {
            let mut pred = 0.0;
            for (k, &b) in beta.iter().enumerate() {
                pred += b * self.values[t - 1 - k];
            }
            let err = self.values[t] - pred;
            sse += err * err;
            n += 1.0;
        }
        sse / n
    }
}

/// Decode a forecaster's [`GenericArtifact::params`] into AR coefficients,
/// truncating or zero-padding to [`ORDER`] so the surface's variable-length
/// vector always yields a well-formed model.
fn coefficients(params: &[f64]) -> [f64; ORDER] {
    let mut beta = [0.0_f64; ORDER];
    for (k, slot) in beta.iter_mut().enumerate() {
        if let Some(&p) = params.get(k) {
            *slot = p;
        }
    }
    beta
}

// --- Scorer (the Referee's held-out evaluation) -----------------------------

/// Scores a forecaster (a [`GenericArtifact`] whose params are AR coefficients)
/// on a data split by evaluating its forecast error over `eval_shards` shards of
/// the synthetic series window and reporting the mean `-rmse` with a normal 95%
/// CI. `value` is `-rmse` so higher is better and a genuine accuracy gain shows
/// as a positive lift over the all-zeros baseline.
///
/// On [`Split::Dev`] the model is scored on the in-sample window with **no**
/// complexity penalty — the signal a researcher hill-climbs. On
/// [`Split::HeldOut`] it is scored on the later, disjoint window **plus** an L2
/// complexity penalty, so a model that inflated its coefficients to shave
/// in-sample noise looks good on dev and still fails the Referee's held-out gate.
#[derive(Clone, Debug)]
pub struct ForecastScorer {
    series: Series,
    eval_shards: u32,
}

impl ForecastScorer {
    /// Build the scorer over the canonical synthetic series. `eval_shards` should
    /// be at least the gate's `min_n` (12) for the result to be admissible.
    #[must_use]
    pub fn new(eval_shards: u32) -> Self {
        Self {
            series: Series::generate(),
            eval_shards,
        }
    }

    /// The L2 complexity of a coefficient vector (sum of squares). Charged only
    /// out-of-sample: this is the generalization gap an over-fit pays.
    fn complexity(beta: &[f64; ORDER]) -> f64 {
        beta.iter().map(|b| b * b).sum()
    }

    /// Synchronous scoring core, exposed so sync callers (unit tests, a continuous
    /// leaderboard, an m-of-n re-score panel) can re-score a model without driving
    /// the always-ready [`Scorer::score`] future. `value = -rmse` (higher better).
    #[must_use]
    pub fn measure(&self, artifact: &GenericArtifact, split: Split) -> Measurement {
        let beta = coefficients(&artifact.params);
        let mse = self.series.forecast_mse(&beta, split);
        // Held-out adds the L2 complexity penalty; dev does not. This is the gap
        // that lets an over-searched model win on dev and lose on held-out.
        let penalty = match split {
            Split::Dev => 0.0,
            Split::HeldOut => COMPLEXITY_PEN * Self::complexity(&beta),
        };
        let rmse = mse.sqrt() + penalty;

        let split_word: u64 = match split {
            Split::Dev => 0x0000_0000_0000_D0D0,
            Split::HeldOut => 0x0000_0000_0000_4E1D,
        };
        // A stable per-model mix word from the coefficient bits, so the eval-shard
        // jitter is deterministic per (model, split) but differs across models.
        let mut model_mix: u64 = 0xcbf2_9ce4_8422_2325;
        for b in &beta {
            for byte in b.to_bits().to_le_bytes() {
                model_mix ^= u64::from(byte);
                model_mix = model_mix.wrapping_mul(0x0000_0100_0000_01B3);
            }
        }

        // Evaluate over shards; each shard sees the same model with a small,
        // deterministic eval-noise perturbation, giving a real sample distribution.
        let n = self.eval_shards.max(1);
        let mut samples = Vec::with_capacity(n as usize);
        for shard in 0..n {
            let mix = model_mix ^ split_word ^ u64::from(shard).wrapping_mul(0x9E37_79B1);
            let noisy = rmse + EVAL_NOISE * jitter(mix);
            samples.push(-noisy); // value = -rmse (higher is better)
        }

        let nf = f64::from(n);
        let mean = samples.iter().sum::<f64>() / nf;
        let var = if n > 1 {
            samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (nf - 1.0)
        } else {
            0.0
        };
        let se = (var / nf).sqrt();
        let half = Z_95 * se;
        Measurement {
            value: mean,
            ci_lower: mean - half,
            ci_upper: mean + half,
            n,
            cost: nf,
        }
    }
}

impl Scorer for ForecastScorer {
    type Artifact = GenericArtifact;

    fn id(&self) -> &str {
        "forecasting-heldout"
    }

    fn score(
        &self,
        artifact: &Self::Artifact,
        split: Split,
    ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send {
        let m = self.measure(artifact, split);
        std::future::ready(Ok(m))
    }
}

/// The baseline forecaster: an all-zeros coefficient model (predicts the series
/// mean of ~0, no autoregression). Any real forecaster must beat *this* on
/// held-out to certify lift. `content` carries a domain-readable description.
#[must_use]
pub fn baseline() -> GenericArtifact {
    GenericArtifact::baseline(ArtifactKind::Forecast, ORDER, "zero-coefficient forecaster")
}

/// A starting forecaster the generic engine searches from: all-zeros params of
/// the right dimension, tagged [`ArtifactKind::Forecast`].
#[must_use]
pub fn start() -> GenericArtifact {
    GenericArtifact::new(
        ArtifactKind::Forecast,
        vec![0.0; ORDER],
        "candidate forecaster",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(beta: &[f64]) -> GenericArtifact {
        GenericArtifact::new(ArtifactKind::Forecast, beta.to_vec(), "m")
    }

    #[test]
    fn series_is_deterministic() {
        let a = Series::generate();
        let b = Series::generate();
        assert_eq!(
            a.values, b.values,
            "the synthetic series must be reproducible"
        );
    }

    #[test]
    fn dev_and_heldout_windows_are_disjoint() {
        let (ds, de) = Series::window(Split::Dev);
        let (hs, he) = Series::window(Split::HeldOut);
        assert!(
            de + GAP <= hs,
            "held-out must start after the dev window + gap"
        );
        assert!(he <= SERIES_LEN, "held-out window must fit the series");
        assert!(de > ds && he > hs);
    }

    #[test]
    fn true_coefficients_beat_baseline_on_heldout() {
        let scorer = ForecastScorer::new(16);
        let base_v = scorer.measure(&baseline(), Split::HeldOut).value;
        let truth_v = scorer.measure(&model(&BETA_TRUE), Split::HeldOut).value;
        assert!(
            truth_v > base_v + 0.05,
            "recovering the true AR coefficients must clearly beat the zero baseline \
             on held-out: {base_v} -> {truth_v}"
        );
    }

    #[test]
    fn true_coefficients_minimize_heldout_error() {
        // The true process coefficients should forecast the held-out window at
        // least as well as a perturbed (over-large) variant once the out-of-sample
        // complexity penalty is paid — the property the gate relies on.
        let scorer = ForecastScorer::new(32);
        let truth = scorer.measure(&model(&BETA_TRUE), Split::HeldOut).value;
        let inflated = scorer
            .measure(&model(&[1.6, -1.1, 0.9]), Split::HeldOut)
            .value;
        assert!(
            truth > inflated,
            "inflated coefficients must generalize worse on held-out: \
             truth={truth} inflated={inflated}"
        );
    }

    #[test]
    fn overfit_looks_better_on_dev_than_heldout() {
        // An inflated model that chases in-sample noise scores better on dev than
        // it deserves: dev (no penalty, in-sample) beats its held-out score. This
        // is the generalization gap the held-out gate exploits.
        let scorer = ForecastScorer::new(32);
        let inflated = model(&[0.9, -0.7, 0.6]);
        let dev = scorer.measure(&inflated, Split::Dev).value;
        let heldout = scorer.measure(&inflated, Split::HeldOut).value;
        assert!(
            dev > heldout,
            "an inflated model must look better on dev than held-out: \
             dev={dev} heldout={heldout}"
        );
    }

    #[test]
    fn scoring_is_deterministic_per_model_and_split() {
        let scorer = ForecastScorer::new(16);
        let m = model(&[0.5, -0.3, 0.1]);
        assert_eq!(
            scorer.measure(&m, Split::HeldOut).value,
            scorer.measure(&m, Split::HeldOut).value
        );
        // Dev and held-out must differ so the gate is meaningful.
        assert_ne!(
            scorer.measure(&m, Split::Dev).value,
            scorer.measure(&m, Split::HeldOut).value
        );
    }

    #[test]
    fn coefficients_truncate_and_pad() {
        assert_eq!(coefficients(&[1.0, 2.0]), [1.0, 2.0, 0.0]);
        assert_eq!(coefficients(&[1.0, 2.0, 3.0, 4.0]), [1.0, 2.0, 3.0]);
    }

    #[tokio::test]
    async fn scorer_future_matches_sync_measure() {
        let scorer = ForecastScorer::new(16);
        let m = model(&BETA_TRUE);
        let via_future = scorer.score(&m, Split::HeldOut).await.unwrap();
        let via_sync = scorer.measure(&m, Split::HeldOut);
        assert_eq!(via_future, via_sync);
    }
}
