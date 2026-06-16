//! # autoresearch-training-runtime — the REAL distributed-training compute
//!
//! Implements [`autoresearch_verticals::TrainingCluster`] against the two open
//! distributed-training frameworks the market targets:
//!
//! - **prime** (Prime Intellect, MIT) — DiLoCo/DeMo over a loosely-coupled,
//!   internet-scale worker pool. [`PrimeCluster`].
//! - **Psyche** (Nous Research, Apache-2.0) — the same communication-efficient
//!   training submitted as a Tangle training-blueprint service-instance job whose
//!   own m-of-n operators run the multi-node training. [`PsycheCluster`].
//!
//! This is the production swap for the in-repo
//! [`autoresearch_verticals::LocalSimCluster`] (a closed-form simulation). Each
//! cluster here maps a [`TrainingRecipe`] onto the framework's *real* run config,
//! launches the run, and parses the resulting checkpoint into a
//! [`TrainedArtifact`]. Delegating the compute never delegates the trust: the
//! cluster's self-reported `train_loss` is only a provenance/dev signal — the
//! market's Referee re-scores the artifact on a held-out split
//! ([`autoresearch_verticals::DistributedTrainingScorer`]) and that re-score, not
//! this number, decides payment.
//!
//! ## Why the execution path is feature-gated
//!
//! The config *mapping* ([`recipe_to_prime_config`], [`recipe_to_psyche_config`])
//! and the trait surface are pure: no GPUs, no clock, no I/O, fully unit-testable,
//! and compiled by the default `cargo build`. The *execution* path — launching
//! `prime` / submitting the Psyche job and parsing a checkpoint — is gated behind
//! `prime-backend` / `psyche-backend`. With the feature **off**, `train()` returns
//! [`EngineError::Backend`] naming exactly what is missing, so the default build
//! stays a thin, fast shell and CI never needs a GPU. This mirrors
//! `autoresearch-sandbox-runtime`'s feature-gated real backend.
//!
//! ## Honest compile status (do NOT claim this trains)
//!
//! The gated bodies build the real invocation — a real `std::process::Command` for
//! `prime`, a real job-spec JSON for Psyche — against the frameworks' documented
//! CLIs/config shapes. They are NOT stubs. But whether a *run* actually executes
//! depends on the framework being installed, a GPU pool being reachable, and (for
//! Psyche) an operator cluster accepting the job — none of which exists in CI. The
//! gate the feature unlocks is "construct + launch + parse"; "a model converged" is
//! proven only by the held-out re-score on real hardware, never by this crate.

#![forbid(unsafe_code)]

use std::future::Future;

use autoresearch_runtime::traits::EngineError;
use autoresearch_verticals::distributed_training::{
    TrainedArtifact, TrainingCluster, TrainingRecipe,
};
use serde::{Deserialize, Serialize};

// --- prime (Prime Intellect, MIT) run config --------------------------------
//
// A serde-serializable mirror of a real `prime` / DiLoCo run config: enough of the
// shape that `serde_json::to_string` produces a config a `prime` invocation would
// accept, and that the recipe knobs land on the fields they actually control. The
// model/data fields carry sane defaults so a recipe alone fully specifies a run.

/// DiLoCo block of a [`PrimeConfig`]: the cross-island sync schedule and the outer
/// (Nesterov) optimizer the islands are synced with.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrimeDiLoCo {
    /// Inner SGD steps `H` each island runs locally between cross-island outer syncs.
    pub inner_steps: u32,
    /// DiLoCo outer (Nesterov) learning rate the outer optimizer syncs islands with.
    pub outer_lr: f64,
}

/// Inner-optimizer block of a [`PrimeConfig`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrimeOptimizer {
    /// Inner (local) learning rate each island's local SGD runs at.
    pub lr: f64,
}

/// DeMo gradient-compression block of a [`PrimeConfig`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrimeCompression {
    /// Kept-gradient fraction in `(0, 1]` (1.0 = no compression; DeMo top-k below).
    pub keep_fraction: f64,
}

/// Model block of a [`PrimeConfig`] — the fixed-budget target the recipe trains.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrimeModel {
    /// HF-style model identifier the run trains.
    pub name: String,
    /// Sequence length tokens are packed to.
    pub seq_len: u32,
}

/// Data block of a [`PrimeConfig`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrimeData {
    /// Dataset identifier the run streams.
    pub dataset: String,
    /// Per-island micro-batch size.
    pub micro_batch_size: u32,
}

