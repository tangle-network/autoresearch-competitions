//! Program / algorithm **superoptimization** vertical: the autoresearch market
//! drives a competition to make a hot kernel run faster *without breaking it*.
//!
//! Researchers submit an *optimization recipe* — the knobs a superoptimizer
//! actually searches over for a tight numeric loop: the loop **unroll factor**, the
//! **vectorization width**, and a **cache-blocking** weight. A candidate is run on a
//! benchmark instance; the market's Referee re-scores the produced artifact on a
//! held-out instance, gates it on a real certified speedup, ranks, and pays.
//! **Delegating the search never delegates the trust** — a candidate's self-reported
//! speedup is ignored; only the held-out re-score decides payment, and a recipe that
//! is *fast but incorrect* is refused outright.
//!
//! # Where this plugs into the universal engine
//!
//! Unlike the distributed-training vertical (which carries its own
//! `DistributedTrainingEngine`), this vertical drives the **universal**
//! [`SupervisorEngine`]: the same seeded local search every domain shares. The recipe
//! is encoded in [`GenericArtifact::params`] (`[unroll, vectorize, cache_block]`); the
//! engine perturbs that vector to maximize the *dev* speedup this [`ProgramScorer`]
//! reports, and the Referee re-scores the produced params on held-out. Researchers
//! differ only by **seed / budget / step / start point** — never by a different engine.
//! That is the generalization the supervisor exists to prove: a new domain is a new
//! scorer, not a new engine.
//!
//! # The optimization surface (honest stand-in)
//!
//! [`ProgramScorer`] is a *deterministic closed-form model* of a kernel's runtime, not
//! a compiler or a CPU. It models the three things that make superoptimization a real
//! search: (1) a **hidden optimum** recipe at which the modeled runtime is minimized
//! (a quadratic bowl around it — too little unroll wastes issue slots, too much
//! spills registers; the same for vector width and cache-block size); (2) a
//! **correctness region** — a recipe that unrolls/vectorizes past a modeled
//! data-dependence bound produces a *wrong* result, which collapses the value to a
//! heavy penalty (a fast-but-incorrect program is worthless); and (3) a small
//! **dev→held-out optimum shift**, so a recipe that over-fits the one dev benchmark
//! instance generalizes slightly worse on the held-out instance — exactly what the
//! held-out gate is there to catch. No `rand`, no clock, no I/O: every measurement is
//! byte-reproducible, so the e2e proof runs in CI.
//!
//! `value` is a **normalized speedup over baseline in `[0, 1]`** (higher is better):
//! `0` is the un-optimized baseline runtime (or an incorrect program), `~1` is the
//! modeled floor runtime. Encoding speedup in `[0, 1]` keeps it in the proportion
//! range the lift estimator's CI math expects.
//!
//! # Honest seam — NOT a real superoptimizer
//!
//! The real artifact is the source/IR a real compiler/superoptimizer backend writes
//! and actually benchmarks. This module models the *market mechanism around
//! superoptimization*: searching an optimization encoding, held-out re-scoring of a
//! delegated candidate, the correctness constraint refusing fast-but-wrong programs,
//! and the gate refusing recipes whose speedup does not generalize.

use std::future::Future;

use autoresearch_runtime::traits::{Scorer, ScorerError};
use autoresearch_runtime::types::{Measurement, Split};
use autoresearch_supervisor::{ArtifactKind, GenericArtifact};

// --- Kernel-runtime model constants ----------------------------------------
//
// A closed-form model of how an optimization recipe converts the un-optimized
// baseline runtime into a faster one. The point is not cycle-accuracy; it is to give
// the market a real multi-knob optimization surface with the *shape* of the known
// superoptimization tradeoffs (an interior optimum per knob, a correctness cliff),
// so a well-searched recipe wins on held-out and the failure modes get gated.

/// Number of recipe knobs the engine searches: `[unroll, vectorize, cache_block]`.
pub const RECIPE_DIM: usize = 3;

/// Dev-instance hidden optimum (`[unroll, vectorize, cache_block]`). At this recipe
/// the modeled dev runtime is at its floor.
const OPT_DEV: [f64; RECIPE_DIM] = [4.0, 8.0, 2.0];
/// Held-out-instance optimum: the same kernel on a *slightly different* instance has a
/// mildly shifted optimum. A recipe that over-fits `OPT_DEV` lands a little off here,
/// so its held-out speedup is a touch lower than its dev speedup.
const OPT_HELDOUT: [f64; RECIPE_DIM] = [4.6, 8.8, 2.3];

