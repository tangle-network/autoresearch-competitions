//! A self-contained, fully deterministic demo vertical: tune the weights of a
//! linear classifier so its held-out accuracy beats a zero-weight baseline.
//!
//! Everything here is reproducible — no `rand` crate, no clock, no I/O. A seeded
//! linear-congruential generator (LCG) synthesizes the dataset once at scorer
//! construction, and the search engine uses its own LCG seeded by the run seed.
//! Running the same seeds twice yields byte-identical lift numbers, which is what
//! lets the M1 end-to-end test assert on a concrete improvement.
//!
//! # Production seam
//!
//! [`LinearScorer`] and [`LocalSearchEngine`] are the **local stand-ins** for the
//! production pieces:
//!
//! - `LinearScorer` stands in for a real agent-profile scorer, which would run a
//!   submitted profile against held-out task suites.
//! - `LocalSearchEngine` stands in for an external agent-loop engine, which would
//!   drive a real self-improvement loop.
//!
//! Both implement the same [`Surface`]/[`Scorer`]/[`Engine`] traits the orchestrator
//! consumes, so the production adapters drop in with no orchestrator change. The
//! lift this vertical produces is a *real* measured accuracy gain on held-out data,
//! not a hardcoded number — the only thing that is synthetic is the dataset itself.

use std::future::Future;

use autoresearch_runtime::traits::{
    Engine, EngineContext, EngineError, Scorer, ScorerError, Surface, SurfaceError,
};
use autoresearch_runtime::types::{ArtifactRef, Measurement, Split};

/// Feature dimensionality of the synthetic task.
const D: usize = 4;
/// Total samples generated.
const N_TOTAL: usize = 200;
/// Samples assigned to the dev split (researcher-visible). Remainder is held out.
const N_DEV: usize = 120;
/// The ground-truth separating hyperplane. A good search recovers a vector
/// pointing the same direction as this and classifies held-out points well.
const W_TRUE: [f64; D] = [1.0, -2.0, 0.5, 1.5];
/// z for a two-sided 95% Wilson interval.
const Z_95: f64 = 1.96;

// --- Deterministic PRNG -----------------------------------------------------

/// A 64-bit linear-congruential generator (the Knuth MMIX constants). Used for
/// both dataset synthesis and the search engine so the whole vertical is seedable
/// and reproducible without any external RNG.
#[derive(Clone, Debug)]
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        // Offset the seed so seed=0 is not a fixed point of the recurrence's high bits.
        Self {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// Advance and return the new state.
    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }

    /// A uniform `f64` in `[0, 1)` from the high 53 bits (the low bits of an LCG are
    /// weak; the high bits are well-distributed).
    fn next_unit(&mut self) -> f64 {
        let bits = self.next_u64() >> 11; // top 53 bits
        (bits as f64) / ((1u64 << 53) as f64)
    }

    /// A uniform `f64` in `[-1, 1)`.
    fn next_signed(&mut self) -> f64 {
        2.0 * self.next_unit() - 1.0
    }
}

// --- Artifact + Surface -----------------------------------------------------

/// The thing researchers submit: a `D`-dimensional weight vector for the linear
/// classifier. The baseline is the all-zeros vector (chance-level accuracy).
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigArtifact {
    pub params: Vec<f64>,
}

impl ConfigArtifact {
    /// The baseline artifact: zero weights => `sign(0) > 0` is false for every
    /// sample, so accuracy is just the fraction of negatives (~chance).
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            params: vec![0.0; D],
        }
    }
}

/// The surface: a fixed-length, finite weight vector with elementwise additive deltas.
#[derive(Clone, Debug, Default)]
pub struct ConfigSurface;

impl Surface for ConfigSurface {
    type Artifact = ConfigArtifact;

    fn id(&self) -> &str {
        "linear-config"
    }

    fn validate(&self, artifact: &Self::Artifact) -> Result<(), SurfaceError> {
        if artifact.params.len() != D {
            return Err(SurfaceError::Invalid(format!(
                "expected {D} params, got {}",
                artifact.params.len()
            )));
        }
        if artifact.params.iter().any(|p| !p.is_finite()) {
            return Err(SurfaceError::Invalid("params must be finite".into()));
        }
        Ok(())
    }

    fn apply_delta(
        &self,
        base: &Self::Artifact,
        delta: &Self::Artifact,
    ) -> Result<Self::Artifact, SurfaceError> {
        if base.params.len() != delta.params.len() {
            return Err(SurfaceError::Apply("length mismatch".into()));
        }
        Ok(ConfigArtifact {
            params: base
                .params
                .iter()
                .zip(&delta.params)
                .map(|(b, d)| b + d)
                .collect(),
        })
    }

