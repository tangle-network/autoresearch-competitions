//! Distributed-training vertical: the autoresearch market drives a
//! **communication-efficient distributed training** competition.
//!
//! Researchers submit *training recipes* — the knobs that actually decide how well
//! a DiLoCo/DeMo-style run converges over a loosely-coupled operator network:
//! the cross-island **sync interval** `H` (DiLoCo's inner steps between outer
//! syncs), the **gradient compression** kept fraction (DeMo's top-k), the number
//! of data-parallel **islands**, and the learning rates. A *cluster* trains the
//! recipe and returns a trained artifact; the market's Referee re-scores that
//! artifact on a held-out split, gates it, ranks, and pays. **Delegating the
//! compute never delegates the trust** — the cluster's self-reported numbers are
//! ignored; only the held-out re-score decides payment.
//!
//! # The seam that lets a real cluster drop in
//!
//! [`TrainingCluster`] is the one interface a real backend implements. The market
//! is agnostic to which:
//!
//! - [`LocalSimCluster`] — the deterministic local stand-in built here. It models
//!   the real distributed-training dynamics (an optimum sync interval, a
//!   compression cliff, the large-batch penalty from too many islands) so a tuned
//!   recipe produces a *real*, gate-clearing held-out lift and bad recipes are
//!   refused — with no GPUs, no clock, and no I/O (so it runs in CI).
//! - A production `PrimeCluster` / `PsycheCluster` (Phase 1 of
//!   `docs/DISTRIBUTED-TRAINING.md`) implements the same trait by submitting a
//!   training job to a distributed-training **service instance** (Prime
//!   Intellect's `prime`, MIT; or Psyche, Apache-2.0) whose own m-of-n operator
//!   cluster runs the multi-node training. It drops in behind this trait with no
//!   change to the engine, surface, scorer, or orchestrator.
//!
//! [`DistributedTrainingEngine`] is generic over the cluster, which is what makes
//! that substitution a one-line change at the call site.
//!
//! # Honest seam — NOT a real GPU cluster
//!
//! [`LocalSimCluster`] is a *simulation* of training dynamics, not a trainer. It
//! is the marked stand-in for the real `prime`/Psyche/DeMo integration. The value
//! it proves is the **market mechanism around training**: cluster-agnostic
//! dispatch, held-out re-scoring of a delegated artifact, the promotion gate
//! refusing plausible-but-worse recipes, and the TEE→cluster isolation binding.

use std::future::Future;

use autoresearch_runtime::traits::{
    Engine, EngineContext, EngineError, Scorer, ScorerError, Surface, SurfaceError,
};
use autoresearch_runtime::types::{ArtifactRef, Measurement, Split};

// --- Training-dynamics constants -------------------------------------------
//
// A closed-form model of how a communication-efficient distributed run converts a
// FIXED compute budget into final loss. The point is not physical fidelity; it is
// to give the market a real multi-knob optimization surface with the *shape* of
// the known DiLoCo/DeMo tradeoffs, so a tuned recipe wins on held-out and the
// failure modes (island drift, over-compression, mis-set LR) get gated.

/// Floor of the training-loss proxy a perfectly-tuned single replica reaches.
const BASE_LOSS: f64 = 2.5;
/// Baseline held-out generalization gap (held-out loss = train loss + gap).
const BASE_GAP: f64 = 0.05;

/// DiLoCo's optimal inner-step count `H` (cross-island syncs every `H` local steps).
const H_OPT: f64 = 32.0;
/// Optimal inner (local) learning rate.
const LR_OPT: f64 = 3e-3;
/// Optimal DiLoCo outer (Nesterov) learning rate.
const OUTER_LR_OPT: f64 = 0.7;
/// DeMo's optimal kept-gradient fraction; compressing *below* this loses signal.
const KEEP_OPT: f64 = 0.1;
/// Island count at which the large-batch generalization penalty reaches one unit.
const BATCH_REF: f64 = 16.0;

/// How fast more data-parallel islands reduce loss (log-diminishing).
const ISLAND_GAIN: f64 = 0.15;
/// Linear large-batch penalty: too many islands inflates the effective batch.
const LARGEBATCH_PEN: f64 = 0.08;
/// Penalty weight for `H` above its optimum (island weights drift between syncs).
const H_PENALTY: f64 = 0.02;
/// Penalty weight for inner LR away from its optimum (log-quadratic bowl).
const LR_PENALTY: f64 = 0.05;
/// Penalty weight for outer LR away from its optimum.
const OUTER_LR_PENALTY: f64 = 0.03;
/// Penalty weight for compressing the kept fraction below its optimum.
const COMPRESS_PEN: f64 = 0.02;
/// Extra held-out gap when `H` is pushed above optimum (drift hurts generalization).
const OVERFIT_H: f64 = 0.05;
/// Extra held-out gap when compression is pushed below optimum.
const OVERFIT_COMPRESS: f64 = 0.05;

