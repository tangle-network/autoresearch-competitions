//! Combinatorial / OR-solver vertical: the autoresearch market drives a
//! **constraint-satisfaction heuristic** competition.
//!
//! Researchers submit a *heuristic weight vector* — the knobs a portfolio solver
//! for a weighted constraint-satisfaction problem (think MAX-SAT branching weights,
//! or the cost terms of a routing/VRP construction heuristic) is sensitive to. A
//! solver runs the heuristic over a bank of problem instances and the market scores
//! the **solution objective** it achieves: the fraction of (weighted) constraints
//! satisfied, averaged over instances. The Referee re-scores the produced weights
//! on a **held-out instance family** with a small distribution shift, gates it,
//! ranks, and pays. **Delegating the search never delegates the trust** — only the
//! held-out objective decides payment.
//!
//! # The universal engine, not a bespoke one
//!
//! This vertical does NOT ship its own engine. It plugs a domain [`Scorer`] for
//! [`GenericArtifact`] into the *same* deterministic [`SupervisorEngine`] every other
//! universal-engine vertical drives: the engine searches the artifact's `params`
//! vector (the heuristic weights) to MAXIMISE the researcher-visible dev objective,
//! and the e2e re-scores the produced weights on held-out. A researcher is just a
//! `(seed, budget, step, start)` configuration of that one engine.
//!
//! # Honest seam — NOT a real solver
//!
//! [`SolverScorer`] is a deterministic *closed-form stand-in* for an OR solver's
//! objective surface, not a solver. It models the shape of a real weighted-CSP
//! tuning landscape — a good weight region with saturating (diminishing) returns,
//! and a held-out distribution shift that punishes weights over-fit to the training
//! instances — so a moderately-tuned submission produces a *real*, gate-clearing
//! held-out lift while an over-searched one fails the gate, with no solver, no clock,
//! and no I/O (so it runs in CI). A real external solver backend can be plugged in
//! behind the same `Engine`/`Scorer` seams; `value = +objective` (higher is better).

use std::future::Future;

use autoresearch_runtime::traits::{Scorer, ScorerError};
use autoresearch_runtime::types::{Measurement, Split};
use autoresearch_supervisor::{ArtifactKind, GenericArtifact};

// --- Solver-objective constants ---------------------------------------------
//
// A closed-form model of how a heuristic weight vector converts into a solution
// objective (fraction of constraints satisfied) over a bank of instances. The point
// is not solver fidelity; it is to give the market a real multi-knob optimization
// surface with the *shape* of a weighted-CSP tuning landscape: one good weight
// region, saturating returns near it, and a held-out shift so weights over-fit to
// the training instances generalize slightly worse.

/// Dimensionality of the heuristic weight vector (the searchable encoding).
pub const WEIGHT_DIM: usize = 6;

/// Objective a trivial (zero-weight) heuristic reaches: it satisfies this fraction
/// of constraints by luck before any tuning. Higher tuning closes the gap to `OBJ_MAX`.
const OBJ_FLOOR: f64 = 0.55;
/// Asymptotic ceiling of the objective — even perfect weights leave some instances'
/// constraints unsatisfiable, so the surface saturates strictly below 1.0.
const OBJ_MAX: f64 = 0.97;

/// How sharply the objective approaches `OBJ_MAX` as weights approach the optimum.
/// Larger = faster saturation (steeper diminishing returns).
const SATURATION: f64 = 0.9;

/// Held-out distribution shift: the held-out instance family's optimal weights are
/// offset from the training family's by this much *per dimension*, in the direction
/// `SHIFT_DIR`. Weights tuned to the training optimum therefore sit slightly off the
/// held-out optimum — a small, honest generalization gap the gate keys off.
const HELDOUT_SHIFT: f64 = 0.18;

/// Std of per-instance measurement noise (gives the objective a real CI over the
/// instance bank). Small relative to the lift a tuned heuristic earns.
const INSTANCE_NOISE: f64 = 0.012;
/// z for a two-sided 95% normal interval.
const Z_95: f64 = 1.96;

/// The training-family optimal weight vector (the good heuristic the search hunts).
/// Deliberately not the origin, so the zero-param baseline has real room to improve.
const W_OPT: [f64; WEIGHT_DIM] = [1.0, -0.5, 0.75, 0.4, -0.9, 0.6];

/// Per-dimension direction of the held-out shift (unit-ish signs). The held-out
/// optimum is `W_OPT + HELDOUT_SHIFT * SHIFT_DIR`.
const SHIFT_DIR: [f64; WEIGHT_DIM] = [1.0, 1.0, -1.0, 1.0, -1.0, 1.0];