    fn to_ref(&self, artifact: &Self::Artifact) -> Result<ArtifactRef, SurfaceError> {
        self.validate(artifact)?;
        // Stable content reference: a FNV-1a hash over the bit pattern of each param.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for p in &artifact.params {
            for byte in p.to_bits().to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
            }
        }
        Ok(ArtifactRef(format!("config:{hash:016x}")))
    }
}

// --- Dataset ----------------------------------------------------------------

/// The synthetic, deterministic dataset shared by the scorer and the engine.
#[derive(Clone, Debug)]
struct Dataset {
    /// All `N_TOTAL` feature vectors.
    xs: Vec<[f64; D]>,
    /// Ground-truth labels: `y[i] = (W_TRUE . xs[i]) > 0`.
    ys: Vec<bool>,
}

impl Dataset {
    /// Build the fixed dataset. Seeded so it is identical on every construction.
    fn generate() -> Self {
        // A fixed dataset seed — NOT a parameter — so dev/held-out are stable.
        let mut rng = Lcg::new(0xA1B2_C3D4_E5F6_0718);
        let mut xs = Vec::with_capacity(N_TOTAL);
        let mut ys = Vec::with_capacity(N_TOTAL);
        for _ in 0..N_TOTAL {
            let mut x = [0.0; D];
            for slot in &mut x {
                *slot = rng.next_signed();
            }
            ys.push(dot(&W_TRUE, &x) > 0.0);
            xs.push(x);
        }
        Self { xs, ys }
    }

    /// Index range for a split. First `N_DEV` are dev; the rest are held out.
    fn range(split: Split) -> std::ops::Range<usize> {
        match split {
            Split::Dev => 0..N_DEV,
            Split::HeldOut => N_DEV..N_TOTAL,
        }
    }

    /// Classification accuracy of weight vector `w` over `split`.
    fn accuracy(&self, w: &[f64], split: Split) -> (f64, u32) {
        let range = Self::range(split);
        let n = range.len();
        let mut correct = 0usize;
        for i in range {
            let pred = dot(w, &self.xs[i]) > 0.0;
            if pred == self.ys[i] {
                correct += 1;
            }
        }
        (correct as f64 / n as f64, n as u32)
    }
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Wilson score interval for a binomial proportion. Tighter and better-behaved than
/// the normal (Wald) interval near 0/1, and always inside `[0, 1]`. Returns
/// `(lower, upper)`.
fn wilson_interval(p: f64, n: u32) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    let n = f64::from(n);
    let z2 = Z_95 * Z_95;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let margin = (Z_95 / denom) * ((p * (1.0 - p) / n) + z2 / (4.0 * n * n)).sqrt();
    ((center - margin).max(0.0), (center + margin).min(1.0))
}

// --- Scorer -----------------------------------------------------------------

/// Scores a [`ConfigArtifact`] by its classification accuracy on the requested
/// split, with a Wilson 95% CI. Holds the deterministic dataset.
#[derive(Clone, Debug)]
pub struct LinearScorer {
    data: Dataset,
}

impl LinearScorer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            data: Dataset::generate(),
        }
    }

    /// Synchronous scoring core (also used internally by the engine's search). The
    /// `Scorer` trait wraps this in a ready future.
    fn measure(&self, artifact: &ConfigArtifact, split: Split) -> Measurement {
        let (acc, n) = self.data.accuracy(&artifact.params, split);
        let (lo, hi) = wilson_interval(acc, n);
        Measurement {
            value: acc,
            ci_lower: lo,
            ci_upper: hi,
            n,
            cost: f64::from(n),
        }
    }
}

impl Default for LinearScorer {
    fn default() -> Self {
        Self::new()
    }
}

impl Scorer for LinearScorer {
    type Artifact = ConfigArtifact;

    fn id(&self) -> &str {
        "linear-accuracy"
    }

    fn score(
        &self,
        artifact: &Self::Artifact,
        split: Split,
    ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send {
        // Pure CPU work; resolve immediately. No `async fn` so the future is `Send`
        // regardless of the dataset's contents.
        let m = self.measure(artifact, split);
        std::future::ready(Ok(m))
    }
}

// --- Engine -----------------------------------------------------------------

/// Number of candidate weight vectors a single search samples.
const SEARCH_BUDGET: usize = 200;
/// Magnitude range for sampled weights: `[-SCALE, SCALE)` per component.
const SEARCH_SCALE: f64 = 3.0;

/// A seeded random-search engine: it samples [`SEARCH_BUDGET`] weight vectors from
/// its own LCG, evaluates each on the **dev** split (the only split a researcher may
/// see), and returns the best. Different seeds explore different regions, so two
/// honest researchers get different — but each genuinely good — results, which gives
/// the ranking real spread instead of a tie.
#[derive(Clone, Debug)]
pub struct LocalSearchEngine {
    seed: u64,
}

impl LocalSearchEngine {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self { seed }
    }

