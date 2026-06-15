//! On-chain job ABI and handlers for the autoresearch-competitions blueprint.
//!
//! These jobs are the thin settlement/commitment spine of the market. All heavy
//! work — running researcher engines, replaying held-out evals — happens
//! off-chain in sandboxes and the Referee; the jobs here only move commitments,
//! certified scores, attestation hashes, and payouts. See `SPEC.md §7` for the
//! canonical job table and `docs/ARCHITECTURE.md` for the on-chain/off-chain split.
//!
//! M0 status: job ABI + router are wired and the seams to [`autoresearch_runtime`]
//! are exercised (REPORT_SCORE evaluates the promotion gate). The bodies are
//! deliberately minimal stubs — real escrow, commit-reveal, scoring dispatch, and
//! settlement land in M1+ per `ROADMAP.md`.

use alloy_sol_types::sol;
use autoresearch_runtime::{Lift, Measurement};
use blueprint_sdk::Router;
use blueprint_sdk::macros::debug_job;
use blueprint_sdk::tangle::extract::{Caller, TangleArg, TangleResult};

pub mod config;
pub mod registration;

pub use config::{DefaultRewardShape, EconomicConfig, FeeSplit};
pub use registration::competitions_registration_payload;

// Job IDs. MUST match the `uint8` constants in `contracts/src/CompetitionManager.sol`.
pub const JOB_CREATE_COMPETITION: u8 = 0;
pub const JOB_JOIN: u8 = 1;
pub const JOB_COMMIT_CANDIDATE: u8 = 2;
pub const JOB_REVEAL_CANDIDATE: u8 = 3;
pub const JOB_REPORT_SCORE: u8 = 4;
pub const JOB_SETTLE: u8 = 5;
pub const JOB_CHALLENGE: u8 = 6;
pub const JOB_TICK: u8 = 7;

sol! {
    /// `CREATE_COMPETITION` — a proposer opens a competition and escrows the reward.
    /// The four `uint8` knob fields encode the SPEC §4 model; the heavy spec
    /// (scorer, surface, gate) stays off-chain behind `spec_ref`.
    struct CreateCompetitionRequest {
        string spec_ref;          // sealed/hashed CompetitionSpec reference (off-chain)
        uint256 reward_pool_wei;
        address reward_asset;     // zero address = native
        uint64 deadline;
        uint8 structure;          // 0 Competitive, 1 Collaborative
        uint8 cadence;            // 0 OneShot, 1 Continuous
        uint8 visibility;         // 0 Public, 1 Private
        uint8 scorer_kind;        // 0 HeldOutEval, 1 PrivateOracle, 2 PrivilegedHardware, 3 HumanPanel
    }

    struct CreateCompetitionResponse {
        uint64 competition_id;
        string status;
    }

    /// `JOIN` — a researcher registers and posts stake.
    struct JoinRequest {
        uint64 competition_id;
        uint256 stake_wei;
    }

    /// `COMMIT_CANDIDATE` — commit phase of commit-reveal (anti-copy).
    struct CommitCandidateRequest {
        uint64 competition_id;
        bytes32 commitment;       // keccak256(abi.encode(artifact_ref, salt))
    }

    /// `REVEAL_CANDIDATE` — reveal phase; the contract checks the hash matches.
    /// `salt` is a fixed-width `bytes32` (canonical across the EOA and Tangle reveal
    /// paths) and the commitment is `keccak256(abi.encode(artifact_ref, salt))`, which
    /// is length-prefixed and collision-free for the dynamic `artifact_ref`.
    struct RevealCandidateRequest {
        uint64 competition_id;
        bytes32 commitment;
        string artifact_ref;      // sealed reference to the candidate artifact
        bytes32 salt;
    }

    /// `REPORT_SCORE` — the Referee commits a certified result. Lift figures are
    /// carried as decimal strings to keep the ABI free of signed-int edge cases;
    /// the handler parses them and evaluates the promotion gate.
    struct ReportScoreRequest {
        uint64 competition_id;
        string candidate_id;
        string lift_delta;        // decimal, e.g. "0.189"
        string lift_ci_lower;     // decimal
        string lift_ci_upper;     // decimal
        uint32 n;                 // paired episodes
        string cost;              // decimal total scoring cost
        string attestation_hash;  // hex keccak of the TEE attestation (empty if none)
    }

    /// `SETTLE` / `FINALIZE` — rank and pay out per the RewardSchedule.
    struct SettleRequest {
        uint64 competition_id;
    }

    struct SettleResponse {
        uint64 competition_id;
        string payouts_json;
        string status;
    }

    /// `CHALLENGE` — a staked dispute of a reported score, triggering re-score.
    struct ChallengeRequest {
        uint64 competition_id;
        string candidate_id;
        uint256 stake_wei;
    }

    /// `TICK` — cron-driven: deadline enforcement, continuous-epoch settlement.
    struct TickRequest {
        uint64 competition_id;
    }

    /// Generic acknowledgement for jobs whose result is recorded on-chain.
    struct JobAck {
        string status;
        string detail;
    }
}