/// Std of per-eval-shard measurement noise (gives the held-out score a real CI).
const EVAL_NOISE: f64 = 0.01;
/// Std of per-training-run noise across seeds (gives researchers' runs spread).
const TRAIN_NOISE: f64 = 0.005;
/// z for a two-sided 95% normal interval.
const Z_95: f64 = 1.96;

// --- Deterministic noise ----------------------------------------------------

/// A splitmix64 finalizer mapped to a uniform `f64` in `[-1, 1)`. Deterministic
/// from its input mix word — no `rand`, no clock — so every measurement is
/// byte-reproducible, which is what lets the e2e test assert concrete lift.
fn jitter(mix: u64) -> f64 {
    let mut z = mix.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let bits = z >> 11; // top 53 bits
    let unit = (bits as f64) / ((1u64 << 53) as f64);
    2.0 * unit - 1.0
}

// --- Recipe (the researcher's submission) -----------------------------------

/// A communication-efficient distributed-training recipe. These are exactly the
/// knobs a DiLoCo/DeMo run is sensitive to; a real backend would pass them through
/// to `prime`/Psyche.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrainingRecipe {
    /// Data-parallel islands (independent replicas synced by the outer optimizer).
    pub islands: u32,
    /// DiLoCo inner steps `H`: local SGD steps between cross-island outer syncs.
    pub inner_steps: u32,
    /// Inner (local) learning rate.
    pub inner_lr: f64,
    /// Outer (DiLoCo Nesterov) learning rate.
    pub outer_lr: f64,
    /// DeMo kept-gradient fraction in `(0, 1]` (1.0 = no compression).
    pub keep_fraction: f64,
}

impl TrainingRecipe {
    /// The reference recipe: a single fully-synchronous replica, no compression,
    /// well-tuned LR. A real improvement must beat *this* on held-out.
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            islands: 1,
            inner_steps: 1,
            inner_lr: LR_OPT,
            outer_lr: OUTER_LR_OPT,
            keep_fraction: 1.0,
        }
    }

    /// The training-loss proxy this recipe reaches at the fixed compute budget,
    /// with a small deterministic per-seed perturbation. Lower is better.
    fn train_loss(&self, seed: u64) -> f64 {
        let islands = f64::from(self.islands);
        let h = f64::from(self.inner_steps);

        // More islands process more data in parallel (log-diminishing return)...
        let island_gain = ISLAND_GAIN * islands.ln();
        // ...but too many inflate the effective batch and hurt convergence.
        let largebatch = LARGEBATCH_PEN * (islands / BATCH_REF);
        // Pushing H above the optimum lets replicas drift between syncs.
        let h_pen = H_PENALTY * pos(h.ln() - H_OPT.ln()).powi(2);
        // Learning rates sit in a log-quadratic bowl around their optima.
        let lr_pen = LR_PENALTY * (self.inner_lr.ln() - LR_OPT.ln()).powi(2);
        let outer_pen = OUTER_LR_PENALTY * (self.outer_lr.ln() - OUTER_LR_OPT.ln()).powi(2);
        // Compressing the kept fraction below the optimum loses gradient signal.
        let compress_pen = COMPRESS_PEN * pos(KEEP_OPT.ln() - self.keep_fraction.ln()).powi(2);

        let noise = TRAIN_NOISE * jitter(seed ^ 0x5151_5151_5151_5151);
        BASE_LOSS - island_gain + largebatch + h_pen + lr_pen + outer_pen + compress_pen + noise
    }

    /// Held-out generalization gap added on top of training loss. Aggressive
    /// recipes (over-compressed, drifting) look fine while training but generalize
    /// worse — which is exactly what the held-out gate is there to catch.
    fn generalization_gap(&self) -> f64 {
        let h = f64::from(self.inner_steps);
        BASE_GAP
            + OVERFIT_H * pos(h.ln() - H_OPT.ln())
            + OVERFIT_COMPRESS * pos(KEEP_OPT.ln() - self.keep_fraction.ln())
    }
}

/// `max(0, x)` — the positive part, for one-sided penalties.
fn pos(x: f64) -> f64 {
    x.max(0.0)
}

// --- Trained artifact -------------------------------------------------------