    /// Run the search synchronously and return the produced candidate.
    ///
    /// This is the same candidate [`Engine::produce`] yields (which is a ready future
    /// over this), exposed as a plain function so an operator-hosted method runner (the
    /// `autoresearch-sandbox` `LocalMethod` stand-in) can execute the method
    /// in-process without driving an executor. Deterministic per seed.
    #[must_use]
    pub fn produce_candidate(&self) -> ConfigArtifact {
        self.search()
    }

    /// The actual search, factored out so it is trivially deterministic and testable.
    /// Holds its own scorer (its own copy of the dataset) — a researcher's engine
    /// never shares the Referee's scorer instance.
    fn search(&self) -> ConfigArtifact {
        let scorer = LinearScorer::new();
        let mut rng = Lcg::new(self.seed);
        let mut best = ConfigArtifact::baseline();
        let mut best_dev = scorer.measure(&best, Split::Dev).value;
        for _ in 0..SEARCH_BUDGET {
            let params: Vec<f64> = (0..D).map(|_| SEARCH_SCALE * rng.next_signed()).collect();
            let candidate = ConfigArtifact { params };
            let dev_acc = scorer.measure(&candidate, Split::Dev).value;
            if dev_acc > best_dev {
                best_dev = dev_acc;
                best = candidate;
            }
        }
        best
    }
}

impl Engine for LocalSearchEngine {
    type Artifact = ConfigArtifact;

    fn id(&self) -> &str {
        "local-random-search"
    }

    fn produce(
        &self,
        _ctx: &EngineContext,
    ) -> impl Future<Output = Result<Self::Artifact, EngineError>> + Send {
        let artifact = self.search();
        std::future::ready(Ok(artifact))
    }
}

// --- Collaborative contributor (the DeMo local stand-in) --------------------

/// A single contributor's seeded local search over **one slice** of the shared
/// weight vector — the local, deterministic **stand-in for the `DeMoTrainingEngine`**
/// (the real training-blueprint / DeMo distributed-training integration).
///
/// # The collaborative framing
///
/// In `Structure::Collaborative` mode (`docs/MECHANISM.md §6`) many contributors pool
/// compute onto **one shared artifact**. Each contributor emits a **delta** that the
/// collaborative runner folds into the running shared artifact via
/// [`ConfigSurface::apply_delta`] (elementwise add). This contributor produces a delta
/// that is non-zero only on the dimensions it "owns" (`dims`), where it places its
/// best local estimate of the ground-truth weight scaled by `quality`. Folding several
/// contributors that own **different** dimensions progressively recovers the true
/// separating hyperplane [`W_TRUE`], so each contributor adds **distinct, real marginal
/// value** to the shared artifact's held-out accuracy — exactly what the runner's
/// single-permutation marginal attribution prices. (The held-out accuracy is a dot
/// product, so these per-dimension marginals are not perfectly separable and the credit
/// is order-dependent; the runner folds in a canonical order — see `collaborative.rs`.)
///
/// A **free-rider** is a contributor with `quality == 0.0` (or one that owns no
/// dimension): its delta is all-zeros, it moves the shared artifact's held-out score by
/// nothing, and the runner credits it **zero share** — the fairness property that
/// improves on the training-blueprint's GPU-minutes baseline (which would pay it for
/// "burning compute" anyway).
///
/// # Honest seam — NOT a real GPU cluster
///
/// This produces a delta by a **seeded local search on its owned dimensions**, with no
/// `rand`, no clock, and no I/O — the same determinism the rest of the vertical relies
/// on. It is the marked stand-in for the real **DeMo (Decoupled Momentum) distributed
/// training** engine, whose contribution verification today is **GPU-minutes + a
/// statistical gradient check** (a KNOWN GAP — gameable by collusion, no held-out
/// gating; `docs/MECHANISM.md §6.1`). The collaborative runner this contributor feeds
/// **improves on that** by pricing each delta on its **held-out-eval-gated marginal
/// contribution** instead of GPU-minutes (§6.2). We do not claim a real cluster.
#[derive(Clone, Debug)]
pub struct SharedSearchContributor {
    seed: u64,
    /// The dimensions of the shared weight vector this contributor improves. A
    /// contributor owning a non-empty slice adds real marginal value; an empty slice
    /// (or `quality == 0`) is a free-rider that contributes a zero delta.
    dims: Vec<usize>,
    /// How well this contributor recovers the true weight on its owned dims, in
    /// `[0, 1]`. `1.0` recovers [`W_TRUE`] exactly on those dims; `0.0` is a free-rider.
    quality: f64,
}