/// `CREATE_COMPETITION` (M0 stub): validates knob coherence and acknowledges.
/// Real id allocation + escrow happen in `CompetitionManager.onJobResult`.
#[debug_job]
pub async fn create_competition(
    Caller(_proposer): Caller,
    TangleArg(req): TangleArg<CreateCompetitionRequest>,
) -> TangleResult<CreateCompetitionResponse> {
    let coherent = knobs_coherent(req.structure, req.cadence);
    let status = if coherent {
        "accepted"
    } else {
        "rejected:incoherent-knobs"
    };
    TangleResult(CreateCompetitionResponse {
        competition_id: 0,
        status: status.to_string(),
    })
}

/// `JOIN` (M0 stub).
#[debug_job]
pub async fn join(
    Caller(_researcher): Caller,
    TangleArg(req): TangleArg<JoinRequest>,
) -> TangleResult<JobAck> {
    ack("joined", &format!("competition={}", req.competition_id))
}

/// `COMMIT_CANDIDATE` (M0 stub).
#[debug_job]
pub async fn commit_candidate(
    Caller(_researcher): Caller,
    TangleArg(req): TangleArg<CommitCandidateRequest>,
) -> TangleResult<JobAck> {
    ack("committed", &format!("competition={}", req.competition_id))
}

/// `REVEAL_CANDIDATE` (M0 stub).
#[debug_job]
pub async fn reveal_candidate(
    Caller(_researcher): Caller,
    TangleArg(req): TangleArg<RevealCandidateRequest>,
) -> TangleResult<JobAck> {
    ack("revealed", &format!("artifact={}", req.artifact_ref))
}

/// `REPORT_SCORE` (M0): genuinely evaluates the promotion gate against the
/// reported lift using [`autoresearch_runtime`]. This is the first real seam
/// between the on-chain ABI and the core domain logic.
///
/// The gate applied is the operator's *configured* gate
/// ([`EconomicConfig::from_env`]), not `Gate::default()`: an operator who tunes
/// `GATE_MIN_N` / `GATE_MIN_LIFT_CI_LOWER` must see that gate enforced at the
/// payout-eligibility decision, otherwise the configured gate would be a no-op.
/// `from_env` fails closed (a nonsensical `GATE_MIN_N=0` override loads the
/// default gate, not a powerless one), so the gate used here is always sane.
#[debug_job]
pub async fn report_score(
    Caller(_referee): Caller,
    TangleArg(req): TangleArg<ReportScoreRequest>,
) -> TangleResult<JobAck> {
    let delta = parse_f64(&req.lift_delta);
    let ci_lower = parse_f64(&req.lift_ci_lower);
    let ci_upper = parse_f64(&req.lift_ci_upper);
    let cost = parse_f64(&req.cost);

    let lift = Lift {
        delta,
        ci_lower,
        ci_upper,
        n: req.n,
    };
    let measurement = Measurement {
        value: delta,
        ci_lower,
        ci_upper,
        n: req.n,
        cost,
    };
    let gate = EconomicConfig::from_env().gate;
    let clears = gate.clears(&lift, &measurement);

    ack(
        "scored",
        &format!("candidate={} gate_clears={}", req.candidate_id, clears),
    )
}

/// `SETTLE` (M0 stub): returns an empty payout set. Real ranking + RewardSchedule
/// settlement (via `autoresearch_runtime::reward`) land in M1.
#[debug_job]
pub async fn settle(
    Caller(_caller): Caller,
    TangleArg(req): TangleArg<SettleRequest>,
) -> TangleResult<SettleResponse> {
    TangleResult(SettleResponse {
        competition_id: req.competition_id,
        payouts_json: "[]".to_string(),
        status: "stub:settlement-pending".to_string(),
    })
}