/// Per-knob curvature of the runtime bowl (how fast runtime grows away from optimum).
/// Larger = a sharper penalty for mis-setting that knob.
const CURVATURE: [f64; RECIPE_DIM] = [0.020, 0.012, 0.030];

/// Best achievable runtime fraction at the optimum: the floor runtime is this times
/// the baseline runtime, so the maximum modeled speedup is `1 - FLOOR_FRACTION`.
const FLOOR_FRACTION: f64 = 0.30;

/// Correctness bound on the *combined* unroll×vectorize aggressiveness. A recipe whose
/// unroll factor times vector width exceeds this crosses a modeled data-dependence
/// boundary and computes a **wrong** result — the program is incorrect at any speed.
const CORRECTNESS_BOUND: f64 = 120.0;

/// The value an incorrect program scores: effectively zero speedup. A fast-but-wrong
/// program must never out-rank a correct one, so this floors below any correct recipe.
const INCORRECT_VALUE: f64 = 0.0;

/// Std of per-benchmark-instance measurement noise (gives the held-out score a real
/// CI). Small relative to the speedup signal so a genuine win still clears the gate.
const EVAL_NOISE: f64 = 0.006;
/// z for a two-sided 95% normal interval.
const Z_95: f64 = 1.96;

// --- Deterministic noise ----------------------------------------------------

/// A splitmix64 finalizer mapped to a uniform `f64` in `[-1, 1)`. Deterministic from
/// its input mix word — no `rand`, no clock — so every measurement is byte-reproducible,
/// which is what lets the e2e test assert a concrete, certified speedup. Mirrors the
/// `jitter` used by the distributed-training vertical.
fn jitter(mix: u64) -> f64 {
    let mut z = mix.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let bits = z >> 11; // top 53 bits
    let unit = (bits as f64) / ((1u64 << 53) as f64);
    2.0 * unit - 1.0
}

/// Hash a recipe's params into a stable mix word, so the per-instance eval noise is a
/// deterministic function of the candidate (not of call order).
fn recipe_mix(params: &[f64]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for p in params {
        for byte in p.to_bits().to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    hash
}

// --- The recipe (decoded from GenericArtifact::params) ----------------------

/// A decoded optimization recipe: the three knobs a superoptimizer searches for a hot
/// numeric kernel. Decoded from [`GenericArtifact::params`]; the engine searches that
/// raw vector, this is the domain reading of it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OptRecipe {
    /// Loop unroll factor (how many iterations are unrolled into one body).
    pub unroll: f64,
    /// Vectorization width (SIMD lanes the inner loop is widened to).
    pub vectorize: f64,
    /// Cache-blocking weight (how aggressively the working set is tiled to fit cache).
    pub cache_block: f64,
}

impl OptRecipe {
    /// Decode a params vector into a recipe. The baseline (all-zero params) decodes to
    /// the *un-optimized* recipe; the engine searches params away from zero toward the
    /// hidden optimum. Only the first [`RECIPE_DIM`] entries are read.
    #[must_use]
    pub fn from_params(params: &[f64]) -> Self {
        let get = |i: usize| params.get(i).copied().unwrap_or(0.0);
        Self {
            unroll: get(0),
            vectorize: get(1),
            cache_block: get(2),
        }
    }

    /// The hidden-optimum recipe for a split, as a [`GenericArtifact`] — the recipe a
    /// perfect search would converge to. Useful for tests and for seeding a strong
    /// researcher near the optimum.
    #[must_use]
    pub fn optimum_artifact(split: Split) -> GenericArtifact {
        let opt = match split {
            Split::Dev => OPT_DEV,
            Split::HeldOut => OPT_HELDOUT,
        };
        GenericArtifact::new(ArtifactKind::Program, opt.to_vec(), "kernel @ optimum")
    }

    /// Whether this recipe produces a **correct** program. Unrolling and vectorizing
    /// past the modeled data-dependence bound reorders dependent operations and yields
    /// a wrong result; a negative knob is also nonsensical (un-decodable) and counts as
    /// incorrect. A correct program is the precondition for *any* speedup.
    #[must_use]
    pub fn is_correct(&self) -> bool {
        if self.unroll < 0.0 || self.vectorize < 0.0 || self.cache_block < 0.0 {
            return false;
        }
        self.unroll * self.vectorize <= CORRECTNESS_BOUND
    }
}

// --- The runtime model ------------------------------------------------------

