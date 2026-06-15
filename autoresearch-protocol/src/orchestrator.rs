//! The one-shot competitive runner.
//!
//! [`run_oneshot_competitive`] is the off-chain Referee + market-maker loop for a
//! `Competitive x OneShot x Public x HeldOutEval` competition. It is generic over
//! the three seams — [`Surface`], [`Scorer`], [`Engine`] — so the same loop drives
//! the M1 demo vertical and the production agent-eval vertical without change.
//!
//! Flow:
//! 1. Score the baseline once on [`Split::HeldOut`] (the bar everyone is measured against).
//! 2. For each researcher: build their engine, let it `produce` a candidate against
//!    the dev split, validate it on the surface, then score it on the held-out split.
//! 3. Estimate each candidate's lift over the baseline ([`crate::lift::estimate_lift`]).
//! 4. Keep only candidates whose lift clears the competition [`Gate`].
//! 5. Rank survivors by point-estimate delta (best first).
//! 6. Settle payouts over the ranked researcher ids via the [`RewardSchedule`].
//!
//! Money is computed here off-chain; the chain only conserves and pays (see the
//! contract's `distribute`). The lift numbers this returns are real measured
//! improvements on held-out data — nothing is mocked.

use autoresearch_runtime::reward::{
    Payout, RewardError, RewardSchedule, settle_snapshot_topk, settle_terminal_prize,
};
use autoresearch_runtime::traits::{Engine, EngineContext, Scorer, Surface};
use autoresearch_runtime::types::{ArtifactRef, CompetitionId, Gate, Knobs, Lift, Split};

use crate::lift::estimate_lift;

/// Static configuration for a single competition run.
#[derive(Clone, Debug)]
pub struct CompetitionConfig {
    pub id: CompetitionId,
    pub gate: Gate,
    pub reward: RewardSchedule,
    /// Total reward pool in wei, escrowed on-chain at creation.
    pub reward_pool_wei: u128,
    pub knobs: Knobs,
}

/// One researcher's run parameters. `seed` is fed to their [`Engine`]; distinct
/// seeds yield distinct (but each independently good) searches, producing a real
/// ranking rather than a tie.
#[derive(Clone, Debug)]
pub struct ResearcherRun {
    pub researcher: String,
    pub seed: u64,
}

/// The result of a one-shot competitive run.
#[derive(Clone, Debug)]
pub struct CompetitionOutcome {
    /// Gate-clearing researchers paired with their certified lift, best delta first.
    pub ranked: Vec<(String, Lift)>,
    /// Settled payouts per the [`RewardSchedule`]. Conserves the reward pool.
    pub payouts: Vec<Payout>,
    /// Count of candidates that cleared the gate (== `ranked.len()`).
    pub winners: usize,
}

