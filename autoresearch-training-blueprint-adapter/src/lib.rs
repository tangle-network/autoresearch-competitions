//! # autoresearch-training-blueprint-adapter
//!
//! A [`TrainingCluster`] adapter that submits [`TrainingRecipe`]s to a running
//! [`training-blueprint`](https://github.com/tangle-network/training-blueprint)
//! operator via its HTTP API and returns a [`TrainedArtifact`] for the
//! autoresearch market to score.
//!
//! The adapter is intentionally thin: it does not train models itself, it does
//! not mint on-chain certifications, and it does not decide payment. It merely
//! translates the market's recipe into a blueprint job, waits for the operator
//! to finish, and carries the operator's self-reported `current_loss` back as
//! the dev-signal `train_loss`. The market's Referee re-scores the artifact on
//! its own held-out split, and that re-score — not this number — decides
//! payment.
//!
//! ## Operator endpoints used
//!
//! - `POST /v1/training/jobs` — create/join a job.
//! - `GET  /v1/training/jobs/{id}` — poll status until `completed == true`.
//!
//! ## Execution path is feature-gated
//!
//! The trait surface and config mapping compile without any network deps. The
//! real HTTP client + polling loop are behind the `operator-backend` feature so
//! the default build stays a thin shell and CI does not need a live operator.

#![forbid(unsafe_code)]

use autoresearch_runtime::traits::EngineError;
use autoresearch_verticals::distributed_training::{
    TrainedArtifact, TrainingCluster, TrainingRecipe,
};
use serde::{Deserialize, Serialize};
use std::future::Future;

// --- Config ------------------------------------------------------------------

/// Job-spec fields the blueprint operator requires but a market
/// [`TrainingRecipe`] does not carry. These describe the fixed training task
/// (model, dataset, method, epoch budget) the cluster is configured to run;
/// the recipe supplies the distributed-training knobs (`inner_steps`, etc.).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlueprintTaskConfig {
    /// HuggingFace-style base model identifier (e.g. "meta-llama/Llama-3.2-1B").
    pub base_model: String,
    /// Dataset URL or HuggingFace dataset name.
    pub dataset_url: String,
    /// Training method the blueprint backend supports (e.g. "sft", "lora", "dpo").
    pub method: String,
    /// Total epochs to run.
    pub total_epochs: u32,
}

impl BlueprintTaskConfig {
    /// Sensible defaults for a small public fine-tuning task.
    #[must_use]
    pub fn default_public() -> Self {
        Self {
            base_model: "meta-llama/Llama-3.2-1B".to_string(),
            dataset_url: "HuggingFaceFW/fineweb-edu".to_string(),
            method: "lora".to_string(),
            total_epochs: 1,
        }
    }
}

