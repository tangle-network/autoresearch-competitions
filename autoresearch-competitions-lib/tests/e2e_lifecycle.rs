//! Full-lifecycle devnet E2E for the autoresearch-competitions blueprint.
//!
//! This is the centerpiece M7 proof: unlike the unit/forge suites that exercise
//! the mechanism in isolation, this drives the blueprint as a LIVE Tangle AVS on a
//! local anvil devnet using the `blueprint-anvil-testing-utils` harness. The
//! harness (a) boots anvil with the full seeded tnt-core stack, (b) registers the
//! blueprint + an active service, (c) seeds an honest operator fleet that runs the
//! real `BlueprintRunner` with our [`router`], then we (d) submit the real
//! competition jobs as on-chain `JobSubmitted` events and (e) assert each routes
//! to its handler and returns the expected ABI-encoded operator result.
//!
//! ## Running it
//!
//! It is gated behind `#[ignore]` because it needs Docker + the seeded foundry
//! anvil image and takes minutes, so the fast `nextest` suite stays deterministic.
//! Run it explicitly:
//!
//! ```bash
//! cargo test -p autoresearch-competitions-lib --test e2e_lifecycle -- --ignored --nocapture
//! ```
//!
//! ## What runs green vs. what needs a live node
//!
//! The harness deploys + registers + requests a service + runs the operator
//! runtime entirely on local anvil — so the deploy → register → service-request →
//! operator-provision → JobSubmitted → router → onJobResult roundtrip all execute
//! against a real chain here. If the seeded broadcast/Docker image is unavailable
//! in an environment, `BlueprintHarness::spawn` fails with a missing-artifacts
//! error; we surface that as an explicit skip rather than a false pass.
//!
//! ## What this test proves — and what it does NOT
//!
//! - Each job's submission leg is real: it is submitted as an on-chain
//!   `JobSubmitted` event, routed to its handler by the live `BlueprintRunner`, and
//!   the operator runtime's `TangleConsumer` produces a result. Results are
//!   **observed via the harness's in-process consumer**
//!   ([`BlueprintHarness::wait_for_job_result_with_deadline`] surfaces the result
//!   from the harness's local result queue). The on-chain `JobResultSubmitted`
//!   submission leg is **exercised by the live runner but not asserted here**: in
//!   this seeded-anvil harness the local queue is the observable result path, and
//!   the on-chain-only wait ([`BlueprintHarness::wait_for_job_result_on_chain_with_deadline`])
//!   does not surface a matching `JobResultSubmitted` for `service_id=0` within the
//!   deadline. So these assertions verify deploy → register → service-request →
//!   provision → JobSubmitted → router → handler-output; they do NOT assert the
//!   on-chain `onJobResult` decode.
//! - `SETTLE` is an explicit M0 stub: the handler does NO ranking, escrow, or
//!   payout — it returns `status="stub:settlement-pending"` with `payouts_json="[]"`.
//!   This test asserts that honest stub contract; it does NOT prove any settlement,
//!   payout, or on-chain reward state occurred. Real settlement (and an on-chain
//!   read of `CompetitionManager` payout/escrow state) lands in M1.

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::SolValue;
use anyhow::Result;
use blueprint_anvil_testing_utils::{BlueprintHarness, missing_tnt_core_artifacts};
use std::time::Duration;

use autoresearch_competitions_lib::{
    CommitCandidateRequest, CreateCompetitionRequest, CreateCompetitionResponse,
    JOB_COMMIT_CANDIDATE, JOB_CREATE_COMPETITION, JOB_REPORT_SCORE, JOB_REVEAL_CANDIDATE,
    JOB_SETTLE, JobAck, ReportScoreRequest, RevealCandidateRequest, SettleRequest, SettleResponse,
    router,
};

