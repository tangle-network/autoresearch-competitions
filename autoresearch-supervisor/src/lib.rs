//! # autoresearch-supervisor — the universal improvement engine
//!
//! One `Engine` that improves **any** artifact against **any** [`Scorer`], so a new
//! algorithmic-advancement domain is just a new scorer — never a new engine. This is
//! the generalization that bounds the blueprint's maintenance: the *hard, reusable*
//! part (how to drive a long-horizon improvement search) lives here once; domains
//! differ only in *how "better" is measured*.
//!
//! ## Two interchangeable backends behind one trait
//!
//! - [`SupervisorEngine`] — the always-available, deterministic **stand-in**: a
//!   seeded local search that proposes candidate [`GenericArtifact`]s and keeps the
//!   ones the dev [`Scorer`] rewards. No `rand`, no clock, no I/O, so every vertical's
//!   CI proof is byte-reproducible. This is what the program-superopt / solver /
//!   theorem-proving / agent verticals drive in tests.
//! - [`SubprocessEngine`] (feature `subprocess-backend`) — an **external-process**
//!   backend: it shells out to a caller-supplied driver binary with a JSON manifest
//!   describing the task, then parses the returned artifact content from stdout. Both
//!   implement `Engine<Artifact = GenericArtifact>`, so swapping the stand-in for the
//!   subprocess backend is a one-line change at the call site. The driver is supplied
//!   by the caller; this crate does not ship one.
//!
//! ## Honest seam — the stand-in does not "think"
//!
//! The stand-in is a *search over a numeric encoding* (`GenericArtifact::params`),
//! which is what lets a deterministic test show the market certifies a real,
//! gate-clearing lift across domains. It does not read or write source code, proofs,
//! or prompts. A real external solver/prover/agent loop can be plugged in behind
//! [`SubprocessEngine`] by providing a driver that consumes the manifest protocol and
//! emits improved artifact content; until then, the market mechanism is exercised by
//! the deterministic stand-in.

#![forbid(unsafe_code)]

use std::future::Future;

use autoresearch_runtime::traits::{
    Engine, EngineContext, EngineError, Scorer, ScorerError, Surface, SurfaceError,
};
use autoresearch_runtime::types::{ArtifactRef, Split};
use serde::{Deserialize, Serialize};

// --- The universal artifact -------------------------------------------------

/// What kind of thing is being improved. The market is agnostic to this — it only
/// tags the artifact for provenance and lets the real backend pick a domain prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactKind {
    /// A program / algorithm implementation (superoptimization).
    Program,
    /// A solver or heuristic for a combinatorial / OR problem.
    Solver,
    /// A formal proof / proof script.
    Proof,
    /// An agent profile (skills / prompts / tools / memory).
    AgentProfile,
    /// A model or pipeline configuration (HPO).
    Config,
    /// A forecasting / statistical model.
    Forecast,
    /// A prompt or retrieval pipeline.
    Prompt,
    /// Anything else.
    Text,
}

/// The universal artifact every vertical shares. It carries two representations:
///
/// - `params` — a numeric encoding the deterministic [`SupervisorEngine`] searches
///   and a domain [`Scorer`] decodes into its metric. This is what makes one engine
///   work across every domain in CI.
/// - `content` — the real artifact text (source, proof, prompt, profile) an
///   external backend operates on and what `to_ref` content-addresses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GenericArtifact {
    pub kind: ArtifactKind,
    pub params: Vec<f64>,
    pub content: String,
}

impl GenericArtifact {
    /// Construct an artifact.
    #[must_use]
    pub fn new(kind: ArtifactKind, params: Vec<f64>, content: impl Into<String>) -> Self {
        Self {
            kind,
            params,
            content: content.into(),
        }
    }

    /// A baseline artifact of `dim` zero-valued params (the point a domain measures
    /// lift against). `content` carries a domain-readable description.
    #[must_use]
    pub fn baseline(kind: ArtifactKind, dim: usize, content: impl Into<String>) -> Self {
        Self::new(kind, vec![0.0; dim], content)
    }
}

// --- The universal surface --------------------------------------------------

/// The surface for [`GenericArtifact`]: a finite, all-finite parameter vector with
/// full-replacement deltas. Every vertical can reuse this — a domain only needs its
/// own surface if it has extra structural constraints on `content`.
#[derive(Clone, Debug, Default)]
pub struct GenericSurface;

impl Surface for GenericSurface {
    type Artifact = GenericArtifact;

    fn id(&self) -> &str {
        "generic-artifact"
    }

    fn validate(&self, artifact: &Self::Artifact) -> Result<(), SurfaceError> {
        if artifact.params.is_empty() {
            return Err(SurfaceError::Invalid("artifact has no params".into()));
        }
        if artifact.params.iter().any(|p| !p.is_finite()) {
            return Err(SurfaceError::Invalid("params must be finite".into()));
        }
        Ok(())
    }