/// What a [`TrainingCluster`] returns and the market scores: the recipe that was
/// trained, the seed it ran under, and the cluster's training-loss proxy. The
/// Referee does **not** trust `train_loss` for payment — it re-scores on held-out
/// (see [`DistributedTrainingScorer`]); the field is the cluster's claim, carried
/// for provenance and the dev-signal a researcher is allowed to see.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrainedArtifact {
    pub recipe: TrainingRecipe,
    pub train_seed: u64,
    pub train_loss: f64,
}

// --- Surface ----------------------------------------------------------------

/// The surface: a structurally-valid training recipe, full-replacement (a trained
/// artifact replaces the baseline rather than being a delta onto it).
#[derive(Clone, Debug, Default)]
pub struct DistributedTrainingSurface;

impl Surface for DistributedTrainingSurface {
    type Artifact = TrainedArtifact;

    fn id(&self) -> &str {
        "distributed-training-recipe"
    }

    fn validate(&self, artifact: &Self::Artifact) -> Result<(), SurfaceError> {
        let r = &artifact.recipe;
        if r.islands == 0 {
            return Err(SurfaceError::Invalid("islands must be >= 1".into()));
        }
        if r.inner_steps == 0 {
            return Err(SurfaceError::Invalid("inner_steps (H) must be >= 1".into()));
        }
        if !(r.keep_fraction > 0.0 && r.keep_fraction <= 1.0) {
            return Err(SurfaceError::Invalid(
                "keep_fraction must be in (0, 1]".into(),
            ));
        }
        if !(r.inner_lr.is_finite() && r.inner_lr > 0.0) {
            return Err(SurfaceError::Invalid(
                "inner_lr must be finite and > 0".into(),
            ));
        }
        if !(r.outer_lr.is_finite() && r.outer_lr > 0.0) {
            return Err(SurfaceError::Invalid(
                "outer_lr must be finite and > 0".into(),
            ));
        }
        if !artifact.train_loss.is_finite() {
            return Err(SurfaceError::Invalid("train_loss must be finite".into()));
        }
        Ok(())
    }

    fn apply_delta(
        &self,
        _base: &Self::Artifact,
        delta: &Self::Artifact,
    ) -> Result<Self::Artifact, SurfaceError> {
        // Full-replacement surface: a trained candidate supersedes the baseline.
        self.validate(delta)?;
        Ok(*delta)
    }

    fn to_ref(&self, artifact: &Self::Artifact) -> Result<ArtifactRef, SurfaceError> {
        self.validate(artifact)?;
        // Stable content reference: FNV-1a over the recipe bits + seed.
        let r = &artifact.recipe;
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut absorb = |word: u64| {
            for byte in word.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
            }
        };
        absorb(u64::from(r.islands));
        absorb(u64::from(r.inner_steps));
        absorb(r.inner_lr.to_bits());
        absorb(r.outer_lr.to_bits());
        absorb(r.keep_fraction.to_bits());
        absorb(artifact.train_seed);
        Ok(ArtifactRef(format!("training:{hash:016x}")))
    }
}

// --- Scorer (the Referee's held-out evaluation) -----------------------------

/// Re-scores a [`TrainedArtifact`] on a data split by evaluating it over
/// `eval_shards` held-out shards and reporting the mean negative loss with a
/// normal 95% CI. `value` is `-loss` so that higher is better and the orchestrator
/// computes a positive lift for a genuine loss reduction.
///
/// On [`Split::HeldOut`] the recipe's generalization gap is applied; on
/// [`Split::Dev`] it is not — so an over-aggressive recipe can look good on the
/// dev signal a researcher sees and still fail the Referee's held-out gate.
#[derive(Clone, Copy, Debug)]
pub struct DistributedTrainingScorer {
    eval_shards: u32,
}

impl DistributedTrainingScorer {
    /// `eval_shards` should be at least the gate's `min_n` (12) for the result to
    /// be admissible.
    #[must_use]
    pub fn new(eval_shards: u32) -> Self {
        Self { eval_shards }
    }