/// HTTP client configuration for a training-blueprint operator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlueprintOperatorConfig {
    /// Operator base URL, e.g. `http://localhost:5000`.
    pub endpoint: String,
    /// Optional bearer token for the operator's billing/auth gate.
    pub auth_token: Option<String>,
    /// Fixed task description.
    pub task: BlueprintTaskConfig,
    /// Polling interval in milliseconds while waiting for job completion.
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    /// Maximum time in seconds to wait for job completion.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl BlueprintOperatorConfig {
    /// Config pointing at a local operator with the default public task.
    #[must_use]
    pub fn local() -> Self {
        Self {
            endpoint: "http://localhost:5000".to_string(),
            auth_token: None,
            task: BlueprintTaskConfig::default_public(),
            poll_interval_ms: default_poll_interval_ms(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

const fn default_poll_interval_ms() -> u64 {
    5_000
}

const fn default_timeout_secs() -> u64 {
    3_600
}

// --- Cluster -----------------------------------------------------------------

/// A [`TrainingCluster`] backed by a training-blueprint operator HTTP API.
///
/// `train()` submits a recipe as a blueprint job, polls until completion, and
/// returns a [`TrainedArtifact`]. The real HTTP path is gated behind the
/// `operator-backend` feature; without it, `train()` returns a clear
/// `Backend` error.
#[derive(Clone, Debug)]
pub struct BlueprintOperatorCluster {
    pub config: BlueprintOperatorConfig,
    pub tee: bool,
}

impl BlueprintOperatorCluster {
    /// Connect to the operator described by `config`.
    #[must_use]
    pub fn new(config: BlueprintOperatorConfig) -> Self {
        Self { config, tee: false }
    }

    /// Mark the operator as running inside a TEE-isolated environment so
    /// private-competition tiers accept it.
    #[must_use]
    pub fn with_tee(mut self) -> Self {
        self.tee = true;
        self
    }
}

impl TrainingCluster for BlueprintOperatorCluster {
    fn id(&self) -> &str {
        "training-blueprint-operator"
    }

    fn train(
        &self,
        recipe: &TrainingRecipe,
        seed: u64,
    ) -> impl Future<Output = Result<TrainedArtifact, EngineError>> + Send {
        let recipe = *recipe;
        let config = self.config.clone();
        async move { blueprint_train(&config, &recipe, seed).await }
    }

    fn provides_sealed_isolation(&self) -> bool {
        self.tee
    }
}

/// Derive a deterministic blueprint `job_id` from a recipe and seed. The same
/// inputs always yield the same id, so repeated market runs are idempotent at
/// the operator.
#[must_use]
pub fn derive_job_id(recipe: &TrainingRecipe, seed: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    recipe.islands.hash(&mut hasher);
    recipe.inner_steps.hash(&mut hasher);
    recipe.inner_lr.to_bits().hash(&mut hasher);
    recipe.outer_lr.to_bits().hash(&mut hasher);
    recipe.keep_fraction.to_bits().hash(&mut hasher);
    seed.hash(&mut hasher);
    hasher.finish()
}

// --- Execution ---------------------------------------------------------------

#[cfg(feature = "operator-backend")]
async fn blueprint_train(
    config: &BlueprintOperatorConfig,
    recipe: &TrainingRecipe,
    seed: u64,
) -> Result<TrainedArtifact, EngineError> {
    use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.timeout_secs.max(10)))
        .build()
        .map_err(|e| EngineError::Backend(format!("failed to build HTTP client: {e}")))?;

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(token) = &config.auth_token {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| EngineError::Backend(format!("invalid auth token: {e}")))?;
        headers.insert(AUTHORIZATION, value);
    }

    let job_id = derive_job_id(recipe, seed);
    let endpoint = config.endpoint.trim_end_matches('/');

    // 1. Submit the job.
    let create_url = format!("{endpoint}/v1/training/jobs");
    let body = serde_json::json!({
        "job_id": job_id,
        "base_model": config.task.base_model,
        "dataset_url": config.task.dataset_url,
        "method": config.task.method,
        "total_epochs": config.task.total_epochs,
        "sync_interval_steps": recipe.inner_steps,
    });

    let resp = client
        .post(&create_url)
        .headers(headers.clone())
        .json(&body)
        .send()
        .await
        .map_err(|e| EngineError::Backend(format!("operator create-job request failed: {e}")))?;

    if !resp.status().is_success() {
        let text = resp
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable response>".to_string());
        return Err(EngineError::Backend(format!(
            "operator create-job returned error: {text}"
        )));
    }

    // 2. Poll until the job completes.
    let status_url = format!("{endpoint}/v1/training/jobs/{job_id}");
    let poll_interval = std::time::Duration::from_millis(config.poll_interval_ms);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(config.timeout_secs);

    loop {
        if std::time::Instant::now() > deadline {
            return Err(EngineError::Backend(format!(
                "operator job {job_id} did not complete within {} seconds",
                config.timeout_secs
            )));
        }

        let resp = client
            .get(&status_url)
            .headers(headers.clone())
            .send()
            .await
            .map_err(|e| EngineError::Backend(format!("operator status request failed: {e}")))?;

        if !resp.status().is_success() {
            let text = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable response>".to_string());
            return Err(EngineError::Backend(format!(
                "operator status returned error: {text}"
            )));
        }

        let status: JobStatus = resp.json().await.map_err(|e| {
            EngineError::Backend(format!("operator status was not valid JSON: {e}"))
        })?;

        if status.completed {
            let train_loss = f64::from(status.current_loss);
            if !train_loss.is_finite() {
                return Err(EngineError::Backend(
                    "operator reported a non-finite current_loss".to_string(),
                ));
            }
            return Ok(TrainedArtifact {
                recipe: *recipe,
                train_seed: seed,
                train_loss,
            });
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Feature-off path: the config mapping is built, but no HTTP run can execute.
#[cfg(not(feature = "operator-backend"))]
async fn blueprint_train(
    _config: &BlueprintOperatorConfig,
    _recipe: &TrainingRecipe,
    _seed: u64,
) -> Result<TrainedArtifact, EngineError> {
    Err(EngineError::Backend(
        "operator-backend feature not enabled: needs a running training-blueprint operator"
            .to_string(),
    ))
}

// --- Wire types --------------------------------------------------------------

/// Subset of the operator's `JobStatus` that the adapter needs to decide when a
/// job is finished and what dev-signal loss to carry back. Only used by the
/// real HTTP execution path.
#[cfg(feature = "operator-backend")]
#[derive(Debug, Clone, Deserialize)]
struct JobStatus {
    #[allow(dead_code)]
    job_id: u64,
    completed: bool,
    current_loss: f32,
}

// --- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe() -> TrainingRecipe {
        TrainingRecipe {
            islands: 4,
            inner_steps: 32,
            inner_lr: 3e-3,
            outer_lr: 0.7,
            keep_fraction: 0.1,
        }
    }

    #[test]
    fn derive_job_id_is_deterministic() {
        let r = recipe();
        assert_eq!(derive_job_id(&r, 7), derive_job_id(&r, 7));
        assert_ne!(derive_job_id(&r, 7), derive_job_id(&r, 8));
    }

    #[test]
    fn config_serde_round_trips() {
        let c = BlueprintOperatorConfig::local();
        let json = serde_json::to_string(&c).expect("serialize");
        let back: BlueprintOperatorConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(c, back);
    }

    #[test]
    fn sealed_isolation_tracks_tee_flag() {
        let not_sealed = BlueprintOperatorCluster::new(BlueprintOperatorConfig::local());
        let sealed = not_sealed.clone().with_tee();
        assert!(!not_sealed.provides_sealed_isolation());
        assert!(sealed.provides_sealed_isolation());
    }

    #[cfg(not(feature = "operator-backend"))]
    #[tokio::test]
    async fn train_without_feature_reports_missing_backend() {
        let cluster = BlueprintOperatorCluster::new(BlueprintOperatorConfig::local());
        let err = cluster
            .train(&recipe(), 7)
            .await
            .expect_err("must error without operator-backend");
        match err {
            EngineError::Backend(msg) => assert!(
                msg.contains("operator-backend feature not enabled"),
                "error must name the missing feature: {msg}"
            ),
            other => panic!("expected Backend error, got {other:?}"),
        }
    }
}
