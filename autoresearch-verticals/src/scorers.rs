//! The three non-`HeldOutEval` scorer kinds and the black-box optimizer engine —
//! the M5 instantiations of [`autoresearch_runtime::ScorerKind`] beyond the
//! Improvement-Plane replay scorer of [`crate::config_opt`].
//!
//! The centerpiece is **Scenario A — the private-oracle (quantum) case**
//! ([`PrivateOracleScorer`] + [`BlackBoxOptimizerEngine`]): researchers are scored
//! against a HIDDEN reference they never see and cannot reproduce, and improve only
//! through bounded scalar queries (solve-hard / verify-easy). This is the
//! "open network beat the withheld benchmark, +39.9%" pattern — the EigenCloud
//! withheld-quantum-benchmark shape — proven end-to-end in
//! `tests/e2e_private_oracle.rs`.
//!
//! Two of the three scorers are **honest local stand-ins**, marked as such:
//!
//! - [`PrivilegedHardwareScorer`] stands in for an expensive/privileged evaluation
//!   (a real quantum device or a proprietary simulator). It computes a deterministic
//!   local score but reports a high cost and a `privileged-hardware` id. It does NOT
//!   talk to real hardware; the integration seam is documented on the type.
//! - [`HumanPanelScorer`] aggregates PRE-RECORDED deterministic panel verdicts into a
//!   [`Measurement`] (mean + CI across panelists). It does NOT run a live async human
//!   panel; the real integration is external/async and is documented on the type.
//!
//! Everything here is fully deterministic — a seeded linear-congruential generator
//! (LCG), no `rand`, no clock, no I/O — so the Scenario A climb and every payout are
//! reproducible (the same property the M1 vertical relies on).

use std::future::Future;

use autoresearch_runtime::privacy::SubmissionBudget;
use autoresearch_runtime::traits::{
    Engine, EngineContext, EngineError, Scorer, ScorerError, Surface, SurfaceError,
};
use autoresearch_runtime::types::{ArtifactRef, Measurement, ScorerKind, Split};

use crate::config_opt::ConfigArtifact;

/// Dimensionality of the hidden-target task. Matches the config-opt surface width so
/// the same [`ConfigArtifact`] flows through both verticals unchanged.
const D: usize = 4;
/// z for a two-sided 95% normal interval (the closeness score is a bounded mean, not
/// a binomial proportion, so a normal CI is the right shape here).
const Z_95: f64 = 1.96;

// --- Deterministic PRNG -----------------------------------------------------

/// A 64-bit linear-congruential generator (the Knuth MMIX constants), self-contained
/// so this module owns its determinism exactly as [`crate::config_opt`] does. Used to
/// synthesize the hidden reference and to drive the black-box search; no external RNG,
/// no clock.
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

    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }

    /// A uniform `f64` in `[0, 1)` from the high 53 bits (LCG low bits are weak).
    fn next_unit(&mut self) -> f64 {
        let bits = self.next_u64() >> 11;
        (bits as f64) / ((1u64 << 53) as f64)
    }

    /// A uniform `f64` in `[-1, 1)`.
    fn next_signed(&mut self) -> f64 {
        2.0 * self.next_unit() - 1.0
    }
}

// ---------------------------------------------------------------------------
// Hidden-target surface + artifact (Scenario A).
// ---------------------------------------------------------------------------

/// A fixed-length, finite real vector — the artifact a researcher submits to the
/// private oracle. This is a re-use of [`ConfigArtifact`]: the surface and the oracle
/// operate on the same `D`-dimensional vector, so the existing config-opt surface
/// could equally serve. We keep a dedicated surface so the id (`hidden-target`) is
/// honest about what is being optimized and so the bounds check matches the oracle.
#[derive(Clone, Debug, Default)]
pub struct HiddenTargetSurface;

impl HiddenTargetSurface {
    /// The starting point a researcher's black-box search departs from: the origin,
    /// which is maximally far (in closeness terms) from a generic hidden target. This
    /// is the analogue of the all-zeros config-opt baseline.
    #[must_use]
    pub fn origin() -> ConfigArtifact {
        ConfigArtifact {
            params: vec![0.0; D],
        }
    }
}

impl Surface for HiddenTargetSurface {
    type Artifact = ConfigArtifact;

    fn id(&self) -> &str {
        "hidden-target"
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
        // Stable content reference over the bit pattern (FNV-1a). The ref is of the
        // SUBMITTED artifact only — it reveals nothing about the hidden target.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for p in &artifact.params {
            for byte in p.to_bits().to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
            }
        }
        Ok(ArtifactRef(format!("hidden-target:{hash:016x}")))
    }
}

// ---------------------------------------------------------------------------
// Private-oracle scorer (Scenario A — the quantum case).
// ---------------------------------------------------------------------------