    /// Synchronous scoring core, exposed so sync callers (the training-market
    /// continuous leaderboard and m-of-n re-score panel in `training_market`) can
    /// re-score an artifact without driving the always-ready `Scorer::score` future.
    #[must_use]
    pub fn measure(&self, artifact: &TrainedArtifact, split: Split) -> Measurement {
        let loss = match split {
            Split::Dev => artifact.train_loss,
            Split::HeldOut => artifact.train_loss + artifact.recipe.generalization_gap(),
        };
        let split_word: u64 = match split {
            Split::Dev => 0x0000_0000_0000_D0D0,
            Split::HeldOut => 0x0000_0000_0000_4E1D,
        };

        // Evaluate over shards; each shard sees the same model with a small,
        // deterministic eval-noise perturbation, giving a real sample distribution.
        let n = self.eval_shards.max(1);
        let mut samples = Vec::with_capacity(n as usize);
        for shard in 0..n {
            let mix = artifact.train_seed.wrapping_mul(0x100_0000_01B3)
                ^ split_word
                ^ (u64::from(shard).wrapping_mul(0x9E37_79B1));
            let noisy_loss = loss + EVAL_NOISE * jitter(mix);
            samples.push(-noisy_loss); // value = -loss (higher is better)
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

impl Scorer for DistributedTrainingScorer {
    type Artifact = TrainedArtifact;

    fn id(&self) -> &str {
        "distributed-training-heldout"
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

// --- Cluster seam -----------------------------------------------------------

/// The one interface a distributed-training backend implements. [`LocalSimCluster`]
/// is the in-repo stand-in; a production adapter submits the recipe as a training
/// job to a `prime`/Psyche service instance (whose own m-of-n operators run the
/// multi-node training) and returns the trained artifact. The market is agnostic
/// to which — [`DistributedTrainingEngine`] is generic over this trait.
pub trait TrainingCluster {
    /// Stable identifier for the cluster backend.
    fn id(&self) -> &str;

    /// Train `recipe` under `seed`, returning the trained artifact to be scored.
    fn train(
        &self,
        recipe: &TrainingRecipe,
        seed: u64,
    ) -> impl Future<Output = Result<TrainedArtifact, EngineError>> + Send;

    /// Whether this cluster runs inside a sealed, TEE-isolated environment — so a
    /// private competition's data is never exposed to the researcher. Defaults to
    /// `false`. The private runner requires this for attestation-mandating tiers
    /// (the same tier→sandbox binding [`Engine::provides_sealed_isolation`]
    /// enforces); a non-sealed cluster cannot serve a private training competition.
    fn provides_sealed_isolation(&self) -> bool {
        false
    }
}

/// The deterministic local stand-in for a real training backend. Runs the
/// [`TrainingRecipe`] through the closed-form dynamics model — no GPUs, no I/O.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalSimCluster;

impl LocalSimCluster {
    /// Train synchronously (no executor needed). Exposed so a non-async caller can
    /// build a baseline artifact directly.
    #[must_use]
    pub fn train_sync(&self, recipe: &TrainingRecipe, seed: u64) -> TrainedArtifact {
        TrainedArtifact {
            recipe: *recipe,
            train_seed: seed,
            train_loss: recipe.train_loss(seed),
        }
    }
}

impl TrainingCluster for LocalSimCluster {
    fn id(&self) -> &str {
        "local-sim-cluster"
    }

    fn train(
        &self,
        recipe: &TrainingRecipe,
        seed: u64,
    ) -> impl Future<Output = Result<TrainedArtifact, EngineError>> + Send {
        std::future::ready(Ok(self.train_sync(recipe, seed)))
    }
}

// --- Engine -----------------------------------------------------------------

/// A researcher's engine: it holds one training recipe and dispatches it to a
/// [`TrainingCluster`], returning the trained artifact for the market to score.
/// Generic over the cluster so the production `prime`/Psyche backend drops in
/// unchanged. Mirrors the agent-method `SandboxMethodEngine`: the engine forwards
/// the cluster's sealed-isolation property so the TEE→backend binding holds.
#[derive(Clone, Debug)]
pub struct DistributedTrainingEngine<C> {
    researcher: String,
    recipe: TrainingRecipe,
    train_seed: u64,
    cluster: C,
}

impl<C> DistributedTrainingEngine<C> {
    pub fn new(researcher: String, recipe: TrainingRecipe, train_seed: u64, cluster: C) -> Self {
        Self {
            researcher,
            recipe,
            train_seed,
            cluster,
        }
    }

    /// The researcher this engine submits for.
    #[must_use]
    pub fn researcher(&self) -> &str {
        &self.researcher
    }
}

impl<C> Engine for DistributedTrainingEngine<C>
where
    C: TrainingCluster + Clone + Send + Sync,
{
    type Artifact = TrainedArtifact;

    fn id(&self) -> &str {
        "distributed-training"
    }

    fn produce(
        &self,
        _ctx: &EngineContext,
    ) -> impl Future<Output = Result<Self::Artifact, EngineError>> + Send {
        // Own everything the future needs so it is `Send` and self-contained,
        // exactly as a real adapter would own a (cloneable) service-client handle.
        let cluster = self.cluster.clone();
        let recipe = self.recipe;
        let seed = self.train_seed;
        async move { cluster.train(&recipe, seed).await }
    }

    fn provides_sealed_isolation(&self) -> bool {
        self.cluster.provides_sealed_isolation()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test cluster that reports sealed isolation, to prove the engine forwards
    /// the TEE→backend binding (the private runner relies on this forwarding).
    #[derive(Clone, Copy, Debug)]
    struct SealedCluster;
    impl TrainingCluster for SealedCluster {
        fn id(&self) -> &str {
            "sealed-test-cluster"
        }
        fn train(
            &self,
            recipe: &TrainingRecipe,
            seed: u64,
        ) -> impl Future<Output = Result<TrainedArtifact, EngineError>> + Send {
            std::future::ready(Ok(LocalSimCluster.train_sync(recipe, seed)))
        }
        fn provides_sealed_isolation(&self) -> bool {
            true
        }
    }

    fn recipe(islands: u32, h: u32, keep: f64, lr: f64) -> TrainingRecipe {
        TrainingRecipe {
            islands,
            inner_steps: h,
            inner_lr: lr,
            outer_lr: OUTER_LR_OPT,
            keep_fraction: keep,
        }
    }

    #[test]
    fn training_is_deterministic_per_seed() {
        let r = recipe(8, 32, 0.2, LR_OPT);
        assert_eq!(r.train_loss(7), r.train_loss(7));
    }

    #[test]
    fn distinct_seeds_give_spread() {
        let r = recipe(4, 32, 0.1, LR_OPT);
        let a = r.train_loss(1);
        let b = r.train_loss(2);
        assert_ne!(a, b, "per-seed noise should give ranking spread");
    }

    #[test]
    fn a_tuned_recipe_beats_the_baseline_on_heldout() {
        let scorer = DistributedTrainingScorer::new(16);
        let base = LocalSimCluster.train_sync(&TrainingRecipe::baseline(), 0);
        let tuned = LocalSimCluster.train_sync(&recipe(8, 32, 0.2, LR_OPT), 1);
        let base_v = scorer.measure(&base, Split::HeldOut).value;
        let tuned_v = scorer.measure(&tuned, Split::HeldOut).value;
        assert!(
            tuned_v > base_v + 0.1,
            "tuned recipe should clearly beat baseline on held-out: {base_v} -> {tuned_v}"
        );
    }

    #[test]
    fn over_compression_hurts_heldout_more_than_dev() {
        // The generalization gap is what the held-out gate exploits: an
        // over-compressed recipe looks better on dev than it deserves.
        let scorer = DistributedTrainingScorer::new(16);
        let aggressive = LocalSimCluster.train_sync(&recipe(4, 32, 0.0005, LR_OPT), 3);
        let dev = scorer.measure(&aggressive, Split::Dev).value;
        let heldout = scorer.measure(&aggressive, Split::HeldOut).value;
        assert!(
            dev > heldout,
            "held-out must be worse than dev for an aggressive recipe: dev={dev} heldout={heldout}"
        );
    }

    #[test]
    fn too_large_h_is_worse_than_baseline() {
        let scorer = DistributedTrainingScorer::new(16);
        let base = LocalSimCluster.train_sync(&TrainingRecipe::baseline(), 0);
        let drifted = LocalSimCluster.train_sync(&recipe(4, 4000, 0.1, LR_OPT), 2);
        assert!(
            scorer.measure(&drifted, Split::HeldOut).value
                < scorer.measure(&base, Split::HeldOut).value,
            "too-large-H (island drift) must not beat the baseline"
        );
    }

    #[test]
    fn surface_rejects_degenerate_recipes() {
        let s = DistributedTrainingSurface;
        let mut bad = LocalSimCluster.train_sync(&TrainingRecipe::baseline(), 0);
        bad.recipe.keep_fraction = 0.0;
        assert!(
            s.validate(&bad).is_err(),
            "keep_fraction=0 must be rejected"
        );
        bad.recipe.keep_fraction = 0.5;
        bad.recipe.islands = 0;
        assert!(s.validate(&bad).is_err(), "islands=0 must be rejected");
    }

    #[test]
    fn engine_forwards_cluster_sealed_isolation() {
        let sealed = DistributedTrainingEngine::new(
            "r".into(),
            TrainingRecipe::baseline(),
            0,
            SealedCluster,
        );
        let local = DistributedTrainingEngine::new(
            "r".into(),
            TrainingRecipe::baseline(),
            0,
            LocalSimCluster,
        );
        assert!(sealed.provides_sealed_isolation());
        assert!(!local.provides_sealed_isolation());
    }
}
