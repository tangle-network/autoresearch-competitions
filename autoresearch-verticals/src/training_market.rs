//! Training-market mechanics: a *continuous* distributed-training leaderboard and
//! an *m-of-n* re-score panel, built on the Phase-0 distributed-training vertical.
//!
//! Phase 0 ([`crate::distributed_training`]) proved a *one-shot* training
//! competition: researchers submit recipes, a [`LocalSimCluster`] trains each, the
//! Referee re-scores the artifact on held-out, gates, and pays the top-k once at a
//! deadline. This module adds the two mechanics that make that market *continuous*
//! and *trust-minimized*:
//!
//! 1. [`ContinuousTrainingMarket`] — a king-of-the-hill training leaderboard.
//!    Researchers submit successively better recipes over time; each new
//!    state-of-the-art is paid its **marginal** held-out loss reduction over the
//!    current best (and nothing more), via
//!    [`autoresearch_runtime::reward::settle_record_bounty`]. The frontier is bought
//!    exactly once — a genuine improvement pays once, a non-improving resubmission
//!    pays zero — which is the property that keeps a long-running training arena
//!    moving without overpaying.
//!
//! 2. [`RescorePanel`] — an m-of-n re-score harness. `K` independent referees
//!    (each a [`DistributedTrainingScorer`] over a *different* eval-shard config)
//!    re-score the **same** [`TrainedArtifact`]. On a genuine artifact the referees
//!    agree within their confidence intervals; a divergent **self-reported** score
//!    (a cluster claiming a better held-out loss than it actually trains to) is
//!    rejected by majority, because no honest referee's CI brackets the inflated
//!    claim.
//!
//! # The honesty seam — re-scoring is what makes delegation safe
//!
//! Both mechanics rest on the same Phase-0 invariant: **delegating the compute
//! never delegates the trust.** The cluster's self-reported `train_loss` is never
//! paid on. The continuous market pays on the Referee's held-out re-score; the
//! panel goes further and requires `m` of `n` *independent* referees to corroborate
//! a held-out number before it is believed. A cluster that lies about its loss
//! moves neither the leaderboard nor the panel.
//!
//! # Determinism
//!
//! Everything here is deterministic: training is the closed-form
//! [`LocalSimCluster`] model, scoring is the seeded
//! [`DistributedTrainingScorer`], and settlement is integer micro-unit math. No
//! `rand`, no clock, no I/O — the tests assert concrete wei payouts and concrete
//! accept/reject panel verdicts.

use autoresearch_protocol::lift::estimate_lift;
use autoresearch_protocol::to_micros;
use autoresearch_runtime::reward::{Payout, RecordBeat, settle_record_bounty};
use autoresearch_runtime::types::{Gate, Lift, Measurement, Split};

use crate::distributed_training::{
    DistributedTrainingScorer, LocalSimCluster, TrainedArtifact, TrainingRecipe,
};

// --- Continuous distributed-training leaderboard ----------------------------

/// One researcher's submission to the continuous training leaderboard: a recipe to
/// train and the researcher it is credited to. The training seed is derived
/// deterministically from submission order (see [`ContinuousTrainingMarket::run`])
/// so the same submission sequence always produces the same payouts.
#[derive(Clone, Debug, PartialEq)]
pub struct RecipeSubmission {
    /// The researcher this submission is credited to.
    pub researcher: String,
    /// The training recipe to train and re-score on held-out.
    pub recipe: TrainingRecipe,
}

impl RecipeSubmission {
    /// Construct a submission.
    #[must_use]
    pub fn new(researcher: impl Into<String>, recipe: TrainingRecipe) -> Self {
        Self {
            researcher: researcher.into(),
            recipe,
        }
    }
}

/// What a single submission did to the leaderboard once trained, re-scored, and
/// gated. Carries the held-out lift over the *original baseline* (in micros) so a
/// caller can render the leaderboard, plus whether this submission set a new record
/// and the wei it was paid for its marginal.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmissionResult {
    /// The researcher this submission was credited to.
    pub researcher: String,
    /// The held-out value the Referee re-scored (`-loss`; higher is better).
    pub heldout_value: f64,
    /// The certified lift over the baseline, in integer micro-units (`1e-6` point).
    /// `None` if the submission failed the gate (noise / no real generalizing lift),
    /// in which case it can never set a record or be paid.
    pub lift_micros: Option<i64>,
    /// Whether this submission set a new state-of-the-art (cleared the gate AND beat
    /// the current best by at least `epsilon`).
    pub became_record: bool,
    /// Wei paid out *by this submission* for its marginal over the prior best.
    pub paid_wei: u128,
}