/// Mean (noise-free) normalized speedup of `recipe` on `split`, in `[0, 1]`.
///
/// An incorrect recipe scores [`INCORRECT_VALUE`] regardless of how "fast" it looks.
/// A correct recipe sits in a quadratic bowl around the split's hidden optimum: at the
/// optimum the runtime is `FLOOR_FRACTION` of baseline (speedup `1 - FLOOR_FRACTION`),
/// and it degrades smoothly toward zero speedup as the recipe moves away.
fn mean_speedup(recipe: &OptRecipe, split: Split) -> f64 {
    if !recipe.is_correct() {
        return INCORRECT_VALUE;
    }
    let opt = match split {
        Split::Dev => OPT_DEV,
        Split::HeldOut => OPT_HELDOUT,
    };
    let knobs = [recipe.unroll, recipe.vectorize, recipe.cache_block];
    // Squared distance from the split optimum, weighted by per-knob curvature.
    let mut penalty = 0.0;
    for i in 0..RECIPE_DIM {
        let d = knobs[i] - opt[i];
        penalty += CURVATURE[i] * d * d;
    }
    // Runtime as a fraction of baseline: floor at the optimum, rising with penalty.
    // Speedup is 1 - runtime_fraction, clamped into [0, 1].
    let runtime_fraction = FLOOR_FRACTION + penalty;
    let speedup = 1.0 - runtime_fraction;
    speedup.clamp(0.0, 1.0)
}

// --- Scorer (researcher-visible dev signal + Referee held-out re-score) ------

/// Scores a program-superopt candidate ([`GenericArtifact`] of
/// [`ArtifactKind::Program`]) by modeling its benchmark runtime and reporting the mean
/// normalized speedup over `eval_instances` benchmark instances, with a normal 95% CI.
/// `value` is the speedup in `[0, 1]` so higher is better and the orchestrator computes
/// a positive lift for a genuine speedup.
///
/// The same scorer serves two roles, distinguished only by [`Split`]:
/// - [`Split::Dev`] — the researcher-visible signal the [`SupervisorEngine`] hill-climbs
///   (optimum at `OPT_DEV`).
/// - [`Split::HeldOut`] — the Referee's re-score (optimum at `OPT_HELDOUT`). A recipe
///   that over-fits the dev instance lands slightly off the held-out optimum, so its
///   held-out speedup is a touch lower — and a fast-but-incorrect recipe scores ~0 on
///   both, so it can never clear the gate.
#[derive(Clone, Copy, Debug)]
pub struct ProgramScorer {
    eval_instances: u32,
}

impl ProgramScorer {
    /// `eval_instances` should be at least the gate's `min_n` (12) for the result to be
    /// admissible.
    #[must_use]
    pub fn new(eval_instances: u32) -> Self {
        Self { eval_instances }
    }

    /// Synchronous scoring core, exposed so sync callers and unit tests can re-score a
    /// candidate without driving the always-ready [`Scorer::score`] future.
    #[must_use]
    pub fn measure(&self, artifact: &GenericArtifact, split: Split) -> Measurement {
        let recipe = OptRecipe::from_params(&artifact.params);
        let mean = mean_speedup(&recipe, split);
        let split_word: u64 = match split {
            Split::Dev => 0x0000_0000_0000_D0D0,
            Split::HeldOut => 0x0000_0000_0000_4E1D,
        };
        let base_mix = recipe_mix(&artifact.params) ^ split_word;

        // Evaluate over benchmark instances; each instance sees the same recipe with a
        // small, deterministic measurement-noise perturbation, giving a real sample
        // distribution and therefore a real CI. An incorrect recipe pins to ~0 with no
        // noise so it is unambiguously refused.
        let n = self.eval_instances.max(1);
        let mut samples = Vec::with_capacity(n as usize);
        for instance in 0..n {
            let value = if recipe.is_correct() {
                let mix = base_mix ^ u64::from(instance).wrapping_mul(0x9E37_79B1);
                (mean + EVAL_NOISE * jitter(mix)).clamp(0.0, 1.0)
            } else {
                INCORRECT_VALUE
            };
            samples.push(value);
        }

        let nf = f64::from(n);
        let mean_sample = samples.iter().sum::<f64>() / nf;
        let var = if n > 1 {
            samples
                .iter()
                .map(|x| (x - mean_sample).powi(2))
                .sum::<f64>()
                / (nf - 1.0)
        } else {
            0.0
        };
        let se = (var / nf).sqrt();
        let half = Z_95 * se;
        Measurement {
            value: mean_sample,
            ci_lower: mean_sample - half,
            ci_upper: mean_sample + half,
            n,
            cost: nf,
        }
    }
}

impl Scorer for ProgramScorer {
    type Artifact = GenericArtifact;