/// `CHALLENGE` (M0 stub).
#[debug_job]
pub async fn challenge(
    Caller(_challenger): Caller,
    TangleArg(req): TangleArg<ChallengeRequest>,
) -> TangleResult<JobAck> {
    ack(
        "challenge-opened",
        &format!("candidate={}", req.candidate_id),
    )
}

/// `TICK` — cron-driven deadline / continuous-epoch hook.
///
/// Takes NO job-call extractors. The cron producer
/// ([`blueprint_producers_extra::cron::CronJob`]) fires this job on a schedule
/// with an **empty body and no metadata** (no `Caller`, no ABI args), so the
/// handler must be extractor-free to decode that call. A tick is global: it
/// sweeps every live competition for an expired deadline or an elapsed continuous
/// epoch, rather than targeting one `competition_id`. The on-chain `TickRequest`
/// ABI struct is retained for the contract-side job table and for a future
/// targeted-tick path, but the cron upkeep path is intentionally bodyless.
#[debug_job]
pub async fn tick() -> TangleResult<JobAck> {
    ack("tick", "swept live competitions for deadlines/epochs")
}

/// Router mapping job IDs to handlers. Jobs arrive as native Tangle calls, so no
/// metadata-compat layer is needed (cf. the trading blueprint's EVM bridge).
pub fn router() -> Router {
    Router::new()
        .route(JOB_CREATE_COMPETITION, create_competition)
        .route(JOB_JOIN, join)
        .route(JOB_COMMIT_CANDIDATE, commit_candidate)
        .route(JOB_REVEAL_CANDIDATE, reveal_candidate)
        .route(JOB_REPORT_SCORE, report_score)
        .route(JOB_SETTLE, settle)
        .route(JOB_CHALLENGE, challenge)
        .route(JOB_TICK, tick)
}

// --- helpers ---------------------------------------------------------------

fn ack(status: &str, detail: &str) -> TangleResult<JobAck> {
    TangleResult(JobAck {
        status: status.to_string(),
        detail: detail.to_string(),
    })
}

fn parse_f64(s: &str) -> f64 {
    s.trim().parse::<f64>().unwrap_or(0.0)
}

/// Structure×Cadence coherence (mirrors `autoresearch_runtime::Knobs::validate`):
/// Collaborative competitions are OneShot only.
fn knobs_coherent(structure: u8, cadence: u8) -> bool {
    // structure: 1 = Collaborative; cadence: 1 = Continuous.
    !(structure == 1 && cadence == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller() -> Caller {
        Caller([0u8; 20])
    }

    #[tokio::test]
    async fn create_rejects_incoherent_knobs() {
        // Collaborative (1) + Continuous (1) is incoherent.
        let req = CreateCompetitionRequest {
            spec_ref: "ref".into(),
            reward_pool_wei: alloy_primitives::U256::from(1u64),
            reward_asset: Default::default(),
            deadline: 0,
            structure: 1,
            cadence: 1,
            visibility: 0,
            scorer_kind: 0,
        };
        let res = create_competition(caller(), TangleArg(req)).await;
        assert!(res.0.status.starts_with("rejected"));
    }

    #[tokio::test]
    async fn report_score_clears_gate_with_sufficient_lift_and_power() {
        let req = ReportScoreRequest {
            competition_id: 1,
            candidate_id: "cand-1".into(),
            lift_delta: "0.10".into(),
            lift_ci_lower: "0.05".into(),
            lift_ci_upper: "0.15".into(),
            n: 16,
            cost: "1.0".into(),
            attestation_hash: String::new(),
        };
        let res = report_score(caller(), TangleArg(req)).await;
        assert!(res.0.detail.contains("gate_clears=true"));
    }

    #[tokio::test]
    async fn report_score_fails_gate_with_low_power() {
        let req = ReportScoreRequest {
            competition_id: 1,
            candidate_id: "cand-2".into(),
            lift_delta: "0.10".into(),
            lift_ci_lower: "0.05".into(),
            lift_ci_upper: "0.15".into(),
            n: 5, // below Gate::default().min_n (12)
            cost: "1.0".into(),
            attestation_hash: String::new(),
        };
        let res = report_score(caller(), TangleArg(req)).await;
        assert!(res.0.detail.contains("gate_clears=false"));
    }
}