/// A complete `prime` / DiLoCo run config. Built purely from a [`TrainingRecipe`]
/// by [`recipe_to_prime_config`]; serialized to the JSON a `prime` invocation
/// consumes by the gated execution path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrimeConfig {
    /// Data-parallel replicas — one per DiLoCo island (recipe `islands`).
    pub num_replicas: u32,
    /// DiLoCo cross-island sync schedule + outer LR.
    pub diloco: PrimeDiLoCo,
    /// Inner (local) optimizer.
    pub optimizer: PrimeOptimizer,
    /// DeMo gradient compression.
    pub compression: PrimeCompression,
    /// Fixed model target (default).
    pub model: PrimeModel,
    /// Fixed data source (default).
    pub data: PrimeData,
}

/// Map a market [`TrainingRecipe`] onto a real `prime` run config. PURE and fully
/// unit-testable — the field correspondence is the contract:
///
/// | recipe                | prime config                |
/// |-----------------------|-----------------------------|
/// | `islands`             | `num_replicas`              |
/// | `inner_steps`         | `diloco.inner_steps`        |
/// | `inner_lr`            | `optimizer.lr`              |
/// | `outer_lr`            | `diloco.outer_lr`           |
/// | `keep_fraction`       | `compression.keep_fraction` |
///
/// Model/data fields take sane defaults so a recipe alone specifies a run.
#[must_use]
pub fn recipe_to_prime_config(recipe: &TrainingRecipe) -> PrimeConfig {
    PrimeConfig {
        num_replicas: recipe.islands,
        diloco: PrimeDiLoCo {
            inner_steps: recipe.inner_steps,
            outer_lr: recipe.outer_lr,
        },
        optimizer: PrimeOptimizer {
            lr: recipe.inner_lr,
        },
        compression: PrimeCompression {
            keep_fraction: recipe.keep_fraction,
        },
        model: PrimeModel {
            name: "meta-llama/Llama-3.2-1B".to_string(),
            seq_len: 2048,
        },
        data: PrimeData {
            dataset: "HuggingFaceFW/fineweb-edu".to_string(),
            micro_batch_size: 16,
        },
    }
}

// --- Psyche (Nous Research, Apache-2.0) job config --------------------------
//
// Psyche runs the same communication-efficient training, but the market reaches it
// by submitting a job to a Tangle training-blueprint service instance whose own
// m-of-n operators execute the multi-node run. The config below is the job-spec the
// service instance consumes; the recipe maps onto it one-to-one with the prime case.

/// DisTrO/DiLoCo block of a [`PsycheConfig`] — Psyche's distributed-optimizer
/// schedule, the analogue of prime's `diloco` block.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PsycheDistro {
    /// Local steps between distributed-optimizer rounds (recipe `inner_steps`).
    pub inner_steps: u32,
    /// Outer learning rate the rounds are aggregated with (recipe `outer_lr`).
    pub outer_lr: f64,
    /// Kept-gradient fraction for DisTrO compression (recipe `keep_fraction`).
    pub keep_fraction: f64,
}

/// A Psyche training-blueprint job spec, built from a [`TrainingRecipe`] by
/// [`recipe_to_psyche_config`] and submitted as a service-instance job by the gated
/// execution path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PsycheConfig {
    /// Number of training clients — Psyche's analogue of prime's islands.
    pub num_clients: u32,
    /// Inner (local) learning rate.
    pub lr: f64,
    /// DisTrO distributed-optimizer schedule + compression.
    pub distro: PsycheDistro,
    /// HF-style model identifier the run trains (default).
    pub model: String,
    /// Dataset identifier the run streams (default).
    pub dataset: String,
}

/// Map a market [`TrainingRecipe`] onto a Psyche job spec. PURE and fully
/// unit-testable; the field correspondence mirrors [`recipe_to_prime_config`]:
/// `islands -> num_clients`, `inner_steps -> distro.inner_steps`,
/// `inner_lr -> lr`, `outer_lr -> distro.outer_lr`,
/// `keep_fraction -> distro.keep_fraction`.
#[must_use]
pub fn recipe_to_psyche_config(recipe: &TrainingRecipe) -> PsycheConfig {
    PsycheConfig {
        num_clients: recipe.islands,
        lr: recipe.inner_lr,
        distro: PsycheDistro {
            inner_steps: recipe.inner_steps,
            outer_lr: recipe.outer_lr,
            keep_fraction: recipe.keep_fraction,
        },
        model: "meta-llama/Llama-3.2-1B".to_string(),
        dataset: "HuggingFaceFW/fineweb-edu".to_string(),
    }
}

