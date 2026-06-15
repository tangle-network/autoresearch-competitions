//! End-to-end proof of the M2 trust-minimization layer on top of the real M1 run.
//!
//! This runs the same `Competitive x OneShot x Public x HeldOutEval` competition the
//! M1 e2e test runs, then disputes the winning candidate two ways through the real
//! dispute machinery:
//!
//! 1. A **frivolous (Byzantine) challenge** against the genuine winner. The honest
//!    m-of-n committee re-runs the deterministic Scorer and agrees the score stands;
//!    one Byzantine validator's lie is a tolerated minority. Outcome: `Upheld` →
//!    the challenger is slashed (challenging an honest score is -EV).
//! 2. A **legitimate challenge against a fabricated score**. A score that fraudulently
//!    claims a candidate clears (when re-scoring shows it does not) is overturned by
//!    the honest majority despite one colluding Byzantine validator. Outcome:
//!    `Overturned` → the researcher is slashed and the challenger is rewarded.
//!
//! Everything is measured through the real deterministic Scorer — no mocked lift. The
//! only injected element is the Byzantine validator's verdict, which is exactly the
//! adversary the m-of-n committee is designed to tolerate. Conservation of stake is
//! asserted in both directions.

use autoresearch_protocol::dispute::{ValidatorVerdict, collect_verdicts, committee_verdict};
use autoresearch_protocol::slash::{SlashPolicy, resolve_dispute};
use autoresearch_protocol::{
    CompetitionConfig, DisputeOutcome, ResearcherRun, run_oneshot_competitive,
};
use autoresearch_runtime::reward::RewardSchedule;
use autoresearch_runtime::traits::{Engine, EngineContext};
use autoresearch_runtime::types::{
    ArtifactRef, Cadence, Gate, Knobs, Lift, ScorerKind, Structure, Visibility,
};
use autoresearch_verticals::{ConfigArtifact, ConfigSurface, LinearScorer, LocalSearchEngine};

const POOL_WEI: u128 = 1_000_000;
const RESEARCHER_STAKE: u128 = 5_000;
const CHALLENGER_STAKE: u128 = 500;

fn knobs() -> Knobs {
    Knobs {
        structure: Structure::Competitive,
        cadence: Cadence::OneShot,
        visibility: Visibility::Public,
        scorer_kind: ScorerKind::HeldOutEval,
    }
}

fn validators() -> Vec<String> {
    // Default 2-of-3 committee (MECHANISM.md §7.2).
    vec!["0xval1".into(), "0xval2".into(), "0xval3".into()]
}

fn slash_policy() -> SlashPolicy {
    // Challenger earns 30% of a slashed researcher stake; remainder to validators/burn.
    SlashPolicy {
        challenger_reward_bps: 3_000,
        burn_remainder: true,
    }
}

/// Reconstruct the winner's actual candidate artifact by re-running their engine.
/// The engine is deterministic per seed, so this is the exact artifact that won.
async fn winning_candidate(seed: u64) -> ConfigArtifact {
    let engine = LocalSearchEngine::new(seed);
    let ctx = EngineContext {
        competition: 1,
        baseline_ref: ArtifactRef("baseline".into()),
        dev_split_ref: Some(ArtifactRef("dev".into())),
        budget_wei: POOL_WEI,
        egress_policy: None,
    };
    engine
        .produce(&ctx)
        .await
        .expect("engine produces a candidate")
}