    fn id(&self) -> &str {
        "program-superopt-heldout"
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

/// The baseline artifact: the un-optimized kernel (all-zero params). A candidate must
/// beat *this* on held-out to certify a speedup. Mirrors `TrainingRecipe::baseline`.
#[must_use]
pub fn baseline_artifact() -> GenericArtifact {
    GenericArtifact::baseline(ArtifactKind::Program, RECIPE_DIM, "un-optimized kernel")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(unroll: f64, vectorize: f64, cache_block: f64) -> GenericArtifact {
        GenericArtifact::new(
            ArtifactKind::Program,
            vec![unroll, vectorize, cache_block],
            "kernel",
        )
    }

    #[test]
    fn measurement_is_deterministic_per_recipe() {
        let s = ProgramScorer::new(16);
        let r = recipe(4.0, 8.0, 2.0);
        assert_eq!(
            s.measure(&r, Split::Dev).value,
            s.measure(&r, Split::Dev).value,
            "scoring must be byte-reproducible"
        );
    }

    #[test]
    fn distinct_recipes_give_spread() {
        let s = ProgramScorer::new(16);
        let a = s.measure(&recipe(3.0, 7.0, 1.0), Split::Dev).value;
        let b = s.measure(&recipe(5.0, 9.0, 3.0), Split::Dev).value;
        assert_ne!(a, b, "different recipes should score differently");
    }

    #[test]
    fn baseline_has_no_speedup() {
        let s = ProgramScorer::new(16);
        // The un-optimized baseline is far from the optimum -> ~zero speedup.
        let v = s.measure(&baseline_artifact(), Split::HeldOut).value;
        assert!(v < 0.05, "un-optimized baseline must show ~no speedup: {v}");
    }

    #[test]
    fn optimum_recipe_beats_baseline_on_heldout() {
        let s = ProgramScorer::new(16);
        let base = s.measure(&baseline_artifact(), Split::HeldOut).value;
        // Score the HELD-OUT optimum on held-out: the best a recipe can do there.
        let opt = OptRecipe::optimum_artifact(Split::HeldOut);
        let tuned = s.measure(&opt, Split::HeldOut).value;
        assert!(
            tuned > base + 0.5,
            "the optimum recipe must clearly beat baseline on held-out: {base} -> {tuned}"
        );
        // And it sits near the modeled speedup ceiling (1 - FLOOR_FRACTION).
        assert!(tuned > 0.6, "optimum speedup near ceiling: {tuned}");
    }

    #[test]
    fn incorrect_recipe_scores_zero_even_if_aggressive() {
        let s = ProgramScorer::new(16);
        // unroll * vectorize = 16 * 16 = 256 > CORRECTNESS_BOUND: fast-looking but WRONG.
        let aggressive = recipe(16.0, 16.0, 2.0);
        assert!(
            !OptRecipe::from_params(&aggressive.params).is_correct(),
            "this recipe must be modeled as incorrect"
        );
        let dev = s.measure(&aggressive, Split::Dev).value;
        let heldout = s.measure(&aggressive, Split::HeldOut).value;
        assert_eq!(
            dev, INCORRECT_VALUE,
            "incorrect program has no value on dev"
        );
        assert_eq!(
            heldout, INCORRECT_VALUE,
            "incorrect program has no value on held-out"
        );
    }

    #[test]
    fn overfitting_dev_generalizes_slightly_worse() {
        let s = ProgramScorer::new(64);
        // The DEV optimum scored on each split: it is perfect on dev, a touch off on
        // held-out (the optimum shifted), so held-out speedup is lower than dev.
        let dev_opt = OptRecipe::optimum_artifact(Split::Dev);
        let dev_v = s.measure(&dev_opt, Split::Dev).value;
        let held_v = s.measure(&dev_opt, Split::HeldOut).value;
        assert!(
            dev_v > held_v,
            "a recipe tuned to the dev instance must generalize slightly worse: dev={dev_v} held={held_v}"
        );
    }

    #[test]
    fn correctness_boundary_is_where_modeled() {
        // Just inside the bound is correct; just outside is not.
        let inside = OptRecipe::from_params(&[10.0, 12.0, 2.0]); // 120 == bound
        let outside = OptRecipe::from_params(&[11.0, 12.0, 2.0]); // 132 > bound
        assert!(inside.is_correct());
        assert!(!outside.is_correct());
        // A negative knob is un-decodable -> incorrect.
        assert!(!OptRecipe::from_params(&[-1.0, 8.0, 2.0]).is_correct());
    }
}