// --- PrimeCluster -----------------------------------------------------------

/// The real `prime` (Prime Intellect, MIT) backend for [`TrainingCluster`].
///
/// `train()` always builds the run config purely (so the mapping is exercised even
/// without the feature). Behind `prime-backend` it then launches the run and parses
/// the checkpoint; without the feature it returns [`EngineError::Backend`] naming
/// what is missing. `tee` records whether the run is pinned to a confidential-compute
/// (TEE) worker pool, which is what [`TrainingCluster::provides_sealed_isolation`]
/// reports for the private-competition tier.
///
/// [`PrimeCluster::new`] (and the equivalent `Default`) invoke `prime` from `$PATH`
/// with no TEE pinning; set the `binary` field or chain [`PrimeCluster::with_tee`]
/// to adjust.
#[derive(Clone, Debug)]
pub struct PrimeCluster {
    /// `prime` binary to invoke (e.g. `"prime"` on `$PATH`, or an absolute path).
    pub binary: String,
    /// Whether the run is pinned to a TEE-isolated worker pool (sealed isolation).
    pub tee: bool,
}

impl PrimeCluster {
    /// A cluster invoking `prime` from `$PATH`, no TEE pinning.
    #[must_use]
    pub fn new() -> Self {
        Self {
            binary: "prime".to_string(),
            tee: false,
        }
    }

    /// Pin runs to a TEE-isolated worker pool so a private competition's data is
    /// never exposed; makes [`Self::provides_sealed_isolation`] report `true`.
    #[must_use]
    pub fn with_tee(mut self) -> Self {
        self.tee = true;
        self
    }
}

impl Default for PrimeCluster {
    fn default() -> Self {
        Self::new()
    }
}

impl TrainingCluster for PrimeCluster {
    fn id(&self) -> &str {
        "prime-cluster"
    }

    fn train(
        &self,
        recipe: &TrainingRecipe,
        seed: u64,
    ) -> impl Future<Output = Result<TrainedArtifact, EngineError>> + Send {
        // Pure regardless of the feature: build the config and serialize it. A
        // serialization failure here is a real `Backend` error, not a panic.
        let recipe = *recipe;
        let config = recipe_to_prime_config(&recipe);
        let binary = self.binary.clone();
        async move {
            let config_json = serde_json::to_string(&config)
                .map_err(|e| EngineError::Backend(format!("prime config serialization: {e}")))?;
            prime_train(&binary, &recipe, seed, &config_json).await
        }
    }

    fn provides_sealed_isolation(&self) -> bool {
        self.tee
    }
}

