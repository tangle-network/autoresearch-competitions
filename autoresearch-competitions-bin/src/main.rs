//! Blueprint runner for autoresearch-competitions.
//!
//! Boot sequence:
//!   1. Load `.env` + environment configuration.
//!   2. Registration mode: write the operator registration payload and exit early
//!      (the SDK preregistration flow), before any network connection.
//!   3. Connect to Tangle, resolve the service id, load the [`EconomicConfig`].
//!   4. Run the [`BlueprintRunner`] with the Tangle producer/consumer plus a cron
//!      producer that fires `JOB_TICK` on a schedule so OneShot deadlines and
//!      Continuous epochs advance without an external poker.

use autoresearch_competitions_lib::{EconomicConfig, JOB_TICK, router};
use blueprint_producers_extra::cron::CronJob;
use blueprint_sdk::contexts::tangle::TangleClientContext;
use blueprint_sdk::runner::BlueprintRunner;
use blueprint_sdk::runner::config::BlueprintEnvironment;
use blueprint_sdk::runner::tangle::config::TangleConfig;
use blueprint_sdk::tangle::consumer::TangleConsumer;
use blueprint_sdk::tangle::producer::TangleProducer;
use blueprint_sdk::{error, info};

/// Default cron schedule for `JOB_TICK`: once a minute (6-field cron with seconds).
const DEFAULT_TICK_CRON: &str = "0 * * * * *";

// `blueprint_sdk::Error` is a large enum, but the runner convention fixes `main`'s
// signature to `Result<(), blueprint_sdk::Error>` (the reference blueprints use the
// same shape), so boxing it here would only obscure the entrypoint.
#[allow(clippy::result_large_err)]
#[tokio::main]
async fn main() -> Result<(), blueprint_sdk::Error> {
    // Load .env before anything reads environment configuration.
    dotenvy::dotenv().ok();
    setup_log();

    // Load configuration from environment variables.
    let env = BlueprintEnvironment::load()?;

    // ── Registration mode: write the operator payload and exit early ──────────
    // The SDK preregistration flow polls the file we write here and forwards the
    // bytes to the manager's `onRegister`. We must NOT connect to the network or
    // build the runner in this mode.
    if env.registration_mode() {
        let max_capacity = std::env::var("OPERATOR_MAX_CAPACITY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8u32);
        let api_endpoint = std::env::var("OPERATOR_API_ENDPOINT").unwrap_or_default();
        let supported_scorers = std::env::var("SUPPORTED_SCORERS").unwrap_or_default();
        let payload = autoresearch_competitions_lib::competitions_registration_payload(
            max_capacity,
            &api_endpoint,
            &supported_scorers,
        );
        let path = blueprint_sdk::registration::write_registration_inputs(&env, payload)
            .await
            .map_err(|e| blueprint_sdk::Error::Other(e.to_string()))?;
        info!(
            "Wrote autoresearch-competitions registration payload to {}",
            path.display()
        );
        return Ok(());
    }

    // Connect to the Tangle network.
    let tangle_client = env
        .tangle_client()
        .await
        .map_err(|e| blueprint_sdk::Error::Other(e.to_string()))?;

    // Get service ID from protocol settings.
    let service_id = env
        .protocol_settings
        .tangle()
        .map_err(|e| blueprint_sdk::Error::Other(e.to_string()))?
        .service_id
        .ok_or_else(|| blueprint_sdk::Error::Other("SERVICE_ID missing".into()))?;

    // Load the economic / payment configuration (gate defaults, stake floor, fee
    // split, reward shape, x402 per-job pricing). `from_env` fails closed per-field
    // so the loaded config is internally runnable; we then `validate` it as the
    // explicit, fail-loud deployment contract and refuse to start on a nonsensical
    // economic state (e.g. a powerless gate, a zero anti-spam bond, or a fee split
    // that does not sum to 100). This is the floor the whole promotion + payout
    // mechanism rests on, so it is a hard error, not a warning.
    let economic = EconomicConfig::from_env();
    economic.validate().map_err(|e| {
        blueprint_sdk::Error::Other(format!(
            "invalid economic configuration: {e}. Fix the offending env var \
             (see OPERATORS.md) and restart."
        ))
    })?;
    info!(
        "Starting autoresearch-competitions blueprint for service {service_id} \
         (gate: min_lift_ci_lower={:.4} min_n={}; stake_floor_wei={}; \
         fee_split operator/referee/validator={}/{}/{}; reward={:?})",
        economic.gate.min_lift_ci_lower,
        economic.gate.min_n,
        economic.min_stake_wei,
        economic.fee_split.operator_pct,
        economic.fee_split.referee_pct,
        economic.fee_split.validator_pct,
        economic.default_reward_shape,
    );

    // Create producer (listens for JobSubmitted events) and consumer (submits results).
    let tangle_producer = TangleProducer::new(tangle_client.clone(), service_id);
    let tangle_consumer = TangleConsumer::new(tangle_client);

    // Registration inputs are also threaded into the TangleConfig so an operator
    // joining a Dynamic-membership service advertises its capacity on registration.
    let tangle_config = {
        let mut config = TangleConfig::default();
        if let Ok(cap_str) = std::env::var("OPERATOR_MAX_CAPACITY")
            && let Ok(capacity) = cap_str.parse::<u32>()
        {
            let api_endpoint = std::env::var("OPERATOR_API_ENDPOINT").unwrap_or_default();
            let supported_scorers = std::env::var("SUPPORTED_SCORERS").unwrap_or_default();
            let inputs = autoresearch_competitions_lib::competitions_registration_payload(
                capacity,
                &api_endpoint,
                &supported_scorers,
            );
            config = config.with_registration_inputs(inputs);
        }
        config
    };

    // Cron producer: fire JOB_TICK on a schedule so deadlines + continuous epochs
    // advance without an external poker. The tick handler is bodyless, so the
    // cron's empty-body JobCall routes cleanly.
    let cron_schedule =
        std::env::var("WORKFLOW_CRON_SCHEDULE").unwrap_or_else(|_| DEFAULT_TICK_CRON.to_string());
    let tick_cron = CronJob::new(JOB_TICK, cron_schedule.as_str())
        .await
        .map_err(|e| blueprint_sdk::Error::Other(format!("Invalid tick cron schedule: {e}")))?;
    info!("Tick cron scheduled: \"{cron_schedule}\"");

    // Build and run the blueprint.
    let result = BlueprintRunner::builder(tangle_config, env)
        .router(router())
        .producer(tangle_producer)
        .producer(tick_cron)
        .consumer(tangle_consumer)
        .with_shutdown_handler(async {
            info!("Shutting down autoresearch-competitions blueprint");
        })
        .run()
        .await;

    if let Err(e) = result {
        error!("Runner failed: {e:?}");
    }

    Ok(())
}

fn setup_log() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::from_default_env();
    fmt().with_env_filter(filter).init();
}