impl SharedSearchContributor {
    /// A productive contributor that improves the shared artifact on `dims` at the
    /// given `quality` (`1.0` = recovers the true weight on those dims).
    #[must_use]
    pub fn new(seed: u64, dims: Vec<usize>, quality: f64) -> Self {
        Self {
            seed,
            dims,
            quality: quality.clamp(0.0, 1.0),
        }
    }

    /// A free-rider: owns nothing, contributes an all-zeros delta, earns nothing. Named
    /// explicitly so the collaborative tests/e2e read clearly.
    #[must_use]
    pub fn free_rider(seed: u64) -> Self {
        Self {
            seed,
            dims: Vec::new(),
            quality: 0.0,
        }
    }

    /// Produce this contributor's delta: zero everywhere except its owned dimensions,
    /// where it places a seeded local estimate of `quality * W_TRUE[dim]`.
    ///
    /// The seed perturbs the estimate deterministically so two contributors owning the
    /// same dimension would still differ (no tie), but the perturbation is small enough
    /// that a productive contributor reliably moves held-out accuracy. A free-rider
    /// (`quality == 0` or no dims) returns the all-zeros delta.
    fn delta(&self) -> ConfigArtifact {
        let mut params = vec![0.0; D];
        if self.quality > 0.0 {
            let mut rng = Lcg::new(self.seed);
            for &dim in &self.dims {
                if dim < D {
                    // Recover quality * W_TRUE[dim] with a small deterministic jitter.
                    let jitter = 0.05 * rng.next_signed();
                    params[dim] = self.quality * W_TRUE[dim] + jitter;
                }
            }
        }
        ConfigArtifact { params }
    }
}

impl Engine for SharedSearchContributor {
    type Artifact = ConfigArtifact;

    fn id(&self) -> &str {
        "shared-search-contributor"
    }