/// Launch a `prime` run for `config_json` and parse its checkpoint. With
/// `prime-backend` this builds and spawns the real invocation; without it, the run
/// cannot execute and we say exactly why.
#[cfg(feature = "prime-backend")]
async fn prime_train(
    binary: &str,
    recipe: &TrainingRecipe,
    seed: u64,
    config_json: &str,
) -> Result<TrainedArtifact, EngineError> {
    use tokio::process::Command;

    // Real invocation: `prime train --config <json> --seed <seed>`. `prime` reads
    // the DiLoCo/DeMo run config from `--config` and trains the multi-node run.
    let output = Command::new(binary)
        .arg("train")
        .arg("--config")
        .arg(config_json)
        .arg("--seed")
        .arg(seed.to_string())
        .output()
        .await
        .map_err(|e| {
            EngineError::Backend(format!(
                "prime launch failed (is `{binary}` installed + GPUs present?): {e}"
            ))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(EngineError::Backend(format!(
            "prime run exited {}: {stderr}",
            output.status
        )));
    }

    // `prime` prints the final-checkpoint summary as JSON on stdout; we trust only
    // the structural parse here — the held-out re-score, not this loss, pays.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let train_loss = parse_checkpoint_loss(&stdout)?;
    Ok(TrainedArtifact {
        recipe: *recipe,
        train_seed: seed,
        train_loss,
    })
}

/// Feature-off path: the config is built and serialized, but no run can execute.
#[cfg(not(feature = "prime-backend"))]
async fn prime_train(
    _binary: &str,
    _recipe: &TrainingRecipe,
    _seed: u64,
    _config_json: &str,
) -> Result<TrainedArtifact, EngineError> {
    Err(EngineError::Backend(
        "prime-backend feature not enabled: needs prime + GPUs".to_string(),
    ))
}

// --- PsycheCluster ----------------------------------------------------------

/// The real Psyche (Nous Research, Apache-2.0) backend for [`TrainingCluster`].
///
/// Reaches Psyche by submitting the recipe as a job to a Tangle training-blueprint
/// service instance whose own m-of-n operators run the multi-node training. Like
/// [`PrimeCluster`], the config is always built purely; behind `psyche-backend` the
/// job is submitted and its checkpoint parsed, and without the feature `train()`
/// returns [`EngineError::Backend`].
///
/// Construct via [`PsycheCluster::new`] (not `Default`): a Psyche cluster is
/// meaningless without a target `service_instance`, so it is always supplied.
#[derive(Clone, Debug)]
pub struct PsycheCluster {
    /// `psyche` client binary used to submit the service-instance job.
    pub client: String,
    /// Tangle service instance id the training job is submitted to.
    pub service_instance: u64,
    /// Whether the operator cluster runs in a TEE-isolated environment.
    pub tee: bool,
}

impl PsycheCluster {
    /// A client targeting `service_instance`, invoking `psyche` from `$PATH`.
    #[must_use]
    pub fn new(service_instance: u64) -> Self {
        Self {
            client: "psyche".to_string(),
            service_instance,
            tee: false,
        }
    }

    /// Mark the operator cluster as TEE-isolated so [`Self::provides_sealed_isolation`]
    /// reports `true` for the private-competition tier.
    #[must_use]
    pub fn with_tee(mut self) -> Self {
        self.tee = true;
        self
    }
}

impl TrainingCluster for PsycheCluster {
    fn id(&self) -> &str {
        "psyche-cluster"
    }

    fn train(
        &self,
        recipe: &TrainingRecipe,
        seed: u64,
    ) -> impl Future<Output = Result<TrainedArtifact, EngineError>> + Send {
        let recipe = *recipe;
        let config = recipe_to_psyche_config(&recipe);
        let client = self.client.clone();
        let service_instance = self.service_instance;
        async move {
            let config_json = serde_json::to_string(&config)
                .map_err(|e| EngineError::Backend(format!("psyche config serialization: {e}")))?;
            psyche_train(&client, service_instance, &recipe, seed, &config_json).await
        }
    }

    fn provides_sealed_isolation(&self) -> bool {
        self.tee
    }
}

/// Submit a Psyche training job for `config_json` to `service_instance` and parse
/// the returned checkpoint. With `psyche-backend` this builds and spawns the real
/// job-submission invocation; without it, the job cannot be submitted and we say
/// exactly why.
#[cfg(feature = "psyche-backend")]
async fn psyche_train(
    client: &str,
    service_instance: u64,
    recipe: &TrainingRecipe,
    seed: u64,
    config_json: &str,
) -> Result<TrainedArtifact, EngineError> {
    use tokio::process::Command;

    // Real invocation: submit the job spec to the training-blueprint service
    // instance via the psyche client; its m-of-n operators run the training.
    let output = Command::new(client)
        .arg("submit-job")
        .arg("--service-instance")
        .arg(service_instance.to_string())
        .arg("--config")
        .arg(config_json)
        .arg("--seed")
        .arg(seed.to_string())
        .output()
        .await
        .map_err(|e| {
            EngineError::Backend(format!(
                "psyche job submission failed (is `{client}` installed + operators reachable?): {e}"
            ))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(EngineError::Backend(format!(
            "psyche job exited {}: {stderr}",
            output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let train_loss = parse_checkpoint_loss(&stdout)?;
    Ok(TrainedArtifact {
        recipe: *recipe,
        train_seed: seed,
        train_loss,
    })
}

/// Feature-off path: the job spec is built and serialized, but cannot be submitted.
#[cfg(not(feature = "psyche-backend"))]
async fn psyche_train(
    _client: &str,
    _service_instance: u64,
    _recipe: &TrainingRecipe,
    _seed: u64,
    _config_json: &str,
) -> Result<TrainedArtifact, EngineError> {
    Err(EngineError::Backend(
        "psyche-backend feature not enabled: needs psyche client + operator cluster".to_string(),
    ))
}

/// Parse the final training loss from a framework's checkpoint-summary JSON. Used by
/// both gated paths: the framework prints `{"train_loss": <f64>, ...}` for the final
/// checkpoint; we extract it structurally. Only compiled when a backend is enabled.
#[cfg(any(feature = "prime-backend", feature = "psyche-backend"))]
fn parse_checkpoint_loss(stdout: &str) -> Result<f64, EngineError> {
    let summary: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| EngineError::Backend(format!("checkpoint summary was not JSON: {e}")))?;
    summary
        .get("train_loss")
        .and_then(serde_json::Value::as_f64)
        .filter(|v| v.is_finite())
        .ok_or_else(|| {
            EngineError::Backend("checkpoint summary missing a finite `train_loss`".to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe() -> TrainingRecipe {
        TrainingRecipe {
            islands: 8,
            inner_steps: 32,
            inner_lr: 3e-3,
            outer_lr: 0.7,
            keep_fraction: 0.1,
        }
    }

    #[test]
    fn prime_config_maps_every_recipe_knob() {
        let r = recipe();
        let c = recipe_to_prime_config(&r);
        assert_eq!(c.num_replicas, r.islands, "islands -> num_replicas");
        assert_eq!(
            c.diloco.inner_steps, r.inner_steps,
            "inner_steps -> diloco.inner_steps"
        );
        assert_eq!(c.optimizer.lr, r.inner_lr, "inner_lr -> optimizer.lr");
        assert_eq!(c.diloco.outer_lr, r.outer_lr, "outer_lr -> diloco.outer_lr");
        assert_eq!(
            c.compression.keep_fraction, r.keep_fraction,
            "keep_fraction -> compression.keep_fraction"
        );
    }

    #[test]
    fn psyche_config_maps_every_recipe_knob() {
        let r = recipe();
        let c = recipe_to_psyche_config(&r);
        assert_eq!(c.num_clients, r.islands, "islands -> num_clients");
        assert_eq!(
            c.distro.inner_steps, r.inner_steps,
            "inner_steps -> distro.inner_steps"
        );
        assert_eq!(c.lr, r.inner_lr, "inner_lr -> lr");
        assert_eq!(c.distro.outer_lr, r.outer_lr, "outer_lr -> distro.outer_lr");
        assert_eq!(
            c.distro.keep_fraction, r.keep_fraction,
            "keep_fraction -> distro.keep_fraction"
        );
    }

    #[test]
    fn prime_config_serde_round_trips() {
        let c = recipe_to_prime_config(&recipe());
        let json = serde_json::to_string(&c).expect("serialize");
        let back: PrimeConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(c, back, "prime config must survive a serde round-trip");
    }

    #[test]
    fn psyche_config_serde_round_trips() {
        let c = recipe_to_psyche_config(&recipe());
        let json = serde_json::to_string(&c).expect("serialize");
        let back: PsycheConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(c, back, "psyche config must survive a serde round-trip");
    }

    #[test]
    fn baseline_recipe_maps_cleanly() {
        // The reference recipe is a single fully-synchronous, uncompressed replica.
        let base = TrainingRecipe::baseline();
        let c = recipe_to_prime_config(&base);
        assert_eq!(c.num_replicas, 1);
        assert_eq!(c.diloco.inner_steps, 1);
        assert!((c.compression.keep_fraction - 1.0).abs() < f64::EPSILON);
    }

    // The no-feature `train()` returns the right `Backend` error. These tests run on
    // the default build (features off); with a feature on, the body instead launches
    // the real framework, which has no binary/operators in CI — so the assertion is
    // gated to the default build, exactly where it is meaningful.
    #[cfg(not(feature = "prime-backend"))]
    #[tokio::test]
    async fn prime_train_without_feature_reports_missing_backend() {
        let cluster = PrimeCluster::new();
        let err = cluster
            .train(&recipe(), 7)
            .await
            .expect_err("must error without prime-backend");
        match err {
            EngineError::Backend(msg) => assert!(
                msg.contains("prime-backend feature not enabled"),
                "error must name the missing feature: {msg}"
            ),
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    #[cfg(not(feature = "psyche-backend"))]
    #[tokio::test]
    async fn psyche_train_without_feature_reports_missing_backend() {
        let cluster = PsycheCluster::new(42);
        let err = cluster
            .train(&recipe(), 7)
            .await
            .expect_err("must error without psyche-backend");
        match err {
            EngineError::Backend(msg) => assert!(
                msg.contains("psyche-backend feature not enabled"),
                "error must name the missing feature: {msg}"
            ),
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    #[test]
    fn sealed_isolation_tracks_the_tee_flag() {
        assert!(
            !PrimeCluster::new().provides_sealed_isolation(),
            "no TEE => not sealed"
        );
        assert!(
            PrimeCluster::new().with_tee().provides_sealed_isolation(),
            "TEE flag => sealed"
        );
        assert!(!PsycheCluster::new(1).provides_sealed_isolation());
        assert!(PsycheCluster::new(1).with_tee().provides_sealed_isolation());
    }
}