/// The full outcome of replaying a submission sequence through the continuous
/// training market.
#[derive(Clone, Debug, PartialEq)]
pub struct ContinuousMarketOutcome {
    /// Per-submission results, in submission order (the leaderboard log).
    pub results: Vec<SubmissionResult>,
    /// The marginal payouts, one per gate-clearing record beat, in order. Replaying
    /// these through [`settle_record_bounty`] reproduces them exactly — the
    /// recomputability guarantee.
    pub payouts: Vec<Payout>,
    /// The final best held-out value, or `None` if no submission ever set a record.
    pub final_best_value: Option<f64>,
    /// The final best in micro-units, or `None` if no record was set.
    pub final_best_micros: Option<i64>,
}

/// A continuous (king-of-the-hill) distributed-training leaderboard.
///
/// Researchers submit recipes over time. Each is trained by a [`LocalSimCluster`],
/// re-scored on held-out by a [`DistributedTrainingScorer`], and its lift over the
/// fixed baseline is estimated and gated. A gate-clearing submission whose held-out
/// lift beats the current best by at least `epsilon_micros` is a *record*; it is
/// paid `wei_per_micro` times its **marginal** over the prior best, exactly as
/// [`autoresearch_runtime::reward::RewardSchedule::RecordBounty`] prescribes.
///
/// The marginal-once invariant is inherited from [`settle_record_bounty`]: across a
/// monotonic record sequence the total paid is `wei_per_micro * (final_best -
/// baseline)`, so the frontier is bought exactly once and a non-improving
/// resubmission pays nothing.
#[derive(Clone, Debug)]
pub struct ContinuousTrainingMarket {
    /// The cluster that trains each submitted recipe (the local stand-in here; a
    /// real external backend drops in unchanged behind the same trait).
    cluster: LocalSimCluster,
    /// The Referee's held-out scorer. `eval_shards` should be >= the gate's `min_n`.
    scorer: DistributedTrainingScorer,
    /// The promotion gate every submission must clear to be eligible to pay.
    gate: Gate,
    /// The recipe the market measures lift against (typically
    /// [`TrainingRecipe::baseline`]).
    baseline_recipe: TrainingRecipe,
    /// The seed the baseline is trained under.
    baseline_seed: u64,
    /// `RecordBounty` minimum marginal (micros) a beat must clear to be paid.
    epsilon_micros: i64,
    /// Wei paid per micro-unit of marginal held-out lift.
    wei_per_micro: u128,
}

impl ContinuousTrainingMarket {
    /// Open a continuous training market.
    ///
    /// `eval_shards` is the Referee's held-out sample size (use >= the gate's
    /// `min_n`, i.e. 12, for results to be admissible). `epsilon_micros` is the
    /// minimum marginal a record must clear; `wei_per_micro` is the rate paid per
    /// micro-unit of marginal held-out lift.
    #[must_use]
    pub fn new(
        eval_shards: u32,
        gate: Gate,
        baseline_recipe: TrainingRecipe,
        baseline_seed: u64,
        epsilon_micros: i64,
        wei_per_micro: u128,
    ) -> Self {
        Self {
            cluster: LocalSimCluster,
            scorer: DistributedTrainingScorer::new(eval_shards),
            gate,
            baseline_recipe,
            baseline_seed,
            epsilon_micros,
            wei_per_micro,
        }
    }

    /// The baseline's certified held-out measurement (lift is measured against this).
    fn baseline_measurement(&self) -> Measurement {
        let baseline = self
            .cluster
            .train_sync(&self.baseline_recipe, self.baseline_seed);
        self.scorer.score_sync(&baseline, Split::HeldOut)
    }

    /// Train, re-score, and gate a single submission against the baseline, returning
    /// its certified lift if it clears the gate. The training seed is `order`, the
    /// submission's index, so the whole replay is deterministic from the sequence.
    fn certify(
        &self,
        submission: &RecipeSubmission,
        order: u64,
        baseline_m: &Measurement,
    ) -> (Measurement, Option<Lift>) {
        let artifact = self.cluster.train_sync(&submission.recipe, order);
        let measurement = self.scorer.score_sync(&artifact, Split::HeldOut);
        let lift = estimate_lift(&measurement, baseline_m);
        // Gate is load-bearing: a positive-but-underpowered or noise lift never
        // certifies, so it can never set a record or be paid (fail-closed on NaN).
        if self.gate.clears(&lift, &measurement) {
            (measurement, Some(lift))
        } else {
            (measurement, None)
        }
    }