    fn produce(
        &self,
        _ctx: &EngineContext,
    ) -> impl Future<Output = Result<Self::Artifact, EngineError>> + Send {
        std::future::ready(Ok(self.delta()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_is_deterministic() {
        let a = Dataset::generate();
        let b = Dataset::generate();
        assert_eq!(a.xs.len(), N_TOTAL);
        assert_eq!(a.ys, b.ys);
        for (x, y) in a.xs.iter().zip(&b.xs) {
            assert_eq!(x, y);
        }
    }

    #[test]
    fn labels_are_balanced_enough_to_be_a_real_task() {
        // If labels were all one class, accuracy would be trivially gameable.
        let data = Dataset::generate();
        let positives = data.ys.iter().filter(|&&y| y).count();
        assert!(
            (40..=160).contains(&positives),
            "labels should not be degenerate: {positives}/200 positive"
        );
    }

    #[test]
    fn baseline_is_near_chance_on_heldout() {
        let scorer = LinearScorer::new();
        let m = scorer.measure(&ConfigArtifact::baseline(), Split::HeldOut);
        assert!(
            m.value <= 0.65,
            "zero-weight baseline should be near chance, got {}",
            m.value
        );
        assert_eq!(m.n, (N_TOTAL - N_DEV) as u32);
    }

    #[test]
    fn ground_truth_weights_are_near_perfect() {
        // Sanity: the true separator must classify held-out points (almost) perfectly,
        // otherwise the task is not learnable and no engine could clear the gate.
        let scorer = LinearScorer::new();
        let m = scorer.measure(
            &ConfigArtifact {
                params: W_TRUE.to_vec(),
            },
            Split::HeldOut,
        );
        assert!(
            m.value > 0.95,
            "W_TRUE should be near-perfect, got {}",
            m.value
        );
    }

    #[test]
    fn search_recovers_a_strong_classifier() {
        let scorer = LinearScorer::new();
        let engine = LocalSearchEngine::new(7);
        let cand = engine.search();
        let heldout = scorer.measure(&cand, Split::HeldOut).value;
        // Generalization: tuned on dev, must still be strong on held-out.
        assert!(
            heldout > 0.75,
            "search should generalize to held-out, got {heldout}"
        );
    }

    #[test]
    fn search_is_deterministic_per_seed() {
        let a = LocalSearchEngine::new(42).search();
        let b = LocalSearchEngine::new(42).search();
        assert_eq!(a, b);
    }

    #[test]
    fn distinct_seeds_give_distinct_results() {
        // Real ranking requires spread; identical results across seeds would be a tie.
        let scorer = LinearScorer::new();
        let accs: Vec<f64> = [1u64, 2, 3, 4, 5]
            .iter()
            .map(|&s| {
                scorer
                    .measure(&LocalSearchEngine::new(s).search(), Split::HeldOut)
                    .value
            })
            .collect();
        let distinct = accs.windows(2).any(|w| w[0] != w[1]);
        assert!(
            distinct,
            "seeds should produce a spread of results: {accs:?}"
        );
    }

    #[test]
    fn contributor_delta_is_zero_off_owned_dims() {
        // A contributor owning dim 0 produces a delta non-zero only at dim 0.
        let c = SharedSearchContributor::new(1, vec![0], 1.0);
        let d = c.delta();
        assert!(d.params[0].abs() > 0.1, "owned dim is non-zero");
        for &p in &d.params[1..] {
            assert_eq!(p, 0.0, "non-owned dims are exactly zero");
        }
    }

    #[test]
    fn free_rider_delta_is_all_zeros() {
        let f = SharedSearchContributor::free_rider(9);
        assert!(f.delta().params.iter().all(|&p| p == 0.0));
        // A zero-quality contributor that owns dims is also a free-rider.
        let z = SharedSearchContributor::new(9, vec![0, 1], 0.0);
        assert!(z.delta().params.iter().all(|&p| p == 0.0));
    }

    #[test]
    fn folding_all_contributors_recovers_a_strong_classifier() {
        // Four contributors each owning one true dimension; folding their deltas
        // (elementwise add onto the zero baseline) recovers a near-W_TRUE vector that
        // classifies held-out points well. This is the collaborative shared artifact.
        let surface = ConfigSurface;
        let scorer = LinearScorer::new();
        let mut shared = ConfigArtifact::baseline();
        for dim in 0..D {
            let c = SharedSearchContributor::new(dim as u64 + 1, vec![dim], 1.0);
            shared = surface.apply_delta(&shared, &c.delta()).unwrap();
        }
        let heldout = scorer.measure(&shared, Split::HeldOut).value;
        assert!(
            heldout > 0.85,
            "folding all productive contributors recovers a strong classifier, got {heldout}"
        );
    }

    #[test]
    fn a_productive_contributor_moves_heldout_a_freerider_does_not() {
        // Marginal effect: a productive delta improves held-out accuracy over a partial
        // shared artifact; a free-rider's zero delta leaves it unchanged. The fold order
        // [0, 1] then dim 3 yields a strictly positive marginal (measured ~+0.05),
        // unlike the {0,1}+2 ordering whose marginal is subsumed (an interaction the
        // collaborative runner handles by rejecting non-improving deltas).
        let surface = ConfigSurface;
        let scorer = LinearScorer::new();
        let mut shared = ConfigArtifact::baseline();
        for dim in [0usize, 1] {
            let c = SharedSearchContributor::new(dim as u64 + 1, vec![dim], 1.0);
            shared = surface.apply_delta(&shared, &c.delta()).unwrap();
        }
        let before = scorer.measure(&shared, Split::HeldOut).value;

        // A productive contributor owning dim 3 raises held-out accuracy.
        let prod = SharedSearchContributor::new(4, vec![3], 1.0);
        let after_prod = scorer
            .measure(
                &surface.apply_delta(&shared, &prod.delta()).unwrap(),
                Split::HeldOut,
            )
            .value;
        assert!(
            after_prod > before,
            "a productive delta must raise held-out: {before} -> {after_prod}"
        );

        // A free-rider leaves held-out exactly unchanged (zero delta).
        let free = SharedSearchContributor::free_rider(9);
        let after_free = scorer
            .measure(
                &surface.apply_delta(&shared, &free.delta()).unwrap(),
                Split::HeldOut,
            )
            .value;
        assert_eq!(
            after_free, before,
            "a free-rider's zero delta must not move held-out at all"
        );
    }

    #[test]
    fn wilson_interval_brackets_p_and_stays_in_unit() {
        let (lo, hi) = wilson_interval(0.8, 80);
        assert!(lo >= 0.0 && hi <= 1.0);
        assert!(lo < 0.8 && 0.8 < hi);
        // Degenerate n=0 widens to the full unit interval.
        assert_eq!(wilson_interval(0.5, 0), (0.0, 1.0));
    }
}