/// A scorer holding a SECRET reference vector the researcher never sees and cannot
/// reproduce. Its only interface is [`Scorer::score`], returning a scalar closeness
/// score with a CI — the solve-hard / verify-easy oracle of the quantum case.
///
/// # Privacy invariant (the load-bearing property)
///
/// The hidden reference cannot be recovered by a researcher from the scores. This rests
/// on two distinct properties — a *structural* one and an *analytic* one — and BOTH are
/// required (the structural property alone is not sufficient, see "Why the score must
/// be perturbed" below):
///
/// - **Structural** — the secret is unreachable by any accessor or encoding: it is a
///   private field with **no public getter**; the type derives **neither `Debug`,
///   `Display`, nor `Serialize`/`Deserialize`**; and there is **no dev split that
///   reveals it** (scoring on [`Split::Dev`] and [`Split::HeldOut`] runs against the
///   SAME hidden reference). The only thing a researcher obtains is a scalar score on
///   an artifact they themselves submitted.
/// - **Analytic** — the returned scalar is a **non-invertible** function of the secret:
///   per-query deterministic perturbation (see below) means no fixed-size set of
///   queries yields a solvable system of equations for the secret.
///
/// Two oracles built from different secret seeds are therefore **indistinguishable to
/// a researcher except through the scores they return** — there is no accessor or
/// serialization that could tell them apart, only the scalar outputs (asserted in the
/// unit tests).
///
/// # Why the score must be perturbed (the analytic leak this closes)
///
/// A *noise-free* closeness `exp(-||x - secret||^2 / (2D))` is algebraically invertible:
/// `||x - secret||^2 = -2D * ln(value)` recovers the exact squared distance, and `D + 1`
/// queries (the origin plus each axis unit vector) solve for `secret` in closed form to
/// full f64 precision — independent of the query budget, since `D + 1` is tiny. A bare
/// scalar closeness is therefore NOT a private oracle; it is a thin encoding of the
/// secret. To make the channel genuinely solve-hard, every reported value carries a
/// **deterministic, artifact-seeded multiplicative perturbation** of the effective
/// squared distance (see [`PrivateOracleScorer::perturbed_closeness`]). Each query then
/// answers a *different, unknown* equation, so the linear system never closes: empirically
/// the `D + 1`-query closed-form solve recovers the secret with per-component error on the
/// order of the secret's own scale (a near-total loss), not `~1e-16`. The perturbation is
/// a pure function of the submitted artifact's bit pattern, so the oracle stays fully
/// deterministic and reproducible — re-querying the same artifact returns the same value
/// (no averaging-away), and different artifacts get independent, unmodellable offsets.
///
/// # The score channel is bounded, not closed (PRIVACY §8)
///
/// Each query still leaks a bounded amount of signal about the hidden target (the
/// perturbed value is monotone *in expectation* in proximity). An optional
/// [`SubmissionBudget`] caps the NUMBER of probing queries, which **bounds** that
/// residual leakage — it does not make it zero. The budget is the rate-limit that
/// prevents unbounded probing (the same hook a real deployment slashes on) and is
/// enforced inside [`PrivateOracleScorer::query`] (the researcher-probing path). The
/// referee's own certification path ([`PrivateOracleScorer::score`] /
/// [`PrivateOracleScorer::certify`]) does NOT draw from this budget — certification is
/// the referee measuring, not the researcher probing, so the two are decoupled.
///
/// # Closeness score
///
/// The base score is `exp(-||artifact - secret||^2 / (2 * D))`, a bounded closeness in
/// `(0, 1]` that is `1.0` exactly at the hidden target and decays with distance; the
/// reported value is this base perturbed per-artifact as above. It is monotone in
/// proximity to the secret in expectation, so a black-box optimizer that climbs the
/// score climbs toward the hidden optimum — without ever seeing it.
pub struct PrivateOracleScorer {
    /// The SECRET reference. Private, never exposed: no getter, no Debug/Serialize.
    secret: [f64; D],
    /// Relative amplitude of the per-artifact perturbation applied to the effective
    /// squared distance before the exponential. Non-zero makes the score channel
    /// non-invertible (see the type doc); the default ([`PrivateOracleScorer::new`])
    /// is [`PrivateOracleScorer::DEFAULT_NOISE_AMP`].
    noise_amp: f64,
    /// Optional query rate-limit on the RESEARCHER-PROBING path (the score-as-channel
    /// bound, PRIVACY §8). Interior mutability via `Cell` so [`PrivateOracleScorer::query`]
    /// can account a consumed submission behind the shared `&self` the `Scorer` trait
    /// hands out, while staying single-threaded-deterministic (no atomics, no clock).
    /// The referee certification path ([`PrivateOracleScorer::certify`]) never touches
    /// this, so per-researcher probing and referee measurement do not share a counter.
    budget: std::cell::Cell<Option<SubmissionBudget>>,
}

impl PrivateOracleScorer {
    /// Build an oracle whose hidden reference is synthesized deterministically from
    /// `secret_seed`. The same seed yields the same hidden target; **the seed is the
    /// only thing that determines the secret, and it is consumed here and dropped** —
    /// it is not stored, so even reflection over the struct cannot recover it from a
    /// constructed oracle (only the derived vector lives on, behind no accessor).
    ///
    /// `budget` optionally rate-limits the RESEARCHER-PROBING path (PRIVACY §8); `None`
    /// is unbounded probing. The score channel is perturbed at
    /// [`PrivateOracleScorer::DEFAULT_NOISE_AMP`] so it is non-invertible (the secret
    /// cannot be solved for in closed form — see the type doc).
    #[must_use]
    pub fn new(secret_seed: u64, budget: Option<SubmissionBudget>) -> Self {
        Self::with_noise_amp(secret_seed, budget, Self::DEFAULT_NOISE_AMP)
    }

    /// Default relative amplitude of the per-artifact perturbation. Chosen large enough
    /// that the `D + 1`-query closed-form inversion loses the secret (per-component
    /// recovery error on the order of the secret's own scale), yet small enough that a
    /// bounded black-box search still climbs to high true closeness (the perturbation is
    /// zero-mean in the effective-distance factor, so the *expected* signal stays
    /// monotone). Both properties are asserted in the unit tests.
    pub const DEFAULT_NOISE_AMP: f64 = 0.35;

    /// Build an oracle with an explicit perturbation amplitude. `noise_amp == 0.0`
    /// reproduces the legacy noise-free (and INVERTIBLE) closeness — provided only for
    /// the regression test that demonstrates the leak the default amplitude closes; it
    /// must not be used for a real private oracle.
    #[must_use]
    pub fn with_noise_amp(
        secret_seed: u64,
        budget: Option<SubmissionBudget>,
        noise_amp: f64,
    ) -> Self {
        let mut rng = Lcg::new(secret_seed);
        let mut secret = [0.0; D];
        for slot in &mut secret {
            // Hidden target components in [-2, 2): a generic, non-trivial point that a
            // black-box search must actually find — never disclosed.
            *slot = 2.0 * rng.next_signed();
        }
        Self {
            secret,
            noise_amp,
            budget: std::cell::Cell::new(budget),
        }
    }