    /// Replay an ordered sequence of `(researcher, recipe)` submissions through the
    /// continuous market: train each, re-score on held-out, gate, and settle the
    /// marginal record bounties.
    ///
    /// Settlement is delegated to [`settle_record_bounty`] over the gate-clearing
    /// beats, so this market inherits its marginal-improvement invariant verbatim:
    /// the frontier is bought exactly once, sub-epsilon and regressing resubmissions
    /// pay nothing, and replaying the returned beats reproduces the same payouts.
    #[must_use]
    pub fn run(&self, submissions: &[RecipeSubmission]) -> ContinuousMarketOutcome {
        let baseline_m = self.baseline_measurement();
        let baseline_micros = to_micros(estimate_lift(&baseline_m, &baseline_m).delta); // == 0

        // First pass: certify every submission and build the ordered beat sequence
        // for the gate-clearing ones. We track each beat's index so we can map the
        // settler's per-beat payouts back onto the corresponding submission.
        let mut results: Vec<SubmissionResult> = Vec::with_capacity(submissions.len());
        let mut beats: Vec<RecordBeat> = Vec::new();
        let mut beat_source: Vec<usize> = Vec::new(); // beat i -> submissions index
        for (order, submission) in submissions.iter().enumerate() {
            let (measurement, lift) = self.certify(submission, order as u64, &baseline_m);
            let lift_micros = lift.map(|l| to_micros(l.delta));
            if let Some(micros) = lift_micros {
                beat_source.push(order);
                beats.push(RecordBeat {
                    researcher: submission.researcher.clone(),
                    new_best_micros: micros,
                });
            }
            results.push(SubmissionResult {
                researcher: submission.researcher.clone(),
                heldout_value: measurement.value,
                lift_micros,
                became_record: false, // set in the settlement pass below
                paid_wei: 0,
            });
        }

        // Settle the marginal record bounties over the gate-clearing beats. This is
        // the SAME settler the e2e/continuous-arena paths use, so the marginal-once
        // invariant and the sub-epsilon / regression rules are shared, not re-derived.
        let payouts = settle_record_bounty(
            baseline_micros,
            &beats,
            self.epsilon_micros,
            self.wei_per_micro,
        );

        // Second pass: replay the settler's accept/reject decision locally so we can
        // mark exactly which submissions became records and what each was paid. The
        // settler pays a beat iff its marginal over the running best clears epsilon;
        // we mirror that decision deterministically to attribute payouts back to
        // submissions (the settler returns payouts in beat order for accepted beats).
        let mut current_best = baseline_micros;
        let mut final_best_micros: Option<i64> = None;
        for (beat_idx, beat) in beats.iter().enumerate() {
            let marginal = beat.new_best_micros.saturating_sub(current_best);
            if marginal >= self.epsilon_micros && marginal > 0 {
                let owed = (marginal as u128).saturating_mul(self.wei_per_micro);
                let src = beat_source[beat_idx];
                results[src].became_record = true;
                results[src].paid_wei = owed;
                current_best = beat.new_best_micros;
                final_best_micros = Some(beat.new_best_micros);
            }
        }

        let final_best_value = final_best_micros.map(|m| baseline_m.value + (m as f64) / 1e6);
        ContinuousMarketOutcome {
            results,
            payouts,
            final_best_value,
            final_best_micros,
        }
    }
}

// --- m-of-n re-score panel --------------------------------------------------

/// A single referee's verdict when re-scoring an artifact against a self-reported
/// held-out value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RefereeVerdict {
    /// The eval-shard count this referee re-scored with (its independent config).
    pub eval_shards: u32,
    /// The held-out value this referee independently measured (`-loss`).
    pub measured_value: f64,
    /// Lower CI bound of this referee's measurement.
    pub ci_lower: f64,
    /// Upper CI bound of this referee's measurement.
    pub ci_upper: f64,
    /// Whether the self-reported claim falls within this referee's CI. A referee
    /// *accepts* a claim it cannot statistically distinguish from its own honest
    /// re-score, and *rejects* a claim that sits outside its interval.
    pub accepts_claim: bool,
}

/// The aggregate verdict of the panel on a self-reported held-out claim.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelVerdict {
    /// Each referee's independent verdict, in panel order.
    pub verdicts: Vec<RefereeVerdict>,
    /// How many referees accepted the claim.
    pub accepting: usize,
    /// The quorum required (`m` of `n`).
    pub quorum: usize,
    /// Whether at least `m` of the `n` referees accepted (the panel's decision).
    pub accepted: bool,
    /// The panel's consensus held-out value: the mean of the referees' measured
    /// values. This is the number the market trusts, never the self-reported claim.
    pub consensus_value: f64,
}

/// An m-of-n re-score harness: `n` independent referees, of which `m` must agree
/// before a self-reported held-out number is believed.
///
/// Each referee is a [`DistributedTrainingScorer`] configured with a *different*
/// eval-shard count, so they sample the held-out distribution independently (the
/// scorer's per-shard noise is seeded from the artifact and the shard index, so
/// distinct shard counts yield genuinely distinct — but still deterministic —
/// sample sets). On a real artifact every honest referee's CI brackets the true
/// held-out value and they corroborate each other; a cluster that *self-reports* a
/// better-than-real loss produces a claim outside every honest referee's CI, so the
/// panel rejects it by majority.
///
/// This is the same m-of-n trust model the Phase-0 docs describe for the cluster's
/// own operators, applied one level up: the *Referee* itself is replicated so no
/// single referee (or a colluding cluster + referee) can certify a number the
/// others cannot reproduce.
#[derive(Clone, Debug)]
pub struct RescorePanel {
    /// One scorer per referee; each has a distinct eval-shard config so the
    /// re-scores are independent samples of the same held-out distribution.
    referees: Vec<DistributedTrainingScorer>,
    /// The quorum `m`: how many referees must accept a claim for the panel to.
    quorum: usize,
}