#[tokio::test]
async fn frivolous_challenge_is_upheld_and_slashes_challenger() {
    let surface = ConfigSurface;
    let scorer = LinearScorer::new();
    let baseline = ConfigArtifact::baseline();
    let gate = Gate::default();

    // 1. Run the real M1 competition.
    let researchers: Vec<ResearcherRun> = (1u64..=5)
        .map(|seed| ResearcherRun {
            researcher: format!("0xresearcher{seed}"),
            seed,
        })
        .collect();
    let cfg = CompetitionConfig {
        id: 1,
        gate,
        reward: RewardSchedule::SnapshotTopK {
            weights_bps: vec![5_000, 3_000, 2_000],
        },
        reward_pool_wei: POOL_WEI,
        knobs: knobs(),
    };
    let outcome =
        run_oneshot_competitive(&cfg, &surface, &scorer, &baseline, &researchers, |run| {
            LocalSearchEngine::new(run.seed)
        })
        .await
        .expect("competition runs");
    assert!(outcome.winners >= 1, "need a winner to dispute");

    // Identify the winning researcher and reconstruct their actual candidate.
    let winner_id = &outcome.ranked[0].0;
    let winner_seed: u64 = winner_id
        .trim_start_matches("0xresearcher")
        .parse()
        .expect("seed parses from researcher id");
    let candidate = winning_candidate(winner_seed).await;

    // 2. A challenger frivolously disputes the genuine winner. The committee re-scores.
    let verdicts = collect_verdicts(
        &scorer,
        &surface,
        &candidate,
        &baseline,
        &gate,
        &validators(),
    )
    .await
    .expect("committee re-scores");
    // The original certification said the winner clears (it really does).
    let original_clears = true;
    // Every honest validator agrees with reality; inject ONE Byzantine liar.
    assert!(
        verdicts.iter().all(|v| v.clears),
        "honest re-score of a real winner must clear: {verdicts:?}"
    );
    let mut tampered = verdicts.clone();
    tampered[2] = ValidatorVerdict {
        validator: "0xbyzantine".into(),
        clears: false, // lies that the winner does not clear
        lift: Lift {
            delta: 0.0,
            ci_lower: -0.05,
            ci_upper: 0.05,
            n: 80,
        },
    };

    let decision = committee_verdict(original_clears, &tampered, 2);
    assert_eq!(
        decision,
        DisputeOutcome::Upheld,
        "a frivolous challenge against a real winner, with one Byzantine vote, must be upheld"
    );

    // 3. The wrong challenger is slashed; the researcher is made whole.
    let resolution = resolve_dispute(
        decision,
        RESEARCHER_STAKE,
        CHALLENGER_STAKE,
        &slash_policy(),
    );
    assert_eq!(
        resolution.challenger_slashed, CHALLENGER_STAKE,
        "challenger loses stake"
    );
    assert_eq!(
        resolution.researcher_slashed, 0,
        "honest researcher is not slashed"
    );
    assert_eq!(resolution.researcher_refund, RESEARCHER_STAKE);
    assert!(
        resolution.conserves(RESEARCHER_STAKE, CHALLENGER_STAKE),
        "Upheld dispute must conserve stake: {resolution:?}"
    );
}

#[tokio::test]
async fn legit_challenge_on_fabricated_score_is_overturned_and_slashes_researcher() {
    let surface = ConfigSurface;
    let scorer = LinearScorer::new();
    let baseline = ConfigArtifact::baseline();
    let gate = Gate::default();

    // A fabricated certification: a Referee/researcher claims the BASELINE clears the
    // gate (it does not — zero lift over itself). Re-scoring exposes the fraud.
    let fabricated_candidate = ConfigArtifact::baseline();
    let original_clears = true; // the fraudulent claim

    let verdicts = collect_verdicts(
        &scorer,
        &surface,
        &fabricated_candidate,
        &baseline,
        &gate,
        &validators(),
    )
    .await
    .expect("committee re-scores");
    // Honest re-score: baseline-vs-baseline is zero lift, so no honest validator clears.
    assert!(
        verdicts.iter().all(|v| !v.clears),
        "honest re-score of a fabricated (zero-lift) score must not clear: {verdicts:?}"
    );

    // A colluding Byzantine validator props up the fraud (votes that it clears).
    let mut tampered = verdicts.clone();
    tampered[2] = ValidatorVerdict {
        validator: "0xbyzantine".into(),
        clears: true, // colludes with the fabricated score
        lift: Lift {
            delta: 0.40,
            ci_lower: 0.35,
            ci_upper: 0.45,
            n: 80,
        },
    };

    let decision = committee_verdict(original_clears, &tampered, 2);
    assert_eq!(
        decision,
        DisputeOutcome::Overturned,
        "a legitimate challenge against a fabricated score must overturn it despite a Byzantine ally"
    );

    // The cheating researcher is slashed in full; the honest challenger is rewarded
    // and refunded. Conservation holds.
    let resolution = resolve_dispute(
        decision,
        RESEARCHER_STAKE,
        CHALLENGER_STAKE,
        &slash_policy(),
    );
    assert_eq!(
        resolution.researcher_slashed, RESEARCHER_STAKE,
        "fraudster loses full stake"
    );
    assert_eq!(
        resolution.challenger_slashed, 0,
        "honest challenger not slashed"
    );
    assert_eq!(
        resolution.challenger_refund, CHALLENGER_STAKE,
        "challenger stake refunded"
    );
    assert!(
        resolution.challenger_reward > 0,
        "successful challenge must be +EV: {resolution:?}"
    );
    // 30% of 5_000 = 1_500 reward, 3_500 to validators/burn.
    assert_eq!(resolution.challenger_reward, 1_500);
    assert_eq!(resolution.burned, 3_500);
    assert!(
        resolution.conserves(RESEARCHER_STAKE, CHALLENGER_STAKE),
        "Overturned dispute must conserve stake: {resolution:?}"
    );
}