    /// A deterministic, artifact-seeded multiplier on the effective squared distance, in
    /// roughly `[1 - noise_amp, 1 + noise_amp]` (clamped strictly positive). Derived from
    /// the artifact's exact bit pattern via the same FNV-1a mix the surface ref uses, so
    /// it is a pure function of the submitted artifact — reproducible, and independent of
    /// the secret. This is the term that makes the score channel non-invertible: every
    /// query carries its own unknown offset, so no fixed set of queries solves for the
    /// secret (the secret does not appear in this factor at all).
    fn distance_perturbation(&self, params: &[f64]) -> f64 {
        if self.noise_amp == 0.0 {
            return 1.0;
        }
        // FNV-1a over the artifact bit pattern -> a per-artifact seed.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for p in params {
            for byte in p.to_bits().to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
            }
        }
        let signed = Lcg::new(hash).next_signed(); // in [-1, 1)
        (1.0 + self.noise_amp * signed).max(0.05)
    }

    /// The perturbed closeness of `artifact` to the hidden reference, in `(0, 1]`. Pure,
    /// deterministic function of the submitted artifact and the secret; the secret never
    /// leaves this method except folded into the scalar. The per-artifact perturbation
    /// (see [`PrivateOracleScorer::distance_perturbation`]) is what makes this scalar a
    /// non-invertible encoding of the secret — see the type doc.
    fn perturbed_closeness(&self, params: &[f64]) -> f64 {
        let mut sq = 0.0;
        for i in 0..D {
            let p = params.get(i).copied().unwrap_or(0.0);
            let d = p - self.secret[i];
            sq += d * d;
        }
        let factor = self.distance_perturbation(params);
        (-(sq * factor) / (2.0 * D as f64)).exp()
    }

    /// Build a [`Measurement`] from a finite point estimate `value` with the oracle's
    /// fixed-`n` normal CI, ordered `ci_lower <= value <= ci_upper` by construction.
    fn measurement_from(value: f64, n: u32) -> Measurement {
        // A normal CI on a bounded mean closeness. Half-width ~ 1/sqrt(n) so a higher n
        // is a tighter, more-powered measurement. `value` is guaranteed finite here, so
        // the clamps below keep the interval ordered.
        let se = 0.5 / f64::from(n.max(1)).sqrt();
        let half = Z_95 * se;
        Measurement {
            value,
            ci_lower: (value - half).max(0.0).min(value),
            ci_upper: (value + half).min(1.0).max(value),
            n,
            // The oracle is cheap locally; report a nominal per-query cost so the cost
            // ledger is populated. (Contrast the privileged-hardware scorer's high cost.)
            cost: f64::from(n),
        }
    }

    /// The referee CERTIFICATION path: score `artifact` WITHOUT consuming the
    /// researcher-probing budget (the referee is measuring, not probing — PRIVACY §8).
    /// Rejects non-finite params so the scorer is self-defending regardless of caller
    /// (the runner pre-validates, but the scorer is also `pub` API).
    ///
    /// # Errors
    /// [`ScorerError::Rejected`] if any param is non-finite.
    fn certify(&self, artifact: &ConfigArtifact, n: u32) -> Result<Measurement, ScorerError> {
        if artifact.params.iter().any(|p| !p.is_finite()) {
            return Err(ScorerError::Rejected(
                "non-finite params cannot be scored".into(),
            ));
        }
        Ok(Self::measurement_from(
            self.perturbed_closeness(&artifact.params),
            n,
        ))
    }

    /// Number of queries still permitted, or `None` if the oracle is unbounded.
    #[must_use]
    pub fn remaining_queries(&self) -> Option<u32> {
        let b = self.budget.get();
        let r = b.map(|b| b.remaining());
        self.budget.set(b);
        r
    }

    /// The researcher-PROBING path: score `artifact`, consuming one unit of the query
    /// budget (if any). Returns [`ScorerError::Rejected`] once the budget is exhausted —
    /// the rate-limit that bounds the score-channel leak and cuts off unbounded probing.
    /// Non-finite params are rejected BEFORE the budget is touched, so a malformed probe
    /// never silently burns budget or emits an invalid measurement.
    ///
    /// The CI half-width shrinks with the configured sample count `n`, giving a
    /// well-powered measurement a black-box engine's lift can clear the gate on.
    fn query(&self, artifact: &ConfigArtifact, n: u32) -> Result<Measurement, ScorerError> {
        // Reject non-finite params before consuming budget or emitting a measurement:
        // a NaN/inf artifact must not burn a probe or produce a malformed CI.
        if artifact.params.iter().any(|p| !p.is_finite()) {
            return Err(ScorerError::Rejected(
                "non-finite params cannot be scored".into(),
            ));
        }
        // Enforce the query budget (the score-as-channel rate-limit). A `None` budget
        // is unbounded.
        if let Some(mut b) = self.budget.get() {
            if !b.try_consume() {
                self.budget.set(Some(b));
                return Err(ScorerError::Rejected(format!(
                    "submission budget exhausted ({} queries used); the score channel is rate-limited (PRIVACY §8)",
                    b.used
                )));
            }
            self.budget.set(Some(b));
        }

        Ok(Self::measurement_from(
            self.perturbed_closeness(&artifact.params),
            n,
        ))
    }
}

/// Sample count the oracle reports per score. Large enough that a real closeness gap
/// clears the default gate's power requirement (`min_n = 12`).
const ORACLE_N: u32 = 96;

impl Scorer for PrivateOracleScorer {
    type Artifact = ConfigArtifact;

    fn id(&self) -> &str {
        "private-oracle"
    }

    fn score(
        &self,
        artifact: &Self::Artifact,
        // The hidden reference is identical across splits: there is NO dev split that
        // reveals more about the secret than the held-out split. A researcher learns
        // only a scalar score on an artifact they submitted, on either split.
        _split: Split,
    ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send {
        // `score` is the REFEREE certification path (the runner measures baselines and
        // candidates through it). It must NOT consume the researcher-probing budget —
        // certification is the referee measuring, not the researcher probing (PRIVACY
        // §8). The budget is consumed only by `query` (the engine's probing path).
        let result = self.certify(artifact, ORACLE_N);
        std::future::ready(result)
    }
}

// ---------------------------------------------------------------------------
// Black-box optimizer engine (Scenario A — improve via scalar queries only).
// ---------------------------------------------------------------------------

/// An [`Engine`] that improves a candidate using ONLY scalar oracle feedback — no
/// surface gradient, no dev exemplars, no sight of the hidden reference. It runs a
/// seeded (1+1) evolutionary search bounded by a query budget: from the current best,
/// it proposes a mutated candidate, queries the oracle for its scalar score, and keeps
/// the candidate iff the score improved. Across queries it genuinely climbs toward the
/// hidden optimum (real improvement, not mocked) and STOPS at the query budget.
///
/// This is the researcher's product in the quantum case: solve-hard (find a point
/// close to the secret) by repeatedly verify-easy (one scalar query at a time).
///
/// Determinism: the mutation stream is a seeded [`Lcg`]; the same `(seed, budget)`
/// yields the same trajectory and the same final candidate.
pub struct BlackBoxOptimizerEngine<'a> {
    seed: u64,
    /// Max oracle queries this engine may spend. The engine never exceeds it; if the
    /// oracle's own [`SubmissionBudget`] is tighter, the engine stops early when the
    /// oracle refuses (it does not error out — it returns the best found so far).
    query_budget: u32,
    /// The oracle the engine queries. Borrowed: the engine never owns or inspects the
    /// secret, it only calls [`Scorer::score`].
    oracle: &'a PrivateOracleScorer,
}

impl<'a> BlackBoxOptimizerEngine<'a> {
    #[must_use]
    pub fn new(seed: u64, query_budget: u32, oracle: &'a PrivateOracleScorer) -> Self {
        Self {
            seed,
            query_budget,
            oracle,
        }
    }

    /// Run the bounded black-box search and return the best candidate found.
    ///
    /// Factored out of `produce` so it is trivially deterministic and unit-testable.
    /// Uses [`Split::Dev`] for its probing queries (a researcher's own optimization
    /// loop) — which, for the private oracle, returns the same scalar as `HeldOut`,
    /// because the oracle has no revealing dev split.
    fn search(&self) -> ConfigArtifact {
        let mut rng = Lcg::new(self.seed);
        let mut best = HiddenTargetSurface::origin();
        // Score the starting point. If the very first query is already refused by the
        // oracle's own tighter budget, return the origin (no improvement possible).
        let mut best_score = match self.oracle.query(&best, ORACLE_N) {
            Ok(m) => m.value,
            Err(_) => return best,
        };
        // One query already spent on the origin.
        let mut spent: u32 = 1;
        // Annealing step: start broad and narrow as the search homes in, so the (1+1)
        // walk both finds the basin and refines inside it within budget.
        while spent < self.query_budget {
            let frac = f64::from(spent) / f64::from(self.query_budget.max(1));
            let step = 2.0 * (1.0 - 0.9 * frac); // shrinks from 2.0 toward 0.2
            let candidate = ConfigArtifact {
                params: best
                    .params
                    .iter()
                    .map(|p| p + step * rng.next_signed())
                    .collect(),
            };
            let score = match self.oracle.query(&candidate, ORACLE_N) {
                Ok(m) => m.value,
                // The oracle's own budget cut us off: stop and return the best so far.
                Err(_) => break,
            };
            spent += 1;
            if score > best_score {
                best_score = score;
                best = candidate;
            }
        }
        best
    }
}