impl RescorePanel {
    /// Build an `m`-of-`n` panel from a set of distinct eval-shard configs (one per
    /// referee). `quorum` is `m`; the panel size `n` is `shard_configs.len()`.
    ///
    /// # Panics
    /// Panics if `shard_configs` is empty or `quorum` exceeds the panel size — an
    /// unsatisfiable quorum is a construction error, not a runtime condition.
    #[must_use]
    pub fn new(shard_configs: &[u32], quorum: usize) -> Self {
        assert!(
            !shard_configs.is_empty(),
            "a panel needs at least one referee"
        );
        assert!(
            quorum >= 1 && quorum <= shard_configs.len(),
            "quorum m ({quorum}) must be in 1..=n ({})",
            shard_configs.len()
        );
        let referees = shard_configs
            .iter()
            .map(|&shards| DistributedTrainingScorer::new(shards))
            .collect();
        Self { referees, quorum }
    }

    /// A canonical `m`-of-`n` majority panel: `n` referees whose shard counts are
    /// `base, base+1, ..., base+n-1` (distinct so the samples are independent), with
    /// quorum `m = floor(n/2) + 1`.
    ///
    /// # Panics
    /// Panics if `n == 0` (see [`RescorePanel::new`]).
    #[must_use]
    pub fn majority(n: usize, base_shards: u32) -> Self {
        let configs: Vec<u32> = (0..n as u32).map(|i| base_shards + i).collect();
        Self::new(&configs, n / 2 + 1)
    }

    /// The panel size `n`.
    #[must_use]
    pub fn size(&self) -> usize {
        self.referees.len()
    }

    /// The quorum `m`.
    #[must_use]
    pub fn quorum(&self) -> usize {
        self.quorum
    }

    /// Re-score `artifact` on held-out across all `n` referees and judge a
    /// `claimed_value` (a self-reported held-out value, `-loss`) against the panel.
    ///
    /// Each referee accepts the claim iff it falls within that referee's 95% CI —
    /// i.e. the referee cannot statistically distinguish the claim from its own
    /// honest re-score. The panel accepts iff at least `m` of `n` referees do. The
    /// returned [`PanelVerdict::consensus_value`] is the mean of the referees'
    /// independently measured values — the number the market trusts.
    #[must_use]
    pub fn judge_claim(&self, artifact: &TrainedArtifact, claimed_value: f64) -> PanelVerdict {
        let verdicts: Vec<RefereeVerdict> = self
            .referees
            .iter()
            .map(|scorer| {
                let m = scorer.score_sync(artifact, Split::HeldOut);
                // Accept iff the claim sits inside this referee's CI. Written as a
                // closed-interval membership; a NaN claim fails both bounds and is
                // rejected (fail-closed), so a non-finite self-report cannot pass.
                let accepts = claimed_value >= m.ci_lower && claimed_value <= m.ci_upper;
                RefereeVerdict {
                    eval_shards: m.n,
                    measured_value: m.value,
                    ci_lower: m.ci_lower,
                    ci_upper: m.ci_upper,
                    accepts_claim: accepts,
                }
            })
            .collect();

        let accepting = verdicts.iter().filter(|v| v.accepts_claim).count();
        let consensus_value =
            verdicts.iter().map(|v| v.measured_value).sum::<f64>() / verdicts.len() as f64;
        PanelVerdict {
            accepted: accepting >= self.quorum,
            accepting,
            quorum: self.quorum,
            consensus_value,
            verdicts,
        }
    }

    /// Re-score `artifact` and judge whether the referees *agree among themselves*
    /// (independent of any self-report): the panel agrees iff at least `m` of `n`
    /// referees' CIs contain the consensus (mean) value. On a genuine artifact this
    /// holds — the honest referees corroborate each other. This is the corroboration
    /// check the continuous market would run before trusting a held-out re-score.
    #[must_use]
    pub fn referees_agree(&self, artifact: &TrainedArtifact) -> bool {
        let measurements: Vec<Measurement> = self
            .referees
            .iter()
            .map(|scorer| scorer.score_sync(artifact, Split::HeldOut))
            .collect();
        let consensus =
            measurements.iter().map(|m| m.value).sum::<f64>() / measurements.len() as f64;
        let agreeing = measurements
            .iter()
            .filter(|m| consensus >= m.ci_lower && consensus <= m.ci_upper)
            .count();
        agreeing >= self.quorum
    }
}