/// Errors from running a competition. Per-researcher engine/scoring failures abort
/// the run: in M1 the loop is in-process and a failure is a bug, not a flaky peer.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("knob combination is incoherent: {0}")]
    IncoherentKnobs(&'static str),
    #[error("surface rejected an artifact: {0}")]
    Surface(#[from] autoresearch_runtime::traits::SurfaceError),
    #[error("scorer failed: {0}")]
    Scorer(#[from] autoresearch_runtime::traits::ScorerError),
    #[error("engine failed for researcher {researcher}: {source}")]
    Engine {
        researcher: String,
        #[source]
        source: autoresearch_runtime::traits::EngineError,
    },
    #[error("the {0:?} reward schedule is not supported by the one-shot competitive runner")]
    UnsupportedReward(RewardSchedule),
    #[error("reward schedule cannot conserve the pool: {0}")]
    InvalidReward(#[from] RewardError),
    /// A privacy-layer control rejected the run (the hard rule, attestation, or
    /// egress). Surfaced by the private runner ([`crate::private`]).
    #[error("privacy control rejected the run: {0}")]
    Privacy(#[from] autoresearch_runtime::privacy::PrivacyError),
}

/// Run a `Competitive x OneShot` competition fully in-process and return the
/// certified ranking and conserving payouts.
///
/// `make_engine` builds a fresh engine per researcher from their [`ResearcherRun`];
/// this is where a real deployment injects per-researcher budget/credentials.
///
/// # Errors
/// Returns [`ProtocolError`] if the knobs are incoherent, if any candidate fails
/// surface validation or scoring, or if the configured reward schedule is not a
/// terminal one-shot shape (`SnapshotTopK` / `TerminalPrize`).
pub async fn run_oneshot_competitive<S, Sc, Eng, Mk>(
    cfg: &CompetitionConfig,
    surface: &S,
    scorer: &Sc,
    baseline: &S::Artifact,
    researchers: &[ResearcherRun],
    make_engine: Mk,
) -> Result<CompetitionOutcome, ProtocolError>
where
    S: Surface,
    Sc: Scorer<Artifact = S::Artifact>,
    Eng: Engine<Artifact = S::Artifact>,
    Mk: Fn(&ResearcherRun) -> Eng,
{
    cfg.knobs
        .validate()
        .map_err(ProtocolError::IncoherentKnobs)?;
    // Reject a reward schedule that cannot conserve the pool before doing any work:
    // an over-weighted SnapshotTopK would otherwise compute over-pool payouts that the
    // on-chain `distribute` rejects, permanently stranding the escrow.
    cfg.reward.validate()?;

    // 1. The baseline bar, measured once on the held-out split.
    let baseline_ref = surface.to_ref(baseline)?;
    let baseline_measurement = scorer.score(baseline, Split::HeldOut).await?;

    // 2-4. Produce, validate, score, and gate each researcher's candidate.
    let mut survivors: Vec<(String, Lift)> = Vec::new();
    for run in researchers {
        let engine = make_engine(run);
        let ctx = EngineContext {
            competition: cfg.id,
            baseline_ref: baseline_ref.clone(),
            // Public/HeldOutEval: researchers may hill-climb on the dev split.
            dev_split_ref: Some(ArtifactRef(format!("dev-split:{}", cfg.id))),
            budget_wei: cfg.reward_pool_wei,
            // Public competition: no proprietary data to protect, so egress is
            // unrestricted (PRIVACY §3). The private runner is what sets a policy.
            egress_policy: None,
        };

        let candidate = engine
            .produce(&ctx)
            .await
            .map_err(|source| ProtocolError::Engine {
                researcher: run.researcher.clone(),
                source,
            })?;
        surface.validate(&candidate)?;

        let measurement = scorer.score(&candidate, Split::HeldOut).await?;
        let lift = estimate_lift(&measurement, &baseline_measurement);

        if cfg.gate.clears(&lift, &measurement) {
            survivors.push((run.researcher.clone(), lift));
        }
    }

    // 5. Rank by point-estimate delta, best first. Ties broken by lower CI bound
    //    (more certain win ranks higher), then by researcher id for determinism.
    survivors.sort_by(|a, b| {
        b.1.delta
            .partial_cmp(&a.1.delta)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.1.ci_lower
                    .partial_cmp(&a.1.ci_lower)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.0.cmp(&b.0))
    });

    // 6. Settle.
    let ranked_ids: Vec<String> = survivors.iter().map(|(r, _)| r.clone()).collect();
    let payouts = settle_terminal_or_topk(&cfg.reward, cfg.reward_pool_wei, &ranked_ids)?;

    Ok(CompetitionOutcome {
        winners: survivors.len(),
        ranked: survivors,
        payouts,
    })
}

/// Dispatch the terminal one-shot reward schedules (`SnapshotTopK` / `TerminalPrize`)
/// over a best-first ranking. Continuous schedules (`RecordBounty`,
/// `TimeAtTopStreaming`) belong to the continuous runner and are rejected here rather
/// than silently mis-settled. Shared by the public ([`run_oneshot_competitive`]) and
/// private ([`crate::private::run_private_competitive`]) runners so both settle
/// identically and conserve the pool.
///
/// # Errors
/// [`ProtocolError::InvalidReward`] for an over-weighted schedule, or
/// [`ProtocolError::UnsupportedReward`] for a continuous schedule.
pub fn settle_terminal_or_topk(
    reward: &RewardSchedule,
    pool_wei: u128,
    ranked_ids: &[String],
) -> Result<Vec<Payout>, ProtocolError> {
    // Conservation guard at the settle seam too (defense in depth): never produce a
    // payout set that could exceed the escrowed pool.
    reward.validate()?;
    match reward {
        RewardSchedule::SnapshotTopK { weights_bps } => {
            Ok(settle_snapshot_topk(pool_wei, ranked_ids, weights_bps))
        }
        RewardSchedule::TerminalPrize => Ok(settle_terminal_prize(
            pool_wei,
            ranked_ids.first().map(String::as_str),
        )),
        other => Err(ProtocolError::UnsupportedReward(other.clone())),
    }
}