impl Engine for BlackBoxOptimizerEngine<'_> {
    type Artifact = ConfigArtifact;

    fn id(&self) -> &str {
        "black-box-optimizer"
    }

    fn produce(
        &self,
        _ctx: &EngineContext,
    ) -> impl Future<Output = Result<Self::Artifact, EngineError>> + Send {
        let artifact = self.search();
        std::future::ready(Ok(artifact))
    }
}

// ---------------------------------------------------------------------------
// Privileged-hardware scorer (STAND-IN).
// ---------------------------------------------------------------------------

/// A [`Scorer`] wrapping an "expensive / privileged" evaluation.
///
/// **STAND-IN — not real hardware.** This computes a deterministic LOCAL score (the
/// same hidden-target closeness used by the oracle, with a fixed reference) but reports
/// a HIGH cost and a `privileged-hardware` id to model an evaluation that runs on a
/// scarce, expensive resource. It does **not** talk to a quantum device, a proprietary
/// simulator, or any external accelerator; nothing here proves real privileged compute.
///
/// # Integration seam
///
/// The production swap replaces [`Scorer::score`] with a call out to the privileged
/// backend (a queued quantum-device job, a licensed simulator endpoint), preserving the
/// same `Measurement` shape — `value` becomes the device's measured figure of merit,
/// `cost` becomes the real device-seconds / dollar cost, and `n` becomes the shot
/// count. The trait seam is identical, so the runner and gate are unchanged.
pub struct PrivilegedHardwareScorer {
    reference: [f64; D],
    /// Reported per-evaluation cost — high, to model a scarce privileged resource.
    cost_per_eval: f64,
}

impl PrivilegedHardwareScorer {
    /// Build the stand-in with a deterministic reference and a high reported cost.
    #[must_use]
    pub fn new(seed: u64, cost_per_eval: f64) -> Self {
        let mut rng = Lcg::new(seed);
        let mut reference = [0.0; D];
        for slot in &mut reference {
            *slot = rng.next_signed();
        }
        Self {
            reference,
            cost_per_eval,
        }
    }

    /// Measure `artifact`, failing closed on non-finite params so the scorer never emits
    /// an invalid [`Measurement`] (e.g. `value = NaN` with a `[0, 1]` CI, whose ordering
    /// invariant would not hold). The runner pre-validates, but this scorer is `pub` API
    /// and so self-defends regardless of caller.
    ///
    /// # Errors
    /// [`ScorerError::Rejected`] if any param is non-finite.
    fn measure(&self, artifact: &ConfigArtifact) -> Result<Measurement, ScorerError> {
        if artifact.params.iter().any(|p| !p.is_finite()) {
            return Err(ScorerError::Rejected(
                "non-finite params cannot be measured".into(),
            ));
        }
        let mut sq = 0.0;
        for i in 0..D {
            let p = artifact.params.get(i).copied().unwrap_or(0.0);
            let d = p - self.reference[i];
            sq += d * d;
        }
        let value = (-sq / (2.0 * D as f64)).exp();
        let n = 64u32;
        let se = 0.5 / f64::from(n).sqrt();
        let half = Z_95 * se;
        // `value` is finite here, so the clamps keep the interval ordered.
        Ok(Measurement {
            value,
            ci_lower: (value - half).max(0.0).min(value),
            ci_upper: (value + half).min(1.0).max(value),
            n,
            // The signature of a privileged scorer: a HIGH reported cost.
            cost: self.cost_per_eval * f64::from(n),
        })
    }
}

impl Scorer for PrivilegedHardwareScorer {
    type Artifact = ConfigArtifact;

    fn id(&self) -> &str {
        "privileged-hardware"
    }

    fn score(
        &self,
        artifact: &Self::Artifact,
        _split: Split,
    ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send {
        std::future::ready(self.measure(artifact))
    }
}

// ---------------------------------------------------------------------------
// Human-panel scorer (STAND-IN).
// ---------------------------------------------------------------------------

/// A [`Scorer`] that aggregates PRE-RECORDED deterministic panel verdicts into a
/// [`Measurement`] — the mean verdict across panelists with a normal CI on that mean.
///
/// **STAND-IN — not a live human panel.** The real integration is an EXTERNAL, ASYNC
/// process: a panel of human judges scores each artifact out of band and the verdicts
/// arrive later. This stand-in models that with a fixed table of per-artifact verdicts
/// so the trait seam, the aggregation, and the CI are exercised deterministically. It
/// does **not** contact real judges; nothing here proves a real human evaluation.
///
/// # Integration seam
///
/// In production this type is fed by an async verdict store: `score` awaits the
/// panel's recorded verdicts for the artifact (or returns
/// [`ScorerError::Unavailable`] until the panel reports), then aggregates them with
/// the SAME mean + CI logic. The aggregation is the durable part; only the verdict
/// source changes.
pub struct HumanPanelScorer {
    /// `(artifact_ref_substr, verdicts)` — for each known artifact, the panelists'
    /// recorded scores in `[0, 1]`. Looked up by the artifact's surface ref.
    verdicts_by_ref: Vec<(String, Vec<f64>)>,
    /// Fallback verdicts for an artifact with no recorded panel (e.g. a baseline):
    /// a neutral panel so the scorer always yields a valid measurement.
    default_verdicts: Vec<f64>,
}

impl HumanPanelScorer {
    /// Neutral verdict used to repair a panel that, after sanitizing, has no usable
    /// recorded score (every entry was non-finite or out of range). A panel must have at
    /// least one real verdict to yield an honest measurement.
    const NEUTRAL_VERDICT: f64 = 0.5;

    /// Build the stand-in from a table of pre-recorded verdicts keyed by a substring of
    /// the artifact's surface ref, plus a default panel for unknown artifacts.
    ///
    /// Verdicts are enforced to the type's own contract — recorded scores in `[0, 1]`:
    /// every verdict is clamped into `[0, 1]`, non-finite verdicts are dropped, and any
    /// panel left empty (including an empty `default_verdicts`) is replaced by a single
    /// neutral verdict. This guarantees [`HumanPanelScorer::aggregate`] only ever sees a
    /// non-empty, finite, in-range panel, so it cannot emit an out-of-order or non-finite
    /// [`Measurement`], and an empty default can never masquerade as a 1-panelist panel.
    #[must_use]
    pub fn new(verdicts_by_ref: Vec<(String, Vec<f64>)>, default_verdicts: Vec<f64>) -> Self {
        Self {
            verdicts_by_ref: verdicts_by_ref
                .into_iter()
                .map(|(k, v)| (k, Self::sanitize_panel(v)))
                .collect(),
            default_verdicts: Self::sanitize_panel(default_verdicts),
        }
    }

    /// Coerce a raw verdict vector to the `[0, 1]`, finite, non-empty contract: drop
    /// non-finite entries, clamp the rest into `[0, 1]`, and fall back to a single
    /// neutral verdict if nothing survives (a panel must have >= 1 real verdict).
    fn sanitize_panel(verdicts: Vec<f64>) -> Vec<f64> {
        let cleaned: Vec<f64> = verdicts
            .into_iter()
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(0.0, 1.0))
            .collect();
        if cleaned.is_empty() {
            vec![Self::NEUTRAL_VERDICT]
        } else {
            cleaned
        }
    }