    fn apply_delta(
        &self,
        _base: &Self::Artifact,
        delta: &Self::Artifact,
    ) -> Result<Self::Artifact, SurfaceError> {
        // Full-replacement surface: a produced candidate supersedes the baseline.
        self.validate(delta)?;
        Ok(delta.clone())
    }

    fn to_ref(&self, artifact: &Self::Artifact) -> Result<ArtifactRef, SurfaceError> {
        self.validate(artifact)?;
        // Content reference: FNV-1a over kind + params + content bytes.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut absorb = |bytes: &[u8]| {
            for &b in bytes {
                hash ^= u64::from(b);
                hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
            }
        };
        absorb(&[artifact.kind as u8]);
        for p in &artifact.params {
            absorb(&p.to_bits().to_le_bytes());
        }
        absorb(artifact.content.as_bytes());
        Ok(ArtifactRef(format!("generic:{hash:016x}")))
    }
}

// --- Deterministic search PRNG ----------------------------------------------

/// A 64-bit linear-congruential generator (Knuth MMIX constants); the same seeded,
/// reproducible PRNG the deterministic verticals use — no `rand`, no clock.
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

// --- The universal engine (deterministic stand-in) --------------------------

/// Number of search proposals the stand-in evaluates per produce, unless overridden.
/// A proxy for the live supervisor's long-horizon improvement steps.
pub const DEFAULT_BUDGET: usize = 256;

/// The universal improvement engine, deterministic stand-in form: a seeded local
/// search that proposes [`GenericArtifact`] candidates and keeps the ones the dev
/// [`Scorer`] rewards. Generic over the dev scorer, so the *same* engine improves a
/// program, a solver, a proof, an agent, or a forecaster — each is just a different
/// `Sc`. The external-process backend is [`SubprocessEngine`].
#[derive(Clone, Debug)]
pub struct SupervisorEngine<Sc> {
    researcher: String,
    start: GenericArtifact,
    dev_scorer: Sc,
    budget: usize,
    step: f64,
    seed: u64,
    sealed: bool,
}

impl<Sc> SupervisorEngine<Sc> {
    /// Improve `start` by searching for higher dev score. `dev_scorer` is the
    /// researcher-visible signal (scored on [`Split::Dev`]); the Referee re-scores the
    /// produced artifact on held-out. `seed` makes the whole search reproducible.
    #[must_use]
    pub fn new(
        researcher: impl Into<String>,
        start: GenericArtifact,
        dev_scorer: Sc,
        seed: u64,
    ) -> Self {
        Self {
            researcher: researcher.into(),
            start,
            dev_scorer,
            budget: DEFAULT_BUDGET,
            step: 1.0,
            seed,
            sealed: false,
        }
    }

    /// Override the number of search proposals (long-horizon step proxy).
    #[must_use]
    pub fn with_budget(mut self, budget: usize) -> Self {
        self.budget = budget;
        self
    }

    /// Override the per-step perturbation magnitude.
    #[must_use]
    pub fn with_step(mut self, step: f64) -> Self {
        self.step = step;
        self
    }

    /// Mark this engine as running inside a sealed (TEE) environment, so a private
    /// competition's data is never exposed (forwarded to `provides_sealed_isolation`).
    #[must_use]
    pub fn sealed(mut self, sealed: bool) -> Self {
        self.sealed = sealed;
        self
    }

    /// The researcher this engine submits for.
    #[must_use]
    pub fn researcher(&self) -> &str {
        &self.researcher
    }
}

impl<Sc> Engine for SupervisorEngine<Sc>
where
    Sc: Scorer<Artifact = GenericArtifact> + Clone + Send + Sync,
{
    type Artifact = GenericArtifact;

    fn id(&self) -> &str {
        "local-search"
    }

    fn produce(
        &self,
        _ctx: &EngineContext,
    ) -> impl Future<Output = Result<Self::Artifact, EngineError>> + Send {
        // Own everything the future needs so it is `Send` and self-contained (a real
        // service-client handle would be cloned the same way).
        let dev = self.dev_scorer.clone();
        let mut best = self.start.clone();
        let budget = self.budget;
        let step = self.step;
        let seed = self.seed;
        async move {
            let to_err = |e: ScorerError| EngineError::Backend(e.to_string());
            let mut best_v = dev.score(&best, Split::Dev).await.map_err(to_err)?.value;
            let mut rng = Lcg::new(seed);
            for _ in 0..budget {
                // Propose: perturb each param around the current best. The encoding is
                // what the domain scorer decodes — the engine itself is domain-blind.
                let params: Vec<f64> = best
                    .params
                    .iter()
                    .map(|p| p + step * rng.next_signed())
                    .collect();
                let cand = GenericArtifact::new(best.kind, params, best.content.clone());
                let v = dev.score(&cand, Split::Dev).await.map_err(to_err)?.value;
                if v > best_v {
                    best_v = v;
                    best = cand;
                }
            }
            Ok(best)
        }
    }

    fn provides_sealed_isolation(&self) -> bool {
        self.sealed
    }
}