// --- Deterministic noise ----------------------------------------------------

/// A splitmix64 finalizer mapped to a uniform `f64` in `[-1, 1)`. Deterministic from
/// its input mix word — no `rand`, no clock — so every measurement is byte-reproducible,
/// which is what lets the e2e assert concrete lift.
fn jitter(mix: u64) -> f64 {
    let mut z = mix.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let bits = z >> 11; // top 53 bits
    let unit = (bits as f64) / ((1u64 << 53) as f64);
    2.0 * unit - 1.0
}

/// A stable 64-bit content seed for a weight vector, so the per-instance noise is a
/// deterministic function of the *weights themselves* (FNV-1a over the bit patterns).
/// Two distinct weight vectors get distinct noise; the same vector always reproduces.
fn weights_seed(weights: &[f64]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for w in weights {
        for byte in w.to_bits().to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    hash
}

// --- The objective surface --------------------------------------------------

/// The objective a heuristic weight vector reaches on a given instance family,
/// *before* measurement noise. Returns the (noiseless) fraction of satisfied
/// constraints in `[OBJ_FLOOR, OBJ_MAX)`.
///
/// The surface is a saturating function of the squared distance from the family's
/// optimal weights: at the optimum it reaches `OBJ_MAX`; far away it decays toward
/// `OBJ_FLOOR`. On [`Split::HeldOut`] the optimum is shifted, so weights tuned to the
/// training optimum land slightly off and score a bit lower than they did on dev.
fn objective(weights: &[f64], split: Split) -> f64 {
    let mut sq_dist = 0.0;
    for i in 0..WEIGHT_DIM {
        let w = weights.get(i).copied().unwrap_or(0.0);
        let target = match split {
            Split::Dev => W_OPT[i],
            Split::HeldOut => W_OPT[i] + HELDOUT_SHIFT * SHIFT_DIR[i],
        };
        let d = w - target;
        sq_dist += d * d;
    }
    // Saturating return: exp(-SATURATION * dist^2) in [0, 1], 1 at the optimum.
    let closeness = (-SATURATION * sq_dist).exp();
    OBJ_FLOOR + (OBJ_MAX - OBJ_FLOOR) * closeness
}

// --- Scorer (the Referee's held-out evaluation) -----------------------------

/// Re-scores a heuristic-weight [`GenericArtifact`] by evaluating it over `instances`
/// problem instances and reporting the mean solution objective with a normal 95% CI.
/// `value` is `+objective` (fraction of constraints satisfied) so that higher is
/// better and the orchestrator computes a positive lift for a genuine improvement.
///
/// On [`Split::HeldOut`] the held-out instance family's shifted optimum is used; on
/// [`Split::Dev`] the training optimum is used — so weights over-fit to the dev
/// signal (an over-searched submission) look good on the dev objective a researcher
/// sees and still fall short of the Referee's held-out gate.
#[derive(Clone, Copy, Debug)]
pub struct SolverScorer {
    instances: u32,
}

impl SolverScorer {
    /// `instances` should be at least the gate's `min_n` (12) for the result to be
    /// admissible (each instance is one CI sample).
    #[must_use]
    pub fn new(instances: u32) -> Self {
        Self { instances }
    }

    /// Synchronous scoring core, exposed so sync callers (and the unit tests) can
    /// re-score a weight vector without driving the always-ready `Scorer::score`
    /// future.
    #[must_use]
    pub fn measure(&self, artifact: &GenericArtifact, split: Split) -> Measurement {
        let base_obj = objective(&artifact.params, split);
        let split_word: u64 = match split {
            Split::Dev => 0x0000_0000_0000_D0D0,
            Split::HeldOut => 0x0000_0000_0000_4E1D,
        };
        let seed = weights_seed(&artifact.params);

        // Evaluate over instances; each instance sees the same heuristic with a small,
        // deterministic per-instance perturbation, giving a real sample distribution.
        let n = self.instances.max(1);
        let mut samples = Vec::with_capacity(n as usize);
        for instance in 0..n {
            let mix = seed.wrapping_mul(0x100_0000_01B3)
                ^ split_word
                ^ (u64::from(instance).wrapping_mul(0x9E37_79B1));
            // Objective is a fraction; keep samples in [0, 1] after noise.
            let noisy = (base_obj + INSTANCE_NOISE * jitter(mix)).clamp(0.0, 1.0);
            samples.push(noisy);
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

impl Scorer for SolverScorer {
    type Artifact = GenericArtifact;

    fn id(&self) -> &str {
        "combinatorial-solver-heldout"
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

// --- Helpers ----------------------------------------------------------------

/// The reference submission a candidate must beat: a trivial all-zero heuristic. It
/// satisfies only `OBJ_FLOOR` of constraints, so any genuine tuning earns real lift.
#[must_use]
pub fn baseline_artifact() -> GenericArtifact {
    GenericArtifact::baseline(
        ArtifactKind::Solver,
        WEIGHT_DIM,
        "trivial zero-weight heuristic",
    )
}

/// A heuristic submission with the given weight vector.
#[must_use]
pub fn solver_artifact(weights: Vec<f64>, label: impl Into<String>) -> GenericArtifact {
    GenericArtifact::new(ArtifactKind::Solver, weights, label)
}

/// The training-family optimal weights, exposed for tests and as the analytic upper
/// bound a search converges toward on the dev signal.
#[must_use]
pub fn dev_optimum() -> Vec<f64> {
    W_OPT.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art(weights: &[f64]) -> GenericArtifact {
        solver_artifact(weights.to_vec(), "test")
    }

    #[test]
    fn scoring_is_deterministic_per_weight_vector() {
        let scorer = SolverScorer::new(16);
        let w = art(&[0.9, -0.4, 0.7, 0.5, -0.8, 0.55]);
        assert_eq!(
            scorer.measure(&w, Split::HeldOut).value,
            scorer.measure(&w, Split::HeldOut).value,
            "same weights must reproduce the same objective"
        );
    }

    #[test]
    fn distinct_weight_vectors_give_spread() {
        let scorer = SolverScorer::new(24);
        let a = scorer
            .measure(&art(&[0.9, -0.4, 0.7, 0.5, -0.8, 0.55]), Split::Dev)
            .value;
        let b = scorer
            .measure(&art(&[0.2, 0.1, 0.0, -0.3, 0.4, -0.1]), Split::Dev)
            .value;
        assert_ne!(a, b, "different heuristics must score differently");
    }

    #[test]
    fn tuned_weights_beat_the_trivial_baseline_on_heldout() {
        let scorer = SolverScorer::new(24);
        let base = scorer.measure(&baseline_artifact(), Split::HeldOut).value;
        // Weights near the *held-out* optimum: a genuinely strong, generalizing solver.
        let good: Vec<f64> = (0..WEIGHT_DIM)
            .map(|i| W_OPT[i] + 0.5 * HELDOUT_SHIFT * SHIFT_DIR[i])
            .collect();
        let tuned = scorer.measure(&art(&good), Split::HeldOut).value;
        assert!(
            tuned > base + 0.10,
            "a tuned heuristic must clearly beat the trivial baseline on held-out: {base} -> {tuned}"
        );
    }

    #[test]
    fn objective_is_a_satisfiable_fraction() {
        // The objective is always a valid constraint-satisfaction fraction.
        for w in [
            vec![0.0; WEIGHT_DIM],
            dev_optimum(),
            vec![5.0; WEIGHT_DIM],
            vec![-3.0, 4.0, 2.0, -1.0, 0.0, 9.0],
        ] {
            for split in [Split::Dev, Split::HeldOut] {
                let o = objective(&w, split);
                assert!(
                    (OBJ_FLOOR..=OBJ_MAX).contains(&o),
                    "objective {o} out of [{OBJ_FLOOR}, {OBJ_MAX}] for split {split:?}"
                );
            }
        }
    }

    #[test]
    fn dev_optimum_overfits_heldout() {
        // The crux the gate exploits: weights tuned exactly to the dev optimum score
        // their best on dev but strictly worse on held-out (the shifted family).
        let scorer = SolverScorer::new(64);
        let at_dev_opt = art(&dev_optimum());
        let dev = scorer.measure(&at_dev_opt, Split::Dev).value;
        let heldout = scorer.measure(&at_dev_opt, Split::HeldOut).value;
        assert!(
            dev > heldout,
            "dev-optimal weights must generalize worse on held-out: dev={dev} heldout={heldout}"
        );
    }

    #[test]
    fn measurement_has_admissible_n_and_real_ci() {
        let scorer = SolverScorer::new(20);
        let m = scorer.measure(&art(&dev_optimum()), Split::HeldOut);
        assert_eq!(m.n, 20, "n is the instance count");
        assert!(m.n >= 12, "n must clear the default gate min_n");
        assert!(
            m.ci_upper > m.ci_lower,
            "instance noise must produce a non-degenerate CI: [{}, {}]",
            m.ci_lower,
            m.ci_upper
        );
        assert!(m.value > 0.0 && m.value < 1.0, "objective is a fraction");
    }
}