    /// Aggregate a panel's verdicts into a [`Measurement`]: mean across panelists with a
    /// normal 95% CI on the mean (`mean +/- Z_95 * s/sqrt(n)`), clamped to `[0, 1]`. `n`
    /// is the panelist count.
    ///
    /// Defensive even though [`HumanPanelScorer::new`] sanitizes its inputs: an empty
    /// slice is treated as a single neutral verdict, every verdict is clamped into
    /// `[0, 1]`, and the point estimate is forced inside `[ci_lower, ci_upper]` — so the
    /// `ci_lower <= value <= ci_upper` ordering invariant holds for ANY input, including
    /// a slice passed directly to this associated fn.
    fn aggregate(verdicts: &[f64]) -> Measurement {
        // Clamp into [0, 1] and drop non-finite, then default to a neutral panel if
        // nothing usable remains, so `mean`/`var` are always finite and in range.
        let clean: Vec<f64> = verdicts
            .iter()
            .copied()
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(0.0, 1.0))
            .collect();
        let clean: &[f64] = if clean.is_empty() {
            &[Self::NEUTRAL_VERDICT]
        } else {
            &clean
        };
        let n = clean.len() as u32;
        let mean = clean.iter().sum::<f64>() / f64::from(n);
        // Sample standard deviation (n-1); a single panelist has no spread -> use a
        // conservative wide CI so a 1-judge panel never looks artificially certain.
        let var = if clean.len() > 1 {
            let m = mean;
            clean.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (f64::from(n) - 1.0)
        } else {
            // One verdict: no measurable spread. Assume the full-range pessimistic
            // variance so the CI is honestly wide.
            0.25
        };
        let se = (var / f64::from(n)).sqrt();
        let half = Z_95 * se;
        // Clamp the point estimate into [0, 1] first, then force the interval to bracket
        // it, so the ordering invariant `ci_lower <= value <= ci_upper` always holds.
        let value = mean.clamp(0.0, 1.0);
        Measurement {
            value,
            ci_lower: (value - half).max(0.0).min(value),
            ci_upper: (value + half).min(1.0).max(value),
            n,
            // Human time is the cost unit here: one unit per panelist verdict.
            cost: f64::from(n),
        }
    }

    /// Resolve the recorded verdicts for an artifact by its surface ref, falling back
    /// to the default panel when none is recorded.
    fn verdicts_for(&self, artifact: &ConfigArtifact) -> Vec<f64> {
        let surface = HiddenTargetSurface;
        let key = match surface.to_ref(artifact) {
            Ok(r) => r.0,
            Err(_) => return self.default_verdicts.clone(),
        };
        for (substr, v) in &self.verdicts_by_ref {
            if key.contains(substr) {
                return v.clone();
            }
        }
        self.default_verdicts.clone()
    }
}

impl Scorer for HumanPanelScorer {
    type Artifact = ConfigArtifact;

    fn id(&self) -> &str {
        "human-panel"
    }

    fn score(
        &self,
        artifact: &Self::Artifact,
        _split: Split,
    ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send {
        let verdicts = self.verdicts_for(artifact);
        std::future::ready(Ok(Self::aggregate(&verdicts)))
    }
}

// ---------------------------------------------------------------------------
// ScorerKind dispatch.
// ---------------------------------------------------------------------------

/// A thin, owned wrapper over the four [`ScorerKind`]s so a competition can declare
/// its kind and have the runner use the matching scorer through ONE [`Scorer`] type.
///
/// This is the dispatch the M5 milestone asks for: a competition's
/// [`autoresearch_runtime::Knobs::scorer_kind`] selects which arm scores, and the
/// generic runners ([`autoresearch_protocol::run_oneshot_competitive`] et al.) consume
/// this enum like any other `Scorer` — no runner change. Every arm reports its
/// own scorer id, so [`KindDispatchScorer::kind`] is always consistent with the id and
/// with the declared `ScorerKind` ([`KindDispatchScorer::kind`] is asserted equal to
/// the constructing kind in the unit tests).
///
/// The [`ScorerKind::HeldOutEval`] arm reuses [`crate::config_opt::LinearScorer`] (the
/// M1 Improvement-Plane stand-in), so all four kinds are reachable through one type.
pub enum KindDispatchScorer {
    /// Held-out replay eval (the M1 Improvement-Plane stand-in).
    HeldOutEval(crate::config_opt::LinearScorer),
    /// The hidden-reference private oracle (Scenario A — the quantum case).
    PrivateOracle(PrivateOracleScorer),
    /// Privileged / expensive hardware (STAND-IN — see [`PrivilegedHardwareScorer`]).
    PrivilegedHardware(PrivilegedHardwareScorer),
    /// Human panel (STAND-IN — see [`HumanPanelScorer`]).
    HumanPanel(HumanPanelScorer),
}

/// Resolve a future that is guaranteed to complete on its first poll (every scorer's
/// `score` here is a `std::future::ready(...)` over pure CPU work). The dispatcher
/// needs the arms' results synchronously to wrap them in ONE `ready` future; the
/// `HeldOutEval` arm ([`crate::config_opt::LinearScorer`]) returns such a future, so
/// a single poll with a no-op waker yields its value without an executor.
fn block_on_ready<F: Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, Waker};

    // The no-op waker is sufficient: the dispatched scorer futures all complete on the
    // first poll (`std::future::ready`), so the waker is never used to reschedule.
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut fut = Box::pin(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        // Unreachable for the `ready(...)` futures the scorers return; if a future ever
        // pends here it is a programming error in a scorer's `score` contract.
        Poll::Pending => unreachable!("dispatched scorer future must be immediately ready"),
    }
}

impl KindDispatchScorer {
    /// The [`ScorerKind`] this dispatcher adjudicates with — always consistent with
    /// the arm and the on-chain `scorer_kind` declared at creation.
    #[must_use]
    pub fn kind(&self) -> ScorerKind {
        match self {
            KindDispatchScorer::HeldOutEval(_) => ScorerKind::HeldOutEval,
            KindDispatchScorer::PrivateOracle(_) => ScorerKind::PrivateOracle,
            KindDispatchScorer::PrivilegedHardware(_) => ScorerKind::PrivilegedHardware,
            KindDispatchScorer::HumanPanel(_) => ScorerKind::HumanPanel,
        }
    }
}

impl Scorer for KindDispatchScorer {
    type Artifact = ConfigArtifact;

    fn id(&self) -> &str {
        match self {
            KindDispatchScorer::HeldOutEval(s) => s.id(),
            KindDispatchScorer::PrivateOracle(s) => s.id(),
            KindDispatchScorer::PrivilegedHardware(s) => s.id(),
            KindDispatchScorer::HumanPanel(s) => s.id(),
        }
    }