// --- Subprocess backend (honest external-process seam) ----------------------

/// The task manifest handed to the external driver: what to improve, where to start,
/// and how much long-horizon budget it has. Serialized to JSON and passed to the
/// subprocess; the driver returns the improved `content`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubprocessManifest {
    pub kind: ArtifactKind,
    /// The baseline artifact `content` the supervisor improves from.
    pub baseline_content: String,
    /// Long-horizon step budget for the supervisor.
    pub budget_steps: usize,
    /// Optional command the supervisor shells to for the dev-eval signal (the
    /// researcher-visible scorer); `None` lets the runtime use its own eval.
    pub dev_eval_cmd: Option<String>,
}

/// An external-process engine that shells out to a caller-supplied driver binary.
/// Implements the same `Engine` as [`SupervisorEngine`], so it is a drop-in for the
/// deterministic stand-in.
///
/// **Honest status:** this is a generic subprocess seam, not an integration with any
/// specific external framework. `produce` builds the manifest, spawns the driver, and
/// parses stdout. Without the `subprocess-backend` feature, `produce` returns a named
/// [`EngineError::Backend`]. The market never trusts what comes back — the Referee
/// re-scores the produced artifact on held-out, exactly as for any engine.
#[derive(Clone, Debug)]
pub struct SubprocessEngine {
    researcher: String,
    /// The driver binary invoked with `--manifest <json>`. This is supplied by the
    /// caller; this crate does not ship a driver.
    pub driver: String,
    pub manifest: SubprocessManifest,
    pub sealed: bool,
}

impl SubprocessEngine {
    /// Construct the subprocess backend. `driver` is the binary launched with the
    /// manifest; it must emit improved artifact content as JSON `{ "content": "..." }`
    /// on stdout.
    #[must_use]
    pub fn new(
        researcher: impl Into<String>,
        driver: impl Into<String>,
        manifest: SubprocessManifest,
    ) -> Self {
        Self {
            researcher: researcher.into(),
            driver: driver.into(),
            manifest,
            sealed: false,
        }
    }

    /// Pin the subprocess to a TEE-isolated worker (sealed isolation).
    #[must_use]
    pub fn with_tee(mut self) -> Self {
        self.sealed = true;
        self
    }

    /// The researcher this engine submits for.
    #[must_use]
    pub fn researcher(&self) -> &str {
        &self.researcher
    }
}

impl Engine for SubprocessEngine {
    type Artifact = GenericArtifact;

    fn id(&self) -> &str {
        "subprocess-engine"
    }

    fn produce(
        &self,
        _ctx: &EngineContext,
    ) -> impl Future<Output = Result<Self::Artifact, EngineError>> + Send {
        let manifest = self.manifest.clone();
        #[cfg(feature = "subprocess-backend")]
        let driver = self.driver.clone();
        async move {
            let manifest_json = serde_json::to_string(&manifest)
                .map_err(|e| EngineError::Backend(format!("manifest serialization: {e}")))?;

            #[cfg(feature = "subprocess-backend")]
            {
                // Spawn the external driver with the manifest; read back the improved
                // artifact content on stdout as JSON `{ "content": "..." }`.
                let out = tokio::process::Command::new(&driver)
                    .arg("--manifest")
                    .arg(&manifest_json)
                    .output()
                    .await
                    .map_err(|e| EngineError::Backend(format!("subprocess driver spawn: {e}")))?;
                if !out.status.success() {
                    return Err(EngineError::Backend(format!(
                        "subprocess driver exited {}: {}",
                        out.status,
                        String::from_utf8_lossy(&out.stderr)
                    )));
                }
                let improved = parse_runtime_output(&out.stdout)?;
                Ok(GenericArtifact::new(manifest.kind, Vec::new(), improved))
            }

            #[cfg(not(feature = "subprocess-backend"))]
            {
                let _ = manifest_json;
                Err(EngineError::Backend(
                    "subprocess-backend feature not enabled: supply a driver binary to run the external process".into(),
                ))
            }
        }
    }

    fn provides_sealed_isolation(&self) -> bool {
        self.sealed
    }
}

