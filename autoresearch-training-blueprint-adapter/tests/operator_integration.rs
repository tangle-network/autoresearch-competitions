//! Integration tests for the training-blueprint operator adapter.
//!
//! These tests mock the operator HTTP API with `wiremock` so they exercise the
//! real HTTP path without needing a live operator or GPU.

#![cfg(feature = "operator-backend")]

use autoresearch_training_blueprint_adapter::{
    BlueprintOperatorCluster, BlueprintOperatorConfig, BlueprintTaskConfig,
};
use autoresearch_verticals::distributed_training::{TrainingCluster, TrainingRecipe};
use wiremock::{
    Match, ResponseTemplate,
    matchers::{method, path, path_regex},
};

fn recipe() -> TrainingRecipe {
    TrainingRecipe {
        islands: 4,
        inner_steps: 32,
        inner_lr: 3e-3,
        outer_lr: 0.7,
        keep_fraction: 0.1,
    }
}

fn config_for(server_uri: &str) -> BlueprintOperatorConfig {
    BlueprintOperatorConfig {
        endpoint: server_uri.to_string(),
        auth_token: Some("test-token".to_string()),
        task: BlueprintTaskConfig {
            base_model: "meta-llama/Llama-3.2-1B".to_string(),
            dataset_url: "HuggingFaceFW/fineweb-edu".to_string(),
            method: "lora".to_string(),
            total_epochs: 1,
        },
        poll_interval_ms: 10,
        timeout_secs: 5,
    }
}

/// A wiremock matcher for the create-job POST body. We just check the JSON
/// contains the expected fields; exact matching is brittle across serde ordering.
struct CreateJobBodyMatch;

impl Match for CreateJobBodyMatch {
    fn matches(&self, request: &wiremock::Request) -> bool {
        let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request.body) else {
            return false;
        };
        body.get("job_id").is_some()
            && body.get("base_model").is_some()
            && body.get("dataset_url").is_some()
            && body.get("method").is_some()
            && body.get("total_epochs").is_some()
            && body.get("sync_interval_steps") == Some(&serde_json::json!(32))
    }
}

#[tokio::test]
async fn submits_job_and_polls_to_completion() {
    let server = wiremock::MockServer::start().await;

    // Create-job mock.
    wiremock::Mock::given(method("POST"))
        .and(path("/v1/training/jobs"))
        .and(CreateJobBodyMatch)
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "job_id": 123,
            "status": "running",
            "message": "Training job started",
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Status mock: first not complete, then complete.
    let status_mock = wiremock::Mock::given(method("GET"))
        .and(path_regex(r"^/v1/training/jobs/\d+$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "job_id": 123,
                    "base_model": "meta-llama/Llama-3.2-1B",
                    "method": "lora",
                    "current_epoch": 1,
                    "total_epochs": 1,
                    "steps_completed": 100,
                    "current_loss": 2.123f32,
                    "operators": 1,
                    "completed": true,
                    "latest_checkpoint_hash": "deadbeef",
                }))
                .insert_header("content-type", "application/json"),
        )
        .expect(1..=5)
        .mount_as_scoped(&server)
        .await;

    let cluster = BlueprintOperatorCluster::new(config_for(&server.uri()));
    let artifact = cluster
        .train(&recipe(), 7)
        .await
        .expect("train should succeed against mock operator");

    assert_eq!(artifact.train_seed, 7);
    assert!((artifact.train_loss - 2.123).abs() < 1e-6);
    assert_eq!(artifact.recipe, recipe());

    drop(status_mock);
}

#[tokio::test]
async fn propagates_operator_error() {
    let server = wiremock::MockServer::start().await;

    wiremock::Mock::given(method("POST"))
        .and(path("/v1/training/jobs"))
        .respond_with(ResponseTemplate::new(500).set_body_string("operator overloaded"))
        .expect(1)
        .mount(&server)
        .await;

    let cluster = BlueprintOperatorCluster::new(config_for(&server.uri()));
    let err = cluster
        .train(&recipe(), 7)
        .await
        .expect_err("should fail when operator returns 500");

    let msg = format!("{err}");
    assert!(msg.contains("operator create-job returned error"), "{msg}");
    assert!(msg.contains("operator overloaded"), "{msg}");
}