// --- A small sync re-score shim so the market needs no executor -------------
//
// The Phase-0 `DistributedTrainingScorer::measure` is private; its `Scorer::score`
// is async (the trait is async). The continuous market and the panel are pure,
// deterministic, synchronous computations, so we expose a sync re-score here by
// driving the ready future to completion without an executor. The scorer's body is
// `std::future::ready(...)`, so this poll always completes immediately — no clock,
// no I/O, no parking.

trait SyncScore {
    /// Synchronously re-score an artifact on a split (the underlying future is
    /// already-ready, so this never blocks).
    fn score_sync(&self, artifact: &TrainedArtifact, split: Split) -> Measurement;
}

impl SyncScore for DistributedTrainingScorer {
    fn score_sync(&self, artifact: &TrainedArtifact, split: Split) -> Measurement {
        // The scorer's measuring core is synchronous; call it directly rather than
        // driving the (always-ready) `Scorer::score` future. No executor, no unsafe.
        self.measure(artifact, split)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVAL_SHARDS: u32 = 16; // >= Gate::default().min_n (12)
    const WEI_PER_MICRO: u128 = 1_000_000_000;
    const EPSILON_MICROS: i64 = 1_000; // 0.001 score point

    /// The strong winner / moderate / failure-mode recipes from the Phase-0 e2e,
    /// reused so the continuous market is exercised on the same dynamics surface.
    fn recipe(islands: u32, h: u32, keep: f64, lr: f64) -> TrainingRecipe {
        TrainingRecipe {
            islands,
            inner_steps: h,
            keep_fraction: keep,
            inner_lr: lr,
            ..TrainingRecipe::baseline()
        }
    }

    fn market() -> ContinuousTrainingMarket {
        ContinuousTrainingMarket::new(
            EVAL_SHARDS,
            Gate::default(),
            TrainingRecipe::baseline(),
            0,
            EPSILON_MICROS,
            WEI_PER_MICRO,
        )
    }

    // --- continuous leaderboard -------------------------------------------

    /// A strengthening sequence of genuine improvements: each new record is paid its
    /// marginal over the prior best, and the total equals `wei_per_micro *
    /// (final_best - baseline)` — the frontier is bought exactly once.
    #[test]
    fn each_marginal_improvement_is_paid_exactly_once() {
        let m = market();
        // Three genuinely-improving recipes: a moderate, a stronger, then the strong
        // winner. (More islands at the optimal sync interval = lower held-out loss.)
        let subs = vec![
            RecipeSubmission::new("alice", recipe(2, 32, 0.1, 3e-3)),
            RecipeSubmission::new("bob", recipe(4, 32, 0.1, 3e-3)),
            RecipeSubmission::new("carol", recipe(8, 32, 0.2, 3e-3)),
        ];
        let outcome = m.run(&subs);

        // Each submission was a genuine, gate-clearing improvement => three records.
        assert_eq!(
            outcome.payouts.len(),
            3,
            "each genuine marginal improvement pays once: {:?}",
            outcome.results
        );
        assert!(outcome.results.iter().all(|r| r.became_record));

        // Marginal-once invariant: total paid == wei_per_micro * (final - baseline).
        let final_micros = outcome.final_best_micros.expect("a record was set");
        let total: u128 = outcome.payouts.iter().map(|p| p.wei).sum();
        assert_eq!(
            total,
            (final_micros as u128) * WEI_PER_MICRO,
            "frontier bought exactly once"
        );

        // (We intentionally do NOT assert that the per-record marginals "telescope" to
        // the final best: a sum of consecutive differences is `last - first` by
        // arithmetic, so that check is a tautology that proves nothing about payouts.
        // The load-bearing invariant is the `total == final * WEI_PER_MICRO` check
        // above — the frontier is bought exactly once, in wei.)
    }

    /// A re-submission of an already-beaten (or equal) recipe pays zero: the market
    /// pays on the MEASURED held-out marginal over the standing best, not on recipe
    /// novelty. Bob resubmits Alice's recipe, but it trains under a different seed
    /// whose per-seed noise (TRAIN_NOISE = 0.005 > epsilon = 0.001) lands at a *worse*
    /// measured value, so it clears no epsilon over the record and pays zero — the
    /// gain was already bought.
    #[test]
    fn non_improving_resubmission_pays_zero() {
        let m = market();
        let strong = recipe(8, 32, 0.2, 3e-3);
        let subs = vec![
            RecipeSubmission::new("alice", strong), // sets the record
            // Same recipe, later seed: its measured held-out value did not clear
            // epsilon over Alice's record, so no marginal is paid.
            RecipeSubmission::new("bob", strong),
            RecipeSubmission::new("carol", recipe(4, 32, 0.1, 3e-3)), // weaker => 0
        ];
        let outcome = m.run(&subs);

        // Only the first submission set a record and was paid.
        assert_eq!(outcome.payouts.len(), 1, "{:?}", outcome.results);
        assert!(outcome.results[0].became_record);
        assert!(outcome.results[0].paid_wei > 0);

        // The duplicate and the weaker resubmission both pay zero and do not record.
        assert!(!outcome.results[1].became_record);
        assert_eq!(outcome.results[1].paid_wei, 0);
        assert!(!outcome.results[2].became_record);
        assert_eq!(outcome.results[2].paid_wei, 0);

        // The frontier was bought exactly once: total == first record's marginal.
        let total: u128 = outcome.payouts.iter().map(|p| p.wei).sum();
        assert_eq!(total, outcome.results[0].paid_wei);
    }

    /// A non-improving submission *between* two improvements does not move the bar,
    /// so the later genuine improvement's marginal is measured from the unchanged
    /// best — never double-paying the dip-and-recover gap.
    #[test]
    fn dip_between_records_does_not_double_pay() {
        let m = market();
        let subs = vec![
            RecipeSubmission::new("alice", recipe(4, 32, 0.1, 3e-3)), // record
            RecipeSubmission::new("bob", recipe(2, 32, 0.1, 3e-3)),   // weaker => 0, no move
            RecipeSubmission::new("carol", recipe(8, 32, 0.2, 3e-3)), // record over alice
        ];
        let outcome = m.run(&subs);

        assert!(outcome.results[0].became_record);
        assert!(!outcome.results[1].became_record);
        assert_eq!(outcome.results[1].paid_wei, 0);
        assert!(outcome.results[2].became_record);

        // carol's marginal is measured from alice's best, not bob's lower value.
        let alice_micros = outcome.results[0].lift_micros.unwrap();
        let carol_micros = outcome.results[2].lift_micros.unwrap();
        let expected_carol_marginal = (carol_micros - alice_micros) as u128 * WEI_PER_MICRO;
        assert_eq!(outcome.results[2].paid_wei, expected_carol_marginal);

        // Total still equals the final frontier bought once.
        let total: u128 = outcome.payouts.iter().map(|p| p.wei).sum();
        assert_eq!(total, carol_micros as u128 * WEI_PER_MICRO);
    }

    /// A plausible-but-worse recipe (over-compressed gradients: worse held-out
    /// generalization) fails the gate and never pays — the continuous market refuses
    /// noise exactly as the one-shot market does.
    #[test]
    fn gated_out_recipe_never_records_or_pays() {
        let m = market();
        let subs = vec![
            // Over-compressed: looks plausible, generalizes worse than baseline.
            RecipeSubmission::new("aggressive", recipe(4, 32, 0.0005, 3e-3)),
            // Sync far too rarely: islands drift, worse than baseline on held-out.
            RecipeSubmission::new("drifter", recipe(4, 4000, 0.1, 3e-3)),
        ];
        let outcome = m.run(&subs);
        assert!(outcome.payouts.is_empty(), "{:?}", outcome.results);
        assert!(outcome.results.iter().all(|r| !r.became_record));
        assert!(outcome.results.iter().all(|r| r.lift_micros.is_none()));
        assert!(outcome.final_best_micros.is_none());
    }

    /// The continuous market's settlement is the batch settler: replaying the
    /// gate-clearing beats through `settle_record_bounty` reproduces the same total.
    #[test]
    fn settlement_is_recomputable_from_beats() {
        let m = market();
        let subs = vec![
            RecipeSubmission::new("alice", recipe(2, 32, 0.1, 3e-3)),
            RecipeSubmission::new("bob", recipe(8, 32, 0.2, 3e-3)),
        ];
        let outcome = m.run(&subs);
        // Rebuild the beats from the recorded results and re-settle independently.
        let beats: Vec<RecordBeat> = outcome
            .results
            .iter()
            .filter(|r| r.became_record)
            .map(|r| RecordBeat {
                researcher: r.researcher.clone(),
                new_best_micros: r.lift_micros.unwrap(),
            })
            .collect();
        let replay = settle_record_bounty(0, &beats, EPSILON_MICROS, WEI_PER_MICRO);
        let replay_total: u128 = replay.iter().map(|p| p.wei).sum();
        let outcome_total: u128 = outcome.payouts.iter().map(|p| p.wei).sum();
        assert_eq!(replay_total, outcome_total);
        assert_eq!(replay, outcome.payouts);
    }

    // --- m-of-n re-score panel --------------------------------------------

    /// On a genuine artifact the referees corroborate each other (their CIs bracket
    /// the consensus), and a self-report at the true held-out value is accepted by a
    /// quorum.
    #[test]
    fn referees_agree_on_a_real_artifact() {
        let panel = RescorePanel::majority(5, 13); // n=5, m=3, shards 13..=17
        let artifact = LocalSimCluster.train_sync(&recipe(8, 32, 0.2, 3e-3), 7);

        // The referees corroborate each other on the genuine artifact.
        assert!(
            panel.referees_agree(&artifact),
            "honest referees must corroborate a real artifact"
        );

        // A self-report AT the true held-out value (the consensus) is accepted by a
        // quorum — the honest claim is indistinguishable from the re-scores.
        let honest =
            DistributedTrainingScorer::new(EVAL_SHARDS).score_sync(&artifact, Split::HeldOut);
        let verdict = panel.judge_claim(&artifact, honest.value);
        assert!(
            verdict.accepted,
            "an honest self-report must clear the quorum: {verdict:?}"
        );
        assert!(verdict.accepting >= verdict.quorum);
    }

    /// A divergent self-reported score (a cluster claiming a much better held-out
    /// loss than it actually trains to) is rejected by the majority: it sits outside
    /// every honest referee's CI.
    #[test]
    fn majority_rejects_a_divergent_self_report() {
        let panel = RescorePanel::majority(5, 13);
        let artifact = LocalSimCluster.train_sync(&recipe(4, 32, 0.1, 3e-3), 9);

        // The honest held-out value is `-loss`; a lying cluster claims it is much
        // better (value far above the true one — i.e. a far lower loss). 0.5 points is
        // ~160x the per-referee eval-noise CI half-width (~0.003), so it sits well
        // outside every honest referee's interval.
        let honest =
            DistributedTrainingScorer::new(EVAL_SHARDS).score_sync(&artifact, Split::HeldOut);
        let inflated_claim = honest.value + 0.5;
        let verdict = panel.judge_claim(&artifact, inflated_claim);

        assert!(
            !verdict.accepted,
            "an inflated self-report must be rejected by the majority: {verdict:?}"
        );
        assert_eq!(
            verdict.accepting, 0,
            "no honest referee's CI brackets a wildly inflated claim"
        );
        // The consensus the market trusts is the honest re-score, not the claim.
        assert!(
            (verdict.consensus_value - honest.value).abs() < 0.05,
            "consensus tracks the honest re-score, not the inflated claim"
        );
    }

    /// A *near-boundary* cheat: a modest fraudulent inflation that is only a couple of
    /// CI band-widths above the honest value (not the trivially-extreme +0.5 of the
    /// test above) is still rejected by the majority. +0.005 is 5x the gate epsilon
    /// (0.001) — unambiguously fraudulent — yet under 2x the per-referee CI half-width
    /// (~0.003), so it lands just past the upper bound of every honest referee's
    /// interval and no referee accepts it. The offset was found empirically against
    /// the seeded measurements; this is the boundary the panel actually has to defend,
    /// not a strawman.
    #[test]
    fn majority_rejects_a_near_boundary_cheat() {
        let panel = RescorePanel::majority(5, 13); // n=5, m=3, shards 13..=17
        let artifact = LocalSimCluster.train_sync(&recipe(4, 32, 0.1, 3e-3), 9);
        let honest =
            DistributedTrainingScorer::new(EVAL_SHARDS).score_sync(&artifact, Split::HeldOut);

        // Fraudulent-but-modest: 5x epsilon, ~1.7x the CI half-width.
        let modest_cheat = honest.value + 0.005;
        let verdict = panel.judge_claim(&artifact, modest_cheat);

        assert!(
            !verdict.accepted,
            "a modest near-boundary inflation must still be rejected by the majority: {verdict:?}"
        );
        assert!(
            verdict.accepting < verdict.quorum,
            "fewer than m referees accept the modest cheat: {verdict:?}"
        );
        // It is genuinely a cheat, not noise: well beyond the gate's epsilon band.
        assert!(
            modest_cheat - honest.value > (EPSILON_MICROS as f64) / 1e6,
            "the inflation exceeds the market's epsilon — it is a real cheat, not slack"
        );
        // The consensus still tracks the honest re-score, not the inflated claim.
        assert!(
            (verdict.consensus_value - honest.value).abs() < 0.005,
            "consensus tracks the honest re-score: {verdict:?}"
        );
    }

    /// The panel decision is the m-of-n count, and `accepted == (accepting >= quorum)`
    /// is wired correctly. This probes the *honest* value (every referee accepts), so
    /// it pins the panel's plumbing but NOT its majority behaviour — the split-vote
    /// regime is exercised by `quorum_accepts_on_a_genuine_split_vote` below.
    #[test]
    fn quorum_is_m_of_n_not_unanimous() {
        let panel = RescorePanel::new(&[12, 16, 64], 2); // n=3, m=2
        let artifact = LocalSimCluster.train_sync(&recipe(8, 32, 0.2, 3e-3), 4);
        let honest =
            DistributedTrainingScorer::new(EVAL_SHARDS).score_sync(&artifact, Split::HeldOut);

        let verdict = panel.judge_claim(&artifact, honest.value);
        // The honest value clears a quorum, and `accepted` is exactly the m-of-n test.
        assert_eq!(verdict.accepted, verdict.accepting >= verdict.quorum);
        assert!(verdict.accepted);
        assert_eq!(verdict.quorum, 2);
        assert_eq!(verdict.verdicts.len(), 3);
    }

    /// The distinguishing regime: a SPLIT vote where a strict minority of referees
    /// reject and the majority accept, so `0 < accepting < n` and the panel accepts
    /// only because the quorum is `m`, not `n`. This is what proves the panel is a
    /// genuine m-of-n majority rather than an all-or-nothing (any/all) check — an
    /// all-accept or all-reject probe would pass identically for quorum 1, 2, or 3.
    ///
    /// Construction: the three referees re-score the same artifact with different
    /// shard counts `[12, 16, 64]`. The standard-error CI half-width shrinks with `n`,
    /// so the `n=64` referee has the *narrowest* interval (half-width ~0.0015 vs
    /// ~0.0028 for `n=12`) and a slightly different center (a different sample of the
    /// seeded per-shard eval noise). A claim placed just *above* the narrow referee's
    /// upper CI bound but still inside the two wider intervals is rejected by exactly
    /// that one referee and accepted by the other two. The offset (+0.0015 over the
    /// honest value) was found empirically against the seeded measurements.
    #[test]
    fn quorum_accepts_on_a_genuine_split_vote() {
        let panel = RescorePanel::new(&[12, 16, 64], 2); // n=3, m=2
        let artifact = LocalSimCluster.train_sync(&recipe(8, 32, 0.2, 3e-3), 4);
        let honest =
            DistributedTrainingScorer::new(EVAL_SHARDS).score_sync(&artifact, Split::HeldOut);

        // Just above the narrow (n=64) referee's upper CI bound, inside the wider two.
        let split_claim = honest.value + 0.0015;
        let verdict = panel.judge_claim(&artifact, split_claim);

        // Strict split: not everyone agrees, but a quorum does.
        assert!(
            verdict.accepting > 0 && verdict.accepting < panel.size(),
            "must be a genuine split vote (0 < accepting < n): {verdict:?}"
        );
        assert_eq!(
            verdict.accepting, 2,
            "the two wider-CI referees accept; the narrow one rejects: {verdict:?}"
        );
        assert!(
            verdict.accepted,
            "a 2-of-3 split clears the m=2 quorum: {verdict:?}"
        );
        // The lone dissenter is exactly the narrowest-CI (n=64) referee.
        let dissenter = verdict
            .verdicts
            .iter()
            .find(|v| !v.accepts_claim)
            .expect("one referee must reject in a split");
        assert_eq!(
            dissenter.eval_shards, 64,
            "the narrowest CI (largest shard count) is the dissenter: {verdict:?}"
        );
        // The decision is genuinely majority-gated: a unanimity (m=n=3) panel on the
        // SAME measurements and SAME claim would reject it.
        let unanimous = RescorePanel::new(&[12, 16, 64], 3);
        assert!(
            !unanimous.judge_claim(&artifact, split_claim).accepted,
            "the identical claim fails a unanimous panel — so acceptance is the m-of-n vote"
        );
    }

    /// A non-finite self-report fails closed: no referee accepts a NaN claim, so the
    /// panel rejects it regardless of quorum.
    #[test]
    fn nan_self_report_is_rejected_fail_closed() {
        let panel = RescorePanel::majority(3, 13);
        let artifact = LocalSimCluster.train_sync(&recipe(4, 32, 0.1, 3e-3), 1);
        let verdict = panel.judge_claim(&artifact, f64::NAN);
        assert!(!verdict.accepted);
        assert_eq!(verdict.accepting, 0);
    }

    /// The sync re-score shim agrees with the async `Scorer::score` (sanity that the
    /// executor-free poll returns the same measurement the trait method would).
    #[test]
    fn sync_rescore_matches_async_score() {
        use autoresearch_runtime::traits::Scorer;
        let scorer = DistributedTrainingScorer::new(EVAL_SHARDS);
        let artifact = LocalSimCluster.train_sync(&recipe(8, 32, 0.2, 3e-3), 2);
        let sync_m = scorer.score_sync(&artifact, Split::HeldOut);
        // Drive the async future on a tiny executor-free poll loop mirroring the shim.
        let async_m = futures_poll_now(scorer.score(&artifact, Split::HeldOut))
            .expect("re-score is infallible for a valid artifact");
        assert_eq!(sync_m, async_m);
    }

    /// Minimal `block_on` for an already-ready future, used only to cross-check the
    /// module's sync shim against the async trait method.
    fn futures_poll_now<F: std::future::Future>(fut: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};
        struct Noop;
        impl Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(fut);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => unreachable!("ready future"),
        }
    }
}