const JOB_RESULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Spawn the lifecycle harness for our router, returning `None` (graceful skip)
/// when the seeded tnt-core artifacts / Docker image are unavailable.
async fn spawn() -> Result<Option<BlueprintHarness>> {
    match BlueprintHarness::builder(router())
        .poll_interval(Duration::from_millis(200))
        .spawn()
        .await
    {
        Ok(harness) => Ok(Some(harness)),
        Err(err) if missing_tnt_core_artifacts(&err) => {
            eprintln!("[skip] seeded tnt-core artifacts / anvil image unavailable: {err}");
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Docker + seeded anvil image; run with --ignored"]
async fn full_competition_lifecycle_on_devnet() -> Result<()> {
    let Some(harness) = spawn().await? else {
        return Ok(());
    };

    eprintln!(
        "[setup] harness up: service_id={} blueprint_id={} caller={}",
        harness.service_id(),
        harness.blueprint_id(),
        harness.caller_account(),
    );

    // ── (a) CREATE_COMPETITION (job 0) ───────────────────────────────────────
    // A coherent Competitive + OneShot competition is accepted by the handler.
    let create_payload = CreateCompetitionRequest {
        spec_ref: "ipfs://spec-cid".into(),
        reward_pool_wei: U256::from(1_000_000_000_000_000_000u128),
        reward_asset: Default::default(),
        deadline: 1_000_000,
        structure: 0,   // Competitive
        cadence: 0,     // OneShot
        visibility: 0,  // Public
        scorer_kind: 0, // HeldOutEval
    }
    .abi_encode();

    let sub = harness
        .submit_job(JOB_CREATE_COMPETITION, Bytes::from(create_payload))
        .await?;
    let out = harness
        .wait_for_job_result_with_deadline(sub, JOB_RESULT_TIMEOUT)
        .await?;
    let created = CreateCompetitionResponse::abi_decode(&out)?;
    eprintln!("[job0] create -> status={}", created.status);
    assert_eq!(created.status, "accepted", "coherent knobs accepted");

    // ── (b) COMMIT_CANDIDATE (job 2) ─────────────────────────────────────────
    let artifact_ref = "ipfs://candidate-cid";
    let salt = alloy_primitives::FixedBytes::<32>::from([7u8; 32]);
    let commitment = alloy_primitives::keccak256((artifact_ref.to_string(), salt).abi_encode());
    let commit_payload = CommitCandidateRequest {
        competition_id: 1,
        commitment,
    }
    .abi_encode();
    let sub = harness
        .submit_job(JOB_COMMIT_CANDIDATE, Bytes::from(commit_payload))
        .await?;
    let out = harness
        .wait_for_job_result_with_deadline(sub, JOB_RESULT_TIMEOUT)
        .await?;
    let ack = JobAck::abi_decode(&out)?;
    eprintln!(
        "[job2] commit -> status={} detail={}",
        ack.status, ack.detail
    );
    assert_eq!(ack.status, "committed");

    // ── (c) REVEAL_CANDIDATE (job 3) ─────────────────────────────────────────
    let reveal_payload = RevealCandidateRequest {
        competition_id: 1,
        commitment,
        artifact_ref: artifact_ref.into(),
        salt,
    }
    .abi_encode();
    let sub = harness
        .submit_job(JOB_REVEAL_CANDIDATE, Bytes::from(reveal_payload))
        .await?;
    let out = harness
        .wait_for_job_result_with_deadline(sub, JOB_RESULT_TIMEOUT)
        .await?;
    let ack = JobAck::abi_decode(&out)?;
    eprintln!(
        "[job3] reveal -> status={} detail={}",
        ack.status, ack.detail
    );
    assert_eq!(ack.status, "revealed");

    // ── (d) REPORT_SCORE (job 4) ─────────────────────────────────────────────
    // A lift that clears the promotion gate (lower CI bound >= 0.02, n >= 12).
    let report_payload = ReportScoreRequest {
        competition_id: 1,
        candidate_id: "cand-1".into(),
        lift_delta: "0.10".into(),
        lift_ci_lower: "0.05".into(),
        lift_ci_upper: "0.15".into(),
        n: 16,
        cost: "1.0".into(),
        attestation_hash: String::new(),
    }
    .abi_encode();
    let sub = harness
        .submit_job(JOB_REPORT_SCORE, Bytes::from(report_payload))
        .await?;
    // Observed via the harness's in-process consumer (see the module doc): the
    // operator's `TangleConsumer` produces the result and the harness surfaces it
    // through its local result queue. The on-chain `JobResultSubmitted` submission
    // leg is exercised by the live runner but is NOT asserted here, because in this
    // seeded-anvil harness the local queue is the observable result path (the
    // on-chain-only wait does not surface a matching `JobResultSubmitted` for
    // `service_id=0` within the deadline). When the harness exposes a reliable
    // on-chain result for this setup, switch this one leg to
    // `wait_for_job_result_on_chain_with_deadline` to assert the full on-chain
    // roundtrip.
    let out = harness
        .wait_for_job_result_with_deadline(sub, JOB_RESULT_TIMEOUT)
        .await?;
    let ack = JobAck::abi_decode(&out)?;
    eprintln!(
        "[job4] report -> status={} detail={}",
        ack.status, ack.detail
    );
    assert_eq!(ack.status, "scored");
    assert!(
        ack.detail.contains("gate_clears=true"),
        "reported lift clears the promotion gate, got: {}",
        ack.detail
    );

    // ── (e) SETTLE (job 5) ───────────────────────────────────────────────────
    let settle_payload = SettleRequest { competition_id: 1 }.abi_encode();
    let sub = harness
        .submit_job(JOB_SETTLE, Bytes::from(settle_payload))
        .await?;
    let out = harness
        .wait_for_job_result_with_deadline(sub, JOB_RESULT_TIMEOUT)
        .await?;
    let settled = SettleResponse::abi_decode(&out)?;
    eprintln!(
        "[job5] settle -> competition_id={} status={} payouts={}",
        settled.competition_id, settled.status, settled.payouts_json
    );
    // SETTLE is an explicit M0 stub. Assert its honest stub contract rather than a
    // tautology: the handler echoes `competition_id`, so `== 1` alone could never
    // fail. These assertions document the not-yet-real state — no ranking, escrow,
    // or payout happened — and will force an intentional update when M1 lands real
    // settlement (status changes off "stub:settlement-pending"; payouts populate).
    assert_eq!(settled.competition_id, 1);
    assert_eq!(
        settled.status, "stub:settlement-pending",
        "SETTLE is a stub in M0; real ranking/escrow/payout lands in M1"
    );
    assert_eq!(
        settled.payouts_json, "[]",
        "M0 SETTLE pays out nothing (empty payout set)"
    );

    eprintln!(
        "[done] lifecycle drove through the live devnet AVS; results observed via the harness \
         in-process consumer, SETTLE asserted as the M0 stub (no real settlement yet)"
    );
    harness.shutdown().await;
    Ok(())
}