    fn score(
        &self,
        artifact: &Self::Artifact,
        split: Split,
    ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send {
        // Every arm's scoring is pure CPU and resolves immediately. Compute the result
        // synchronously here (each arm's `score` future is itself a `ready(...)`), then
        // wrap it once in a single `ready` future. This keeps the dispatcher's return a
        // single concrete type WITHOUT capturing `&self` in the returned future, which
        // the trait's `impl Future + Send` (no borrow) contract requires.
        let result = match self {
            KindDispatchScorer::HeldOutEval(s) => block_on_ready(s.score(artifact, split)),
            // The dispatcher is used as a `Scorer` by the runner — i.e. the referee
            // CERTIFICATION path — so it certifies (unbudgeted), exactly like the direct
            // `PrivateOracleScorer::score` impl. The researcher-probing budget is spent
            // only through the engine's `query` path, never here.
            KindDispatchScorer::PrivateOracle(s) => s.certify(artifact, ORACLE_N),
            KindDispatchScorer::PrivilegedHardware(s) => s.measure(artifact),
            KindDispatchScorer::HumanPanel(s) => {
                Ok(HumanPanelScorer::aggregate(&s.verdicts_for(artifact)))
            }
        };
        std::future::ready(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autoresearch_runtime::types::Gate;

    fn art(params: [f64; D]) -> ConfigArtifact {
        ConfigArtifact {
            params: params.to_vec(),
        }
    }

    // --- oracle privacy invariant -----------------------------------------

    #[test]
    fn oracle_exposes_no_path_to_the_secret() {
        // The ONLY public surface of the oracle is `score` / `query` (scalars),
        // `remaining_queries` (a count), and `new`. There is no getter for `secret`,
        // and the type has no Debug/Display/Serialize impl, so the secret cannot be
        // printed or serialized. This test documents that by construction: if any of
        // those impls were added, an attempt to use them below would fail to compile.
        let oracle = PrivateOracleScorer::new(0xDEAD_BEEF, None);
        // A scalar score is the only thing a researcher can obtain.
        let m = oracle.query(&art([0.0; D]), ORACLE_N).unwrap();
        assert!(m.value > 0.0 && m.value <= 1.0, "closeness in (0, 1]");
        // remaining_queries is a count, not the secret.
        assert_eq!(oracle.remaining_queries(), None);
        // NOTE (compile-time invariant): `format!("{oracle:?}")` and
        // `serde_json::to_string(&oracle)` do NOT compile — the type derives neither
        // Debug nor Serialize — which is the structural proof the secret never leaks.
    }

    #[test]
    fn two_secrets_are_indistinguishable_except_through_scores() {
        // Two oracles with different secret seeds. A researcher cannot tell them apart
        // by any accessor or serialization (there are none) — only the scalar scores
        // they return on a SUBMITTED artifact differ.
        let a = PrivateOracleScorer::new(1, None);
        let b = PrivateOracleScorer::new(2, None);
        // For a generic probe the scores differ (the only observable difference).
        let probe = art([0.3, -0.7, 1.1, 0.2]);
        let sa = a.query(&probe, ORACLE_N).unwrap().value;
        let sb = b.query(&probe, ORACLE_N).unwrap().value;
        assert_ne!(
            sa, sb,
            "different hidden targets must yield different scalar scores on a probe"
        );
        // Same seed => identical hidden target => identical scores (determinism).
        let a2 = PrivateOracleScorer::new(1, None);
        assert_eq!(
            a.query(&probe, ORACLE_N).unwrap().value,
            a2.query(&probe, ORACLE_N).unwrap().value
        );
    }

    #[test]
    fn oracle_dev_and_heldout_splits_reveal_the_same_scalar() {
        // CRITICAL: there is no dev split that reveals more about the secret. Scoring
        // the same artifact on Dev and HeldOut returns the identical scalar.
        let oracle = PrivateOracleScorer::new(7, None);
        let probe = art([0.5, 0.5, -0.5, 0.1]);
        let dev = oracle.query(&probe, ORACLE_N).unwrap().value;
        // Re-query (a fresh oracle to avoid budget interplay; here budget is None).
        let heldout = oracle.query(&probe, ORACLE_N).unwrap().value;
        assert_eq!(dev, heldout, "dev split must not reveal more than held-out");
    }

    #[test]
    fn oracle_score_is_one_only_at_the_hidden_target_and_decays() {
        // Closeness peaks at 1.0 only when the artifact equals the secret, and is
        // strictly less elsewhere — the property a black-box climb exploits.
        let oracle = PrivateOracleScorer::new(99, None);
        // We don't know the secret, but a far point must score well below a near one.
        let far = oracle
            .query(&art([5.0, 5.0, 5.0, 5.0]), ORACLE_N)
            .unwrap()
            .value;
        let origin = oracle.query(&art([0.0; D]), ORACLE_N).unwrap().value;
        assert!(
            far < origin,
            "a far point must be less close than the origin"
        );
        assert!(far > 0.0, "closeness is strictly positive");
    }

    // --- query budget enforcement -----------------------------------------

    #[test]
    fn oracle_enforces_the_query_budget_and_cuts_off_probing() {
        let oracle = PrivateOracleScorer::new(3, Some(SubmissionBudget::new(2)));
        assert_eq!(oracle.remaining_queries(), Some(2));
        assert!(oracle.query(&art([0.0; D]), ORACLE_N).is_ok());
        assert!(oracle.query(&art([1.0; D]), ORACLE_N).is_ok());
        assert_eq!(oracle.remaining_queries(), Some(0));
        // The third query is REFUSED — unbounded probing is prevented.
        let err = oracle.query(&art([2.0; D]), ORACLE_N).unwrap_err();
        assert!(
            matches!(err, ScorerError::Rejected(_)),
            "over-budget query must be rejected, got {err:?}"
        );
    }

    #[tokio::test]
    async fn referee_certification_does_not_consume_the_probe_budget() {
        // The referee certification path (`score`) must NOT draw from the
        // researcher-probing budget: certification is the referee measuring, not the
        // researcher probing (PRIVACY §8). A tiny budget that would be exhausted by a
        // single probe is left fully intact by any number of `score` calls.
        let oracle = PrivateOracleScorer::new(3, Some(SubmissionBudget::new(1)));
        assert_eq!(oracle.remaining_queries(), Some(1));
        for _ in 0..10 {
            // Certification succeeds repeatedly without spending the probe budget.
            oracle
                .score(&art([0.2, 0.4, -0.1, 0.3]), Split::HeldOut)
                .await
                .unwrap();
        }
        assert_eq!(
            oracle.remaining_queries(),
            Some(1),
            "certification must not consume the researcher-probing budget"
        );
        // The probe path still consumes it: one probe drains the single unit.
        assert!(oracle.query(&art([0.0; D]), ORACLE_N).is_ok());
        assert_eq!(oracle.remaining_queries(), Some(0));
    }

    #[test]
    fn oracle_rejects_non_finite_params_on_both_paths() {
        // Neither the probe path (`query`) nor the certify path may emit a Measurement
        // for a non-finite artifact (value would be NaN with an unordered CI). Both fail
        // closed, and the probe rejection happens BEFORE any budget is consumed.
        let oracle = PrivateOracleScorer::new(5, Some(SubmissionBudget::new(3)));
        let bad = art([f64::NAN, 0.0, 0.0, 0.0]);
        assert!(matches!(
            oracle.query(&bad, ORACLE_N),
            Err(ScorerError::Rejected(_))
        ));
        assert_eq!(
            oracle.remaining_queries(),
            Some(3),
            "a non-finite probe must not burn budget"
        );
        assert!(matches!(
            oracle.certify(&bad, ORACLE_N),
            Err(ScorerError::Rejected(_))
        ));
    }

    #[test]
    fn oracle_score_channel_is_not_invertible_to_the_secret() {
        // THE load-bearing analytic property: the hidden secret is NOT recoverable from
        // the scores in closed form. We run the exact `D + 1`-query closed-form attack
        // (origin + each axis unit vector, invert `value = exp(-||x-secret||^2/2D)` and
        // solve `secret_i = (1 + A0 - A_i)/2`) against BOTH a noise-free oracle (which it
        // breaks) and the default perturbed oracle (which it must defeat).
        let secret_seed = 0xC0FF_EE15_900Du64;

        // Recover the secret synthesized from `secret_seed` exactly as `new` does, so we
        // can measure the attacker's reconstruction error against ground truth. (This is
        // test-only knowledge — the attacker below uses ONLY scalar scores.)
        let mut rng = Lcg::new(secret_seed);
        let mut truth = [0.0f64; D];
        for s in &mut truth {
            *s = 2.0 * rng.next_signed();
        }

        // The closed-form attack, using only scalar scores from `oracle`.
        let attack = |oracle: &PrivateOracleScorer| -> f64 {
            let inv = |v: f64| -2.0 * (D as f64) * v.ln(); // claimed ||x-secret||^2
            let a0 = inv(oracle.query(&art([0.0; D]), ORACLE_N).unwrap().value);
            let mut max_err = 0.0f64;
            for i in 0..D {
                let mut e = [0.0; D];
                e[i] = 1.0;
                let ai = inv(oracle.query(&art(e), ORACLE_N).unwrap().value);
                let rec_i = (1.0 + a0 - ai) / 2.0;
                max_err = max_err.max((rec_i - truth[i]).abs());
            }
            max_err
        };

        // Noise-free oracle: the attack recovers the secret to ~f64 precision (the leak
        // the perturbation closes). This documents the vulnerability concretely.
        let leaky = PrivateOracleScorer::with_noise_amp(secret_seed, None, 0.0);
        let leaky_err = attack(&leaky);
        assert!(
            leaky_err < 1e-9,
            "noise-free oracle IS invertible (documents the leak): err={leaky_err:e}"
        );

        // Default (perturbed) oracle: the same closed-form solve fails — per-component
        // reconstruction error is on the order of the secret's own scale, not precision.
        let secure = PrivateOracleScorer::new(secret_seed, None);
        let secure_err = attack(&secure);
        assert!(
            secure_err > 0.1,
            "perturbed oracle must defeat the closed-form solve: err={secure_err}"
        );
    }

    // --- black-box optimizer climbs and stops at budget -------------------

    #[test]
    fn black_box_optimizer_climbs_toward_the_hidden_optimum() {
        let oracle = PrivateOracleScorer::new(0xABCD, None);
        let origin_score = oracle
            .query(&HiddenTargetSurface::origin(), ORACLE_N)
            .unwrap()
            .value;
        let engine = BlackBoxOptimizerEngine::new(42, 300, &oracle);
        let best = engine.search();
        let best_score = oracle.query(&best, ORACLE_N).unwrap().value;
        assert!(
            best_score > origin_score,
            "black-box search must improve over the origin via scalar queries: {best_score} vs {origin_score}"
        );
        // A genuinely strong climb gets meaningfully close to the hidden optimum.
        assert!(
            best_score > 0.6,
            "bounded black-box search should climb high toward the hidden target: {best_score}"
        );
    }

    #[test]
    fn black_box_optimizer_is_deterministic_per_seed() {
        let oracle = PrivateOracleScorer::new(11, None);
        let a = BlackBoxOptimizerEngine::new(5, 200, &oracle).search();
        let oracle2 = PrivateOracleScorer::new(11, None);
        let b = BlackBoxOptimizerEngine::new(5, 200, &oracle2).search();
        assert_eq!(a, b, "same seed + same oracle => identical trajectory");
    }

    #[test]
    fn black_box_optimizer_stops_at_the_query_budget() {
        // With the oracle's OWN budget set to exactly the engine's budget, the engine
        // must consume no more than budget queries and not error. Set the oracle budget
        // one BELOW the engine's: the engine stops gracefully when the oracle refuses.
        let oracle = PrivateOracleScorer::new(13, Some(SubmissionBudget::new(50)));
        let engine = BlackBoxOptimizerEngine::new(7, 1000, &oracle);
        // Should not panic / not error: the engine returns its best-so-far when the
        // oracle cuts it off at 50 queries even though its own budget was 1000.
        let best = engine.search();
        assert_eq!(
            oracle.remaining_queries(),
            Some(0),
            "the oracle's tighter budget must be fully and exactly consumed"
        );
        // And the returned artifact is still a valid, finite candidate.
        assert!(best.params.iter().all(|p| p.is_finite()));
        assert_eq!(best.params.len(), D);
    }

    #[test]
    fn black_box_optimizer_self_budget_is_respected() {
        // No oracle budget; the ENGINE's own query_budget bounds the work. Count the
        // queries via a fresh oracle with a budget equal to the engine budget: the
        // engine must spend at most that many.
        let budget = 64u32;
        let oracle = PrivateOracleScorer::new(21, Some(SubmissionBudget::new(budget)));
        let engine = BlackBoxOptimizerEngine::new(9, budget, &oracle);
        let _ = engine.search();
        // The engine spends exactly `budget` queries (origin + budget-1 mutations),
        // never exceeding it.
        assert_eq!(
            oracle.remaining_queries(),
            Some(0),
            "engine must spend its full self-budget and no more"
        );
    }

    // --- privileged-hardware stand-in --------------------------------------

    #[test]
    fn privileged_hardware_reports_high_cost_and_valid_ci() {
        let scorer = PrivilegedHardwareScorer::new(4, 1_000.0);
        assert_eq!(scorer.id(), "privileged-hardware");
        let m = scorer.measure(&art([0.0; D])).unwrap();
        assert!(m.value > 0.0 && m.value <= 1.0);
        assert!(m.ci_lower <= m.value && m.value <= m.ci_upper);
        assert!(m.ci_lower >= 0.0 && m.ci_upper <= 1.0);
        assert!(m.n >= 1);
        // The signature of a privileged scorer: a HIGH reported cost.
        assert!(
            m.cost >= 1_000.0,
            "privileged cost must be high: {}",
            m.cost
        );
    }

    #[test]
    fn privileged_hardware_rejects_non_finite_params() {
        // The scorer is pub API and must self-defend: a non-finite param fails closed
        // rather than emitting a Measurement with value=NaN and an unordered CI.
        let scorer = PrivilegedHardwareScorer::new(4, 1_000.0);
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let m = scorer.measure(&art([bad, 0.0, 0.0, 0.0]));
            assert!(
                matches!(m, Err(ScorerError::Rejected(_))),
                "non-finite param must be rejected, got {m:?}"
            );
        }
    }

    // --- human-panel stand-in ----------------------------------------------

    #[test]
    fn human_panel_aggregates_verdicts_into_a_measurement_with_ci() {
        // A panel that scores a specific artifact highly; default neutral otherwise.
        let target = art([1.0, -2.0, 0.5, 1.5]);
        let surface = HiddenTargetSurface;
        let key = surface.to_ref(&target).unwrap().0;
        let scorer =
            HumanPanelScorer::new(vec![(key, vec![0.9, 0.8, 0.85, 0.95])], vec![0.5, 0.5, 0.5]);
        assert_eq!(scorer.id(), "human-panel");
        let m = scorer.verdicts_for(&target);
        let agg = HumanPanelScorer::aggregate(&m);
        // Mean of the recorded panel.
        let expected_mean = (0.9 + 0.8 + 0.85 + 0.95) / 4.0;
        assert!((agg.value - expected_mean).abs() < 1e-9);
        assert_eq!(agg.n, 4);
        assert!(agg.ci_lower < agg.value && agg.value < agg.ci_upper);
        assert!(agg.ci_lower >= 0.0 && agg.ci_upper <= 1.0);

        // An unknown artifact falls back to the neutral default panel.
        let other = art([9.0, 9.0, 9.0, 9.0]);
        let def = HumanPanelScorer::aggregate(&scorer.verdicts_for(&other));
        assert!((def.value - 0.5).abs() < 1e-9, "default panel mean is 0.5");
    }

    #[test]
    fn human_panel_single_verdict_has_an_honestly_wide_ci() {
        let scorer = HumanPanelScorer::new(vec![], vec![0.7]);
        let agg = HumanPanelScorer::aggregate(&scorer.default_verdicts);
        assert_eq!(agg.n, 1);
        // A 1-judge panel must not look artificially certain.
        assert!(
            agg.ci_upper - agg.ci_lower > 0.5,
            "single-panelist CI must be honestly wide: [{}, {}]",
            agg.ci_lower,
            agg.ci_upper
        );
    }

    #[test]
    fn human_panel_new_sanitizes_out_of_range_and_non_finite_verdicts() {
        // new() enforces the [0, 1], finite contract its doc states. An out-of-range
        // verdict (5.0) is clamped to 1.0; a non-finite verdict (inf) is dropped. The
        // resulting aggregate is finite, in range, and properly ordered — none of the
        // invariant breaks the finding reproduced can occur.
        let scorer = HumanPanelScorer::new(vec![], vec![5.0, f64::INFINITY, 0.5, -3.0]);
        let agg = HumanPanelScorer::aggregate(&scorer.default_verdicts);
        // inf dropped; {5.0->1.0, 0.5, -3.0->0.0} survive => n=3, mean=0.5.
        assert_eq!(agg.n, 3);
        assert!(agg.value.is_finite() && (0.0..=1.0).contains(&agg.value));
        assert!(
            agg.ci_lower <= agg.value && agg.value <= agg.ci_upper,
            "CI must be ordered: [{}, {}] around {}",
            agg.ci_lower,
            agg.value,
            agg.ci_upper
        );
        assert!(agg.ci_lower >= 0.0 && agg.ci_upper <= 1.0);
        assert!(
            (agg.value - 0.5).abs() < 1e-9,
            "mean of {{1.0,0.5,0.0}} is 0.5"
        );
    }

    #[test]
    fn human_panel_empty_default_becomes_one_real_neutral_verdict() {
        // An empty default_verdicts must NOT report n=1 of phantom statistical power on a
        // mean of -0.0; new() repairs it to a single neutral verdict, and aggregate on an
        // explicitly empty slice is equally defended.
        let scorer = HumanPanelScorer::new(vec![], vec![]);
        assert_eq!(scorer.default_verdicts, vec![0.5]);
        let agg = HumanPanelScorer::aggregate(&scorer.default_verdicts);
        assert_eq!(agg.n, 1);
        assert!((agg.value - 0.5).abs() < 1e-9);
        assert!(agg.ci_lower <= agg.value && agg.value <= agg.ci_upper);

        // Directly aggregating an empty slice is also fail-safe (n=1 neutral, ordered).
        let empty = HumanPanelScorer::aggregate(&[]);
        assert_eq!(empty.n, 1);
        assert!(empty.value.is_finite());
        assert!(empty.ci_lower <= empty.value && empty.value <= empty.ci_upper);
    }

    #[test]
    fn human_panel_aggregate_orders_the_ci_for_arbitrary_input() {
        // Even passed an out-of-range slice directly (bypassing new()), aggregate must
        // never return value outside [ci_lower, ci_upper] (the finding's break: a 5.0
        // verdict gave ci_lower=4.02 > ci_upper=1.0 with value=5.0 outside both).
        let agg = HumanPanelScorer::aggregate(&[5.0, 5.0, 5.0]);
        assert!(agg.value.is_finite() && (0.0..=1.0).contains(&agg.value));
        assert!(
            agg.ci_lower <= agg.value && agg.value <= agg.ci_upper,
            "ordering invariant must hold: [{}, {}] around {}",
            agg.ci_lower,
            agg.value,
            agg.ci_upper
        );
        // A non-finite verdict slice yields a finite, ordered measurement too.
        let agg2 = HumanPanelScorer::aggregate(&[f64::INFINITY, 0.5]);
        assert!(agg2.value.is_finite());
        assert!(agg2.ci_lower <= agg2.value && agg2.value <= agg2.ci_upper);
    }

    // --- ScorerKind dispatch ----------------------------------------------

    #[tokio::test]
    async fn kind_dispatch_reports_the_declared_kind_for_each_arm() {
        let held = KindDispatchScorer::HeldOutEval(crate::config_opt::LinearScorer::new());
        assert_eq!(held.kind(), ScorerKind::HeldOutEval);
        assert_eq!(held.id(), "linear-accuracy");

        let oracle = KindDispatchScorer::PrivateOracle(PrivateOracleScorer::new(1, None));
        assert_eq!(oracle.kind(), ScorerKind::PrivateOracle);
        assert_eq!(oracle.id(), "private-oracle");

        let hw = KindDispatchScorer::PrivilegedHardware(PrivilegedHardwareScorer::new(1, 100.0));
        assert_eq!(hw.kind(), ScorerKind::PrivilegedHardware);
        assert_eq!(hw.id(), "privileged-hardware");

        let panel = KindDispatchScorer::HumanPanel(HumanPanelScorer::new(vec![], vec![0.5, 0.6]));
        assert_eq!(panel.kind(), ScorerKind::HumanPanel);
        assert_eq!(panel.id(), "human-panel");

        // Every arm scores through the ONE dispatch type and yields a valid measurement.
        for s in [&held, &oracle, &hw, &panel] {
            let m = s
                .score(&art([0.1, 0.2, 0.3, 0.4]), Split::HeldOut)
                .await
                .unwrap();
            assert!(m.value.is_finite());
            assert!(m.ci_lower <= m.value && m.value <= m.ci_upper);
            assert!(Gate::default().min_n > 0); // gate is wired; sanity
        }
    }
}