/// Extract the improved artifact `content` from the driver's JSON stdout. Behind the
/// feature because only the subprocess path parses it.
#[cfg(feature = "subprocess-backend")]
fn parse_runtime_output(stdout: &[u8]) -> Result<String, EngineError> {
    #[derive(Deserialize)]
    struct Out {
        content: String,
    }
    let out: Out = serde_json::from_slice(stdout)
        .map_err(|e| EngineError::Backend(format!("subprocess driver output parse: {e}")))?;
    Ok(out.content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autoresearch_runtime::types::Measurement;

    /// A deterministic dev/held-out scorer for the self-test: rewards params that
    /// approach a hidden target (value = -sum of squared error). A perfect recovery
    /// scores 0; the zero-param baseline scores a large negative value, so any real
    /// search produces a positive lift — exactly the shape every vertical relies on.
    #[derive(Clone, Debug)]
    struct QuadraticScorer {
        target: Vec<f64>,
    }

    impl Scorer for QuadraticScorer {
        type Artifact = GenericArtifact;
        fn id(&self) -> &str {
            "quadratic"
        }
        fn score(
            &self,
            artifact: &Self::Artifact,
            _split: Split,
        ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send {
            let sse: f64 = artifact
                .params
                .iter()
                .zip(&self.target)
                .map(|(p, t)| (p - t).powi(2))
                .sum();
            let m = Measurement {
                value: -sse,
                ci_lower: -sse,
                ci_upper: -sse,
                n: 16,
                cost: 1.0,
            };
            std::future::ready(Ok(m))
        }
    }

    fn ctx() -> EngineContext {
        EngineContext {
            competition: 1,
            baseline_ref: ArtifactRef("base".into()),
            dev_split_ref: None,
            budget_wei: 0,
            egress_policy: None,
        }
    }

    #[tokio::test]
    async fn supervisor_improves_any_scorer() {
        let target = vec![1.0, -2.0, 0.5, 1.5];
        let scorer = QuadraticScorer {
            target: target.clone(),
        };
        let start = GenericArtifact::baseline(ArtifactKind::Config, target.len(), "baseline");
        let engine = SupervisorEngine::new("r", start.clone(), scorer.clone(), 7).with_budget(2000);

        let start_v = scorer.score(&start, Split::HeldOut).await.unwrap().value;
        let produced = engine.produce(&ctx()).await.unwrap();
        let produced_v = scorer.score(&produced, Split::HeldOut).await.unwrap().value;

        assert!(
            produced_v > start_v + 1.0,
            "the universal engine must improve the artifact: {start_v} -> {produced_v}"
        );
        // Never worse than where it started (keeps the running best).
        assert!(produced_v >= start_v);
    }

    #[tokio::test]
    async fn search_is_deterministic_per_seed() {
        let scorer = QuadraticScorer {
            target: vec![1.0, 2.0, 3.0],
        };
        let start = GenericArtifact::baseline(ArtifactKind::Program, 3, "s");
        let mk = || SupervisorEngine::new("r", start.clone(), scorer.clone(), 42).with_budget(500);
        let a = mk().produce(&ctx()).await.unwrap();
        let b = mk().produce(&ctx()).await.unwrap();
        assert_eq!(a, b, "same seed must reproduce the same artifact");
    }

    #[test]
    fn surface_rejects_empty_and_nonfinite() {
        let s = GenericSurface;
        assert!(
            s.validate(&GenericArtifact::new(ArtifactKind::Text, vec![], "x"))
                .is_err()
        );
        assert!(
            s.validate(&GenericArtifact::new(
                ArtifactKind::Text,
                vec![f64::NAN],
                "x"
            ))
            .is_err()
        );
        assert!(
            s.validate(&GenericArtifact::baseline(ArtifactKind::Text, 3, "x"))
                .is_ok()
        );
    }

    #[cfg(not(feature = "subprocess-backend"))]
    #[tokio::test]
    async fn subprocess_backend_without_feature_reports_missing() {
        let manifest = SubprocessManifest {
            kind: ArtifactKind::Program,
            baseline_content: "fn main() {}".into(),
            budget_steps: 10,
            dev_eval_cmd: None,
        };
        let engine = SubprocessEngine::new("r", "node", manifest);
        let err = engine.produce(&ctx()).await.unwrap_err();
        match err {
            EngineError::Backend(m) => assert!(m.contains("subprocess-backend feature not enabled")),
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    #[test]
    fn engine_forwards_sealed_isolation() {
        let scorer = QuadraticScorer { target: vec![0.0] };
        let start = GenericArtifact::baseline(ArtifactKind::Config, 1, "s");
        assert!(
            !SupervisorEngine::new("r", start.clone(), scorer.clone(), 1)
                .provides_sealed_isolation()
        );
        assert!(
            SupervisorEngine::new("r", start, scorer, 1)
                .sealed(true)
                .provides_sealed_isolation()
        );
    }
}
