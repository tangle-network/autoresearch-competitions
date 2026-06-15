//! The continuous (king-of-the-hill) arena.
//!
//! Where [`crate::orchestrator::run_oneshot_competitive`] settles a single
//! `OneShot` deadline, [`ContinuousArena`] runs the `Continuous` cadence: a
//! leaderboard that *keeps moving*. Researchers submit improving artifacts over
//! time; each new state-of-the-art that beats the current best by `epsilon` earns
//! the **marginal** improvement ([`ContinuousSchedule::RecordBounty`]), or the top
//! holder earns per epoch held ([`ContinuousSchedule::TimeAtTopStreaming`]). This
//! is the "37% -> 39.9% and still climbing" Public Continuous Arena (Scenario B).
//!
//! # Why this is the streaming form of the same invariant
//!
//! [`autoresearch_runtime::reward::settle_record_bounty`] settles a `RecordBounty`
//! over a *batch* of beats; its marginal-improvement invariant is proven in that
//! module's tests (total paid == `wei_per_micro * (final_best - baseline)`). This
//! arena is the *live* form: it sees one submission at a time, scores it through
//! the real held-out [`Scorer`], gates it, and pays the marginal on the spot. The
//! two must agree — replaying the arena's recorded [`EntryKind::Record`] beats
//! through `settle_record_bounty` reproduces the same total. That batch==streaming
//! equivalence is the recomputability guarantee: anyone can replay the leaderboard
//! log to reproduce ranks and payouts, and is asserted in the tests below.
//!
//! # Conservation
//!
//! `spent_wei <= pool_wei` ALWAYS, so the arena can never over-spend its escrow no
//! matter how strong the submissions or how long it runs. The two schedules enforce
//! this differently, each mirroring its on-chain counterpart:
//!
//! - `RecordBounty` records are **all-or-nothing**: a beat whose full marginal would
//!   exceed the remaining pool is rejected (no pay, no best move, no history append),
//!   mirroring the on-chain `recordBeat` `Overdistribution` revert. Every booked
//!   record therefore carries its full, unclamped marginal — which is what keeps the
//!   recorded history a faithful, recomputable mirror of the on-chain event log even
//!   when the pool binds.
//! - `TimeAtTopStreaming` epoch credits are **clamped** to the remaining pool
//!   (`min(wei_per_epoch, remaining)`), mirroring the on-chain `tickEpoch` clamp, so a
//!   partial final epoch is credited and further ticks credit zero without panicking.
//!
//! # Money + lift units
//!
//! All money is integer wei; all lift is integer **micro-units** (1e-6 of a score
//! point), identical to [`autoresearch_runtime::reward`]. Floats never decide
//! payouts: a measured [`Lift::delta`] is converted to micros once via
//! [`to_micros`] and all subsequent arithmetic is integer and exact.

use autoresearch_runtime::traits::{Scorer, Surface};
use autoresearch_runtime::types::{Gate, Measurement, Split};

use crate::lift::estimate_lift;
use crate::orchestrator::ProtocolError;

/// Reward shape for a continuous arena. The two continuous schedules from
/// [`autoresearch_runtime::reward::RewardSchedule`], narrowed to exactly the
/// parameters this state machine consumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuousSchedule {
    /// Pay each record-beat for its *marginal* lift over the current best, once
    /// that margin clears `epsilon_micros`.
    RecordBounty {
        epsilon_micros: i64,
        wei_per_micro: u128,
    },
    /// Pay the current top holder `wei_per_epoch` for each epoch they hold #1.
    /// Records under this schedule only move the top spot; money is credited in
    /// [`ContinuousArena::tick_epoch`].
    TimeAtTopStreaming { wei_per_epoch: u128 },
}

/// Why a [`LeaderboardEntry`] was emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A new state-of-the-art that beat the prior best by at least `epsilon`.
    /// Under `RecordBounty` this carries the marginal payment; under
    /// `TimeAtTopStreaming` it carries `paid_wei == 0` (records do not pay
    /// immediately under streaming).
    Record,
    /// A per-epoch credit to the current top holder under `TimeAtTopStreaming`.
    EpochCredit,
}

/// One append-only row in the verifiable leaderboard log. The sequence of these
/// IS the leaderboard: replaying them reproduces every rank and payout. Mirrors
/// the on-chain `RecordBeat` / epoch-credit event surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaderboardEntry {
    /// The epoch the entry was booked in.
    pub epoch: u64,
    /// The researcher the entry credits.
    pub researcher: String,
    /// The new best score in micro-units at the time of a `Record` entry; for an
    /// `EpochCredit` entry this echoes the standing best the holder is being paid
    /// for holding.
    pub lift_micros: i64,
    /// Wei paid by this entry. For a `Record` entry this is the full, unclamped
    /// marginal (an over-pool record is rejected rather than booked, so a recorded
    /// `Record` always reflects its exact marginal); for an `EpochCredit` entry it is
    /// the per-epoch credit clamped to the remaining pool.
    pub paid_wei: u128,
    /// Whether this is a record beat or a per-epoch credit.
    pub kind: EntryKind,
}

/// The outcome of a single [`ContinuousArena::submit`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubmitOutcome {
    /// Whether this submission set a new state-of-the-art (cleared the gate AND
    /// beat the current best by at least `epsilon`).
    pub became_record: bool,
    /// Wei paid out *by this submission*. Non-zero only for a `RecordBounty`
    /// record; a `TimeAtTopStreaming` record pays 0 here (it pays per epoch).
    pub paid_wei: u128,
    /// The new best in micro-units if this submission became the record, else
    /// `None`.
    pub new_best_micros: Option<i64>,
}

/// Convert a measured [`autoresearch_runtime::types::Lift::delta`] (a score-point
/// delta) into integer micro-units, consistent with
/// [`autoresearch_runtime::reward`].
///
/// `micros = round(delta * 1_000_000)`, saturating at the `i64` bounds and mapping
/// any non-finite input (`NaN` / `inf`) to `0`. The `0` mapping is fail-closed:
/// a non-finite lift never clears the gate (see [`Gate::clears`]), so it can never
/// reach a payment path, but mapping it to a benign `0` rather than a saturated
/// extreme keeps the micros value from ever standing in as a spurious record.
#[must_use]
pub fn to_micros(delta: f64) -> i64 {
    if !delta.is_finite() {
        return 0;
    }
    let scaled = (delta * 1_000_000.0).round();
    // `as i64` saturates on out-of-range floats in Rust, but be explicit so the
    // clamp is visible and intentional.
    if scaled >= i64::MAX as f64 {
        i64::MAX
    } else if scaled <= i64::MIN as f64 {
        i64::MIN
    } else {
        scaled as i64
    }
}

/// A king-of-the-hill arena over one continuous competition.
///
/// State advances by two operations: [`submit`](ContinuousArena::submit) (a
/// researcher offers an artifact) and [`tick_epoch`](ContinuousArena::tick_epoch)
/// (the clock advances). The `history` is the append-only leaderboard log; nothing
/// is ever mutated or removed from it, so [`standings`](ContinuousArena::standings)
/// can recompute the current best-per-researcher purely from the log.
#[derive(Clone, Debug)]
pub struct ContinuousArena {
    /// On-chain competition id this arena mirrors.
    pub id: u64,
    /// The promotion gate every submission must clear to be eligible to pay.
    pub gate: Gate,
    /// The reward shape (record bounty or time-at-top streaming).
    pub schedule: ContinuousSchedule,
    /// Total escrowed pool in wei. `spent_wei` can never exceed this.
    pub pool_wei: u128,
    /// The baseline score in micro-units; the first record's marginal is measured
    /// from here.
    pub baseline_micros: i64,
    /// The current best score in micro-units, or `None` before the first record.
    pub best_micros: Option<i64>,
    /// The researcher currently holding the top spot, or `None` before the first
    /// record.
    pub top_holder: Option<String>,
    /// Total wei paid out so far. Invariant: `spent_wei <= pool_wei`.
    pub spent_wei: u128,
    /// The current epoch cursor.
    pub epoch: u64,
    /// The append-only leaderboard log.
    pub history: Vec<LeaderboardEntry>,
}

impl ContinuousArena {
    /// Open a fresh arena. `baseline_micros` is the bar the first record is
    /// measured from (typically [`to_micros`] of the baseline's certified value,
    /// or `0` when lift is measured directly as a delta over baseline).
    #[must_use]
    pub fn new(
        id: u64,
        gate: Gate,
        schedule: ContinuousSchedule,
        pool_wei: u128,
        baseline_micros: i64,
    ) -> Self {
        Self {
            id,
            gate,
            schedule,
            pool_wei,
            baseline_micros,
            best_micros: None,
            top_holder: None,
            spent_wei: 0,
            epoch: 0,
            history: Vec::new(),
        }
    }

    /// The bar the next submission must beat: the current best if one exists, else
    /// the baseline.
    fn current_bar(&self) -> i64 {
        self.best_micros.unwrap_or(self.baseline_micros)
    }

    /// Wei still available to pay out.
    fn remaining_pool(&self) -> u128 {
        self.pool_wei.saturating_sub(self.spent_wei)
    }

    /// Submit `artifact` on behalf of `researcher`.
    ///
    /// The artifact is scored on [`Split::HeldOut`] through the real [`Scorer`],
    /// its lift over `baseline_measurement` is estimated, and it must clear the
    /// arena [`Gate`] to be eligible. A submission that does not clear the gate, or
    /// that fails to beat the current bar by `epsilon`, pays nothing and does not
    /// move the best (`became_record == false`).
    ///
    /// For [`ContinuousSchedule::RecordBounty`] a qualifying record is paid its full
    /// marginal `wei_per_micro * (new_best - prior_bar)` on the spot; a record whose
    /// marginal would exceed the remaining pool is rejected outright (no pay, no best
    /// move, `became_record == false`), mirroring the on-chain `Overdistribution`
    /// revert. For [`ContinuousSchedule::TimeAtTopStreaming`] a qualifying record only
    /// updates the best / top holder; payment happens in
    /// [`tick_epoch`](ContinuousArena::tick_epoch).
    ///
    /// # Errors
    /// Returns [`ProtocolError`] if surface validation or scoring fails.
    pub async fn submit<S, Sc>(
        &mut self,
        researcher: &str,
        artifact: &S::Artifact,
        surface: &S,
        scorer: &Sc,
        baseline_measurement: &Measurement,
    ) -> Result<SubmitOutcome, ProtocolError>
    where
        S: Surface,
        Sc: Scorer<Artifact = S::Artifact>,
    {
        surface.validate(artifact)?;
        let measurement = scorer.score(artifact, Split::HeldOut).await?;
        let lift = estimate_lift(&measurement, baseline_measurement);

        // Gate is load-bearing: noise / sub-threshold lift never moves the best and
        // never pays. Fail-closed (a `NaN` lift cannot clear — see `Gate::clears`).
        if !self.gate.clears(&lift, &measurement) {
            return Ok(SubmitOutcome {
                became_record: false,
                paid_wei: 0,
                new_best_micros: None,
            });
        }

        let new_micros = to_micros(lift.delta);
        let bar = self.current_bar();
        // Saturating: `new_micros` (from the saturating `to_micros`) and `bar` (a
        // caller-controlled `baseline_micros` or a prior saturated best) can be at
        // the i64 extremes, so a plain `-` would overflow — panicking under debug /
        // wrapping to garbage under release. Capping the gap at i64::MAX/MIN is
        // consistent with the saturating `.saturating_mul` in `try_record_bounty`;
        // the subsequent gate + pool checks bound the payout.
        let marginal = new_micros.saturating_sub(bar);

        match self.schedule {
            ContinuousSchedule::RecordBounty {
                epsilon_micros,
                wei_per_micro,
            } => self.try_record_bounty(
                researcher,
                new_micros,
                marginal,
                epsilon_micros,
                wei_per_micro,
            ),
            ContinuousSchedule::TimeAtTopStreaming { .. } => {
                self.try_streaming_record(researcher, new_micros, marginal)
            }
        }
    }

    /// `RecordBounty` record path. A beat qualifies iff `marginal >= epsilon` and
    /// `marginal > 0`. The full marginal `wei_per_micro * marginal` is owed; a beat
    /// whose owed marginal would exceed the remaining pool is REJECTED in full — it
    /// pays nothing, does not move the best, and is not appended to history
    /// (`became_record == false`).
    ///
    /// This all-or-nothing rejection mirrors the on-chain `recordBeat`, which reverts
    /// with `Overdistribution` rather than paying a partial marginal
    /// ([`CompetitionManager.sol`] `recordBeat`). Keeping the arena's history a faithful
    /// mirror of the on-chain `RecordBeat` event log under pool exhaustion is what makes
    /// the recomputability guarantee hold even when the pool binds: replaying the
    /// recorded beats through [`autoresearch_runtime::reward::settle_record_bounty`]
    /// reproduces the exact same per-record payouts, because every appended record was
    /// paid its full, unclamped marginal. (Conservation is preserved a fortiori: a
    /// record that cannot be fully funded is never booked, so `spent_wei <= pool_wei`.)
    fn try_record_bounty(
        &mut self,
        researcher: &str,
        new_micros: i64,
        marginal: i64,
        epsilon_micros: i64,
        wei_per_micro: u128,
    ) -> Result<SubmitOutcome, ProtocolError> {
        if marginal < epsilon_micros || marginal <= 0 {
            return Ok(SubmitOutcome {
                became_record: false,
                paid_wei: 0,
                new_best_micros: None,
            });
        }

        // marginal > 0 is guaranteed above, so the cast is non-negative.
        let owed = (marginal as u128).saturating_mul(wei_per_micro);
        // Reject (do not clamp) an over-pool record so history stays a faithful mirror
        // of the on-chain `Overdistribution` revert and every booked record carries its
        // full marginal — the precondition the batch settler replays against.
        if owed > self.remaining_pool() {
            return Ok(SubmitOutcome {
                became_record: false,
                paid_wei: 0,
                new_best_micros: None,
            });
        }

        self.spent_wei += owed;
        self.best_micros = Some(new_micros);
        self.top_holder = Some(researcher.to_string());
        self.history.push(LeaderboardEntry {
            epoch: self.epoch,
            researcher: researcher.to_string(),
            lift_micros: new_micros,
            paid_wei: owed,
            kind: EntryKind::Record,
        });

        Ok(SubmitOutcome {
            became_record: true,
            paid_wei: owed,
            new_best_micros: Some(new_micros),
        })
    }

    /// `TimeAtTopStreaming` record path. A record only seizes the top spot (no
    /// immediate pay); the holder is paid per epoch in `tick_epoch`. The
    /// epsilon-clearing rule for becoming the record is the same as RecordBounty so
    /// the leaderboard "keeps moving" by real, gate-clearing margins.
    fn try_streaming_record(
        &mut self,
        researcher: &str,
        new_micros: i64,
        marginal: i64,
    ) -> Result<SubmitOutcome, ProtocolError> {
        if marginal <= 0 {
            return Ok(SubmitOutcome {
                became_record: false,
                paid_wei: 0,
                new_best_micros: None,
            });
        }

        self.best_micros = Some(new_micros);
        self.top_holder = Some(researcher.to_string());
        self.history.push(LeaderboardEntry {
            epoch: self.epoch,
            researcher: researcher.to_string(),
            lift_micros: new_micros,
            paid_wei: 0,
            kind: EntryKind::Record,
        });

        Ok(SubmitOutcome {
            became_record: true,
            paid_wei: 0,
            new_best_micros: Some(new_micros),
        })
    }

    /// Advance the clock by one epoch and, under [`ContinuousSchedule::TimeAtTopStreaming`],
    /// credit the current top holder `min(wei_per_epoch, remaining_pool)`. Returns
    /// the wei credited this epoch (0 under `RecordBounty`, or when there is no top
    /// holder yet, or when the pool is exhausted).
    ///
    /// `RecordBounty` has no per-epoch payment (it pays on the beat itself), so its
    /// tick is a pure clock advance — the seam where a deadline / window check would
    /// live.
    pub fn tick_epoch(&mut self) -> u128 {
        self.epoch += 1;

        let wei_per_epoch = match self.schedule {
            ContinuousSchedule::TimeAtTopStreaming { wei_per_epoch } => wei_per_epoch,
            ContinuousSchedule::RecordBounty { .. } => return 0,
        };

        let Some(holder) = self.top_holder.clone() else {
            return 0;
        };

        let credited = wei_per_epoch.min(self.remaining_pool());
        if credited == 0 {
            return 0;
        }

        self.spent_wei += credited;
        self.history.push(LeaderboardEntry {
            epoch: self.epoch,
            researcher: holder,
            lift_micros: self.best_micros.unwrap_or(self.baseline_micros),
            paid_wei: credited,
            kind: EntryKind::EpochCredit,
        });
        credited
    }

    /// The append-only leaderboard log. Replaying these rows reproduces every rank
    /// and payout (the recomputable view).
    #[must_use]
    pub fn leaderboard(&self) -> &[LeaderboardEntry] {
        &self.history
    }

    /// Current best score per researcher, recomputed purely from `history`. This is
    /// the recomputable standings view: it never reads `best_micros` / `top_holder`,
    /// only the log, so an independent indexer with the same log produces the same
    /// result. Sorted best-first, ties broken by researcher id for determinism.
    #[must_use]
    pub fn standings(&self) -> Vec<(String, i64)> {
        use std::collections::BTreeMap;
        let mut best: BTreeMap<String, i64> = BTreeMap::new();
        for entry in &self.history {
            // Only Record entries advance a researcher's best; EpochCredit rows are
            // payments for holding, not new scores.
            if entry.kind == EntryKind::Record {
                best.entry(entry.researcher.clone())
                    .and_modify(|b| *b = (*b).max(entry.lift_micros))
                    .or_insert(entry.lift_micros);
            }
        }
        let mut out: Vec<(String, i64)> = best.into_iter().collect();
        // Best-first; BTreeMap already gives id order, so this is a stable sort by
        // descending score with id as the deterministic tiebreak.
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autoresearch_runtime::reward::{RecordBeat, settle_record_bounty, total_wei};
    use autoresearch_runtime::traits::{ScorerError, SurfaceError};
    use autoresearch_runtime::types::{ArtifactRef, Lift};
    use std::future::Future;

    // --- A minimal deterministic Surface/Scorer over a raw micro-units artifact.
    // The artifact IS its held-out lift delta (in score points); the scorer reports
    // a well-powered, tight measurement so the gate keys purely off the value under
    // test. This isolates the arena mechanism from any search noise.

    #[derive(Clone, Debug)]
    struct DeltaArtifact {
        /// Held-out lift over baseline, in score points (e.g. 0.05 = +5pp).
        delta: f64,
    }

    struct DeltaSurface;
    impl Surface for DeltaSurface {
        type Artifact = DeltaArtifact;
        fn id(&self) -> &str {
            "delta"
        }
        fn validate(&self, a: &Self::Artifact) -> Result<(), SurfaceError> {
            if a.delta.is_finite() {
                Ok(())
            } else {
                Err(SurfaceError::Invalid("non-finite delta".into()))
            }
        }
        fn apply_delta(
            &self,
            _b: &Self::Artifact,
            d: &Self::Artifact,
        ) -> Result<Self::Artifact, SurfaceError> {
            Ok(d.clone())
        }
        fn to_ref(&self, a: &Self::Artifact) -> Result<ArtifactRef, SurfaceError> {
            Ok(ArtifactRef(format!("delta:{}", a.delta)))
        }
    }

    /// Baseline measurement: a fixed bar the candidate's lift is measured against.
    fn baseline_measurement() -> Measurement {
        Measurement {
            value: 0.50,
            ci_lower: 0.49,
            ci_upper: 0.51,
            n: 10_000,
            cost: 0.0,
        }
    }

    /// Scorer that reports `baseline.value + artifact.delta` with a tight CI and
    /// ample n, so a positive delta always clears the gate. Deterministic; no I/O.
    struct DeltaScorer;
    impl Scorer for DeltaScorer {
        type Artifact = DeltaArtifact;
        fn id(&self) -> &str {
            "delta-scorer"
        }
        fn score(
            &self,
            artifact: &Self::Artifact,
            _split: Split,
        ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send {
            let value = 0.50 + artifact.delta;
            // Tight CI (half-width 0.005) and big n => the gate keys off the delta.
            std::future::ready(Ok(Measurement {
                value,
                ci_lower: value - 0.005,
                ci_upper: value + 0.005,
                n: 10_000,
                cost: 0.0,
            }))
        }
    }

    fn art(delta: f64) -> DeltaArtifact {
        DeltaArtifact { delta }
    }

    fn record_bounty_arena(
        epsilon_micros: i64,
        wei_per_micro: u128,
        pool: u128,
    ) -> ContinuousArena {
        ContinuousArena::new(
            42,
            Gate::default(),
            ContinuousSchedule::RecordBounty {
                epsilon_micros,
                wei_per_micro,
            },
            pool,
            0, // baseline lift bar is 0 (lift is measured as a delta over baseline)
        )
    }

    // --- to_micros ---------------------------------------------------------

    #[test]
    fn to_micros_rounds_and_is_consistent_with_reward_units() {
        assert_eq!(to_micros(0.05), 50_000);
        assert_eq!(to_micros(0.399), 399_000);
        assert_eq!(to_micros(-0.10), -100_000);
        // Non-finite is fail-closed to 0.
        assert_eq!(to_micros(f64::NAN), 0);
        assert_eq!(to_micros(f64::INFINITY), 0);
    }

    // --- extreme marginal does not overflow -------------------------------

    /// Regression: a `baseline_micros` at the i64 floor plus a positive record makes
    /// `new_micros - bar` exceed i64 range. The marginal is computed saturating, so
    /// this must NOT panic under debug (overflow-checks) — it caps at `i64::MAX` and
    /// the saturating owed-mul + pool check bound the payout. (Plain `-` here is
    /// `to_micros(1.0) - i64::MIN`, which overflows.)
    #[tokio::test]
    async fn extreme_baseline_marginal_does_not_overflow() {
        let mut arena = ContinuousArena::new(
            99,
            Gate::default(),
            ContinuousSchedule::RecordBounty {
                epsilon_micros: 1,
                wei_per_micro: 1,
            },
            u128::MAX,
            i64::MIN, // baseline at the floor: the first record's gap exceeds i64 range
        );
        let out = arena
            .submit(
                "alice",
                &art(0.10),
                &DeltaSurface,
                &DeltaScorer,
                &baseline_measurement(),
            )
            .await
            .unwrap();
        // No panic; the record is booked with a saturated marginal capped at i64::MAX.
        assert!(out.became_record);
        assert_eq!(out.paid_wei, i64::MAX as u128);
        assert_eq!(arena.best_micros, Some(100_000));
        assert!(arena.spent_wei <= arena.pool_wei);
    }

    // --- record path -------------------------------------------------------

    #[tokio::test]
    async fn record_path_pays_marginal_and_moves_the_best() {
        let mut arena = record_bounty_arena(1_000, 1_000_000, u128::MAX);
        let surface = DeltaSurface;
        let scorer = DeltaScorer;
        let base = baseline_measurement();

        let out = arena
            .submit("alice", &art(0.10), &surface, &scorer, &base)
            .await
            .unwrap();
        assert!(out.became_record);
        assert_eq!(out.new_best_micros, Some(100_000));
        // First record's marginal is measured from the baseline bar (0).
        assert_eq!(out.paid_wei, 100_000 * 1_000_000);
        assert_eq!(arena.best_micros, Some(100_000));
        assert_eq!(arena.top_holder.as_deref(), Some("alice"));

        // A second, stronger record pays only its marginal over alice's best.
        let out2 = arena
            .submit("bob", &art(0.16), &surface, &scorer, &base)
            .await
            .unwrap();
        assert!(out2.became_record);
        assert_eq!(out2.new_best_micros, Some(160_000));
        assert_eq!(out2.paid_wei, 60_000 * 1_000_000);
        assert_eq!(arena.best_micros, Some(160_000));
    }

    // --- sub-epsilon ignored ----------------------------------------------

    #[tokio::test]
    async fn sub_epsilon_beat_pays_nothing_and_does_not_move_best() {
        // epsilon = 10_000 micros (0.01 score point).
        let mut arena = record_bounty_arena(10_000, 1, u128::MAX);
        let surface = DeltaSurface;
        let scorer = DeltaScorer;
        let base = baseline_measurement();

        arena
            .submit("alice", &art(0.10), &surface, &scorer, &base)
            .await
            .unwrap();
        // +0.005 over alice = 5_000 micros, below epsilon => ignored.
        let out = arena
            .submit("bob", &art(0.105), &surface, &scorer, &base)
            .await
            .unwrap();
        assert!(!out.became_record);
        assert_eq!(out.paid_wei, 0);
        assert_eq!(arena.best_micros, Some(100_000));
        assert_eq!(arena.top_holder.as_deref(), Some("alice"));
        // The sub-epsilon beat left no Record entry.
        let records = arena
            .leaderboard()
            .iter()
            .filter(|e| e.kind == EntryKind::Record)
            .count();
        assert_eq!(records, 1, "sub-epsilon beat must not be recorded");

        // A later qualifying beat is measured from alice's unchanged 100_000, not bob's.
        let out2 = arena
            .submit("carol", &art(0.13), &surface, &scorer, &base)
            .await
            .unwrap();
        assert!(out2.became_record);
        // 130_000 - 100_000 = 30_000 micros marginal.
        assert_eq!(out2.paid_wei, 30_000);
    }

    // --- regression ignored ------------------------------------------------

    #[tokio::test]
    async fn regression_pays_nothing_and_does_not_move_best() {
        let mut arena = record_bounty_arena(0, 1_000, u128::MAX);
        let surface = DeltaSurface;
        let scorer = DeltaScorer;
        let base = baseline_measurement();

        arena
            .submit("alice", &art(0.20), &surface, &scorer, &base)
            .await
            .unwrap();
        // A worse submission than the standing best.
        let out = arena
            .submit("bob", &art(0.12), &surface, &scorer, &base)
            .await
            .unwrap();
        assert!(!out.became_record);
        assert_eq!(out.paid_wei, 0);
        assert_eq!(arena.best_micros, Some(200_000));
        assert_eq!(arena.top_holder.as_deref(), Some("alice"));
    }

    // --- gate is load-bearing ----------------------------------------------

    #[tokio::test]
    async fn gate_excludes_a_positive_but_underpowered_lift() {
        // A scorer whose CI is wide enough that a small delta fails the gate's
        // min_lift_ci_lower (0.02) even though the point estimate is positive.
        struct WideScorer;
        impl Scorer for WideScorer {
            type Artifact = DeltaArtifact;
            fn id(&self) -> &str {
                "wide"
            }
            fn score(
                &self,
                artifact: &Self::Artifact,
                _split: Split,
            ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send {
                let value = 0.50 + artifact.delta;
                std::future::ready(Ok(Measurement {
                    value,
                    ci_lower: value - 0.20, // very wide => delta CI lower < 0.02
                    ci_upper: value + 0.20,
                    n: 10_000,
                    cost: 0.0,
                }))
            }
        }
        let mut arena = record_bounty_arena(0, 1_000, u128::MAX);
        let out = arena
            .submit(
                "alice",
                &art(0.05),
                &DeltaSurface,
                &WideScorer,
                &baseline_measurement(),
            )
            .await
            .unwrap();
        assert!(
            !out.became_record,
            "underpowered lift must not clear the gate"
        );
        assert_eq!(out.paid_wei, 0);
        assert_eq!(arena.best_micros, None);
        assert!(arena.history.is_empty());
    }

    // --- pool exhaustion rejects over-pool records ------------------------

    #[tokio::test]
    async fn over_pool_record_is_rejected_not_clamped() {
        // Pool covers a 50_000-micro record (5e10) but not a 100_000-micro one (1e11).
        let mut arena = record_bounty_arena(0, 1_000_000, 60_000_000_000);
        let surface = DeltaSurface;
        let scorer = DeltaScorer;
        let base = baseline_measurement();

        // First record owes 100_000 * 1e6 = 1e11 wei but pool is 6e10. The full
        // marginal exceeds the pool, so the record is REJECTED outright (mirroring the
        // on-chain Overdistribution revert): no pay, no best move, no history append.
        let out = arena
            .submit("alice", &art(0.10), &surface, &scorer, &base)
            .await
            .unwrap();
        assert!(!out.became_record, "an over-pool record must be rejected");
        assert_eq!(out.paid_wei, 0);
        assert_eq!(
            arena.best_micros, None,
            "rejected record must not move the bar"
        );
        assert_eq!(arena.spent_wei, 0);
        assert!(
            arena.history.is_empty(),
            "rejected record must not be booked"
        );

        // A smaller record that fits in the pool is booked at its FULL marginal (no
        // clamping): 50_000 * 1e6 = 5e10 <= 6e10.
        let out2 = arena
            .submit("bob", &art(0.05), &surface, &scorer, &base)
            .await
            .unwrap();
        assert!(out2.became_record);
        assert_eq!(out2.paid_wei, 50_000 * 1_000_000, "booked at full marginal");
        assert_eq!(arena.best_micros, Some(50_000));
        assert!(
            arena.spent_wei <= arena.pool_wei,
            "pool must never be over-spent"
        );

        // A further record whose marginal (150_000 over bob) no longer fits the
        // remaining 1e10 is again rejected without panic; the best stays at bob's.
        let out3 = arena
            .submit("carol", &art(0.20), &surface, &scorer, &base)
            .await
            .unwrap();
        assert!(!out3.became_record);
        assert_eq!(out3.paid_wei, 0);
        assert_eq!(arena.best_micros, Some(50_000));
        assert!(arena.spent_wei <= arena.pool_wei);
    }

    /// Recomputability under a BINDING pool: with a pool that rejects some records, the
    /// recorded `Record` beats replayed through the batch settler must reproduce the
    /// exact same per-record payouts and total. This is the guarantee that the old
    /// clamp-and-record behaviour silently broke (a clamped record could not be
    /// reproduced by the unclamped settler); the reject-over-pool model restores it.
    #[tokio::test]
    async fn recomputable_under_binding_pool() {
        let epsilon = 1_000;
        let wei_per_micro = 1_000_000u128;
        // Pool covers alice (1e11) + bob's 60_000 marginal (6e10) = 1.6e11, but NOT a
        // later large jump, so at least one record is rejected and the pool binds.
        let pool = 160_000u128 * wei_per_micro;
        let mut arena = record_bounty_arena(epsilon, wei_per_micro, pool);
        let s = DeltaSurface;
        let sc = DeltaScorer;
        let base = baseline_measurement();

        let submissions = [
            ("alice", 0.10), // record: marginal 100_000 -> 1e11, fits
            ("bob", 0.16),   // record: marginal  60_000 -> 6e10, fits (pool now exact)
            ("carol", 0.30), // record: marginal 140_000 -> exceeds remaining => REJECTED
            ("dave", 0.161), // +1_000 over bob: clears epsilon, marginal 1_000 -> fits? pool empty => REJECTED
        ];
        for (who, delta) in submissions {
            arena
                .submit(who, &art(delta), &s, &sc, &base)
                .await
                .unwrap();
        }

        // Only alice and bob were booked; the pool bound out carol and dave.
        let records: Vec<&LeaderboardEntry> = arena
            .leaderboard()
            .iter()
            .filter(|e| e.kind == EntryKind::Record)
            .collect();
        assert_eq!(
            records.len(),
            2,
            "binding pool rejected the over-pool records"
        );
        assert_eq!(arena.best_micros, Some(160_000));
        assert_eq!(arena.spent_wei, 160_000 * wei_per_micro);
        assert!(arena.spent_wei <= arena.pool_wei);

        // Replay the RECORDED beats through the batch settler: because every booked
        // record carries its full unclamped marginal, the settler reproduces them
        // exactly. (This is the assertion the old clamping behaviour could not pass.)
        let streaming_payouts: Vec<u128> = records.iter().map(|e| e.paid_wei).collect();
        let beats: Vec<RecordBeat> = records
            .iter()
            .map(|e| RecordBeat {
                researcher: e.researcher.clone(),
                new_best_micros: e.lift_micros,
            })
            .collect();
        let batch = settle_record_bounty(arena.baseline_micros, &beats, epsilon, wei_per_micro);
        let batch_payouts: Vec<u128> = batch.iter().map(|p| p.wei).collect();
        assert_eq!(
            streaming_payouts, batch_payouts,
            "replay of recorded beats under a binding pool must reproduce payouts"
        );
        assert_eq!(arena.spent_wei, total_wei(&batch));
    }

    // --- TimeAtTopStreaming epoch crediting --------------------------------

    #[tokio::test]
    async fn time_at_top_credits_holder_per_epoch_and_conserves() {
        let mut arena = ContinuousArena::new(
            7,
            Gate::default(),
            ContinuousSchedule::TimeAtTopStreaming {
                wei_per_epoch: 1_000,
            },
            2_500, // pool only covers 2.5 epochs
            0,
        );
        let surface = DeltaSurface;
        let scorer = DeltaScorer;
        let base = baseline_measurement();

        // No holder yet => ticking credits nothing.
        assert_eq!(arena.tick_epoch(), 0);

        // Alice takes the top spot; a streaming record pays 0 on submit.
        let out = arena
            .submit("alice", &art(0.10), &surface, &scorer, &base)
            .await
            .unwrap();
        assert!(out.became_record);
        assert_eq!(
            out.paid_wei, 0,
            "streaming records pay per-epoch, not on submit"
        );

        // Two full epochs credited at wei_per_epoch.
        assert_eq!(arena.tick_epoch(), 1_000);
        assert_eq!(arena.tick_epoch(), 1_000);
        // Third epoch: only 500 left in the pool => clamped.
        assert_eq!(arena.tick_epoch(), 500);
        // Pool exhausted: further ticks credit 0, no panic.
        assert_eq!(arena.tick_epoch(), 0);

        assert_eq!(arena.spent_wei, 2_500);
        assert!(arena.spent_wei <= arena.pool_wei);
        // All credits went to the holder.
        let credited: u128 = arena
            .leaderboard()
            .iter()
            .filter(|e| e.kind == EntryKind::EpochCredit)
            .map(|e| e.paid_wei)
            .sum();
        assert_eq!(credited, 2_500);
        assert!(
            arena
                .leaderboard()
                .iter()
                .filter(|e| e.kind == EntryKind::EpochCredit)
                .all(|e| e.researcher == "alice")
        );
    }

    #[tokio::test]
    async fn time_at_top_handoff_credits_the_new_holder() {
        let mut arena = ContinuousArena::new(
            8,
            Gate::default(),
            ContinuousSchedule::TimeAtTopStreaming { wei_per_epoch: 100 },
            u128::MAX,
            0,
        );
        let s = DeltaSurface;
        let sc = DeltaScorer;
        let base = baseline_measurement();

        arena
            .submit("alice", &art(0.10), &s, &sc, &base)
            .await
            .unwrap();
        arena.tick_epoch(); // alice
        arena
            .submit("bob", &art(0.20), &s, &sc, &base)
            .await
            .unwrap();
        arena.tick_epoch(); // bob now holds

        let alice_credit: u128 = arena
            .leaderboard()
            .iter()
            .filter(|e| e.kind == EntryKind::EpochCredit && e.researcher == "alice")
            .map(|e| e.paid_wei)
            .sum();
        let bob_credit: u128 = arena
            .leaderboard()
            .iter()
            .filter(|e| e.kind == EntryKind::EpochCredit && e.researcher == "bob")
            .map(|e| e.paid_wei)
            .sum();
        assert_eq!(alice_credit, 100);
        assert_eq!(bob_credit, 100);
    }

    // --- standings recompute ----------------------------------------------

    #[tokio::test]
    async fn standings_recomputed_from_history_match_live_state() {
        let mut arena = record_bounty_arena(0, 1, u128::MAX);
        let s = DeltaSurface;
        let sc = DeltaScorer;
        let base = baseline_measurement();

        arena
            .submit("alice", &art(0.10), &s, &sc, &base)
            .await
            .unwrap();
        arena
            .submit("bob", &art(0.16), &s, &sc, &base)
            .await
            .unwrap();
        arena
            .submit("carol", &art(0.25), &s, &sc, &base)
            .await
            .unwrap();

        let standings = arena.standings();
        // Recomputed purely from history; best-first.
        assert_eq!(standings[0], ("carol".to_string(), 250_000));
        assert_eq!(standings[1], ("bob".to_string(), 160_000));
        assert_eq!(standings[2], ("alice".to_string(), 100_000));
        // The live best agrees with the recomputed leader.
        assert_eq!(arena.best_micros, Some(standings[0].1));
        assert_eq!(arena.top_holder.as_deref(), Some(standings[0].0.as_str()));
    }

    // --- batch == streaming cross-check -----------------------------------

    /// The arena's live/streaming settlement must equal the batch settlement in
    /// `reward::settle_record_bounty` over the same record sequence. This is the
    /// recomputability guarantee: replaying the recorded beats reproduces the same
    /// total paid. Also proves the marginal invariant in the streaming setting:
    /// total Record pay == wei_per_micro * (final_best - baseline).
    #[tokio::test]
    async fn streaming_matches_batch_record_bounty_settlement() {
        let epsilon = 1_000;
        let wei_per_micro = 1_000_000_000u128;
        let mut arena = record_bounty_arena(epsilon, wei_per_micro, u128::MAX);
        let s = DeltaSurface;
        let sc = DeltaScorer;
        let base = baseline_measurement();

        // A strengthening sequence, including a sub-epsilon beat and a regression
        // that must both be ignored identically by streaming and batch.
        let submissions = [
            ("alice", 0.10),  // +0.10  record
            ("bob", 0.1005),  // +0.0005 over alice => sub-epsilon, ignored
            ("carol", 0.16),  // record
            ("dave", 0.12),   // regression, ignored
            ("alice", 0.399), // the 39.9% echo, record
        ];
        for (who, delta) in submissions {
            arena
                .submit(who, &art(delta), &s, &sc, &base)
                .await
                .unwrap();
        }

        // Streaming total = sum of Record entry payouts.
        let streaming_total: u128 = arena
            .leaderboard()
            .iter()
            .filter(|e| e.kind == EntryKind::Record)
            .map(|e| e.paid_wei)
            .sum();

        // Batch: replay the SAME submission sequence as beats through the reward
        // settler (it applies the identical epsilon / regression rules).
        let beats: Vec<RecordBeat> = submissions
            .iter()
            .map(|(who, delta)| RecordBeat {
                researcher: (*who).to_string(),
                new_best_micros: to_micros(*delta),
            })
            .collect();
        let batch = settle_record_bounty(arena.baseline_micros, &beats, epsilon, wei_per_micro);
        let batch_total = total_wei(&batch);

        assert_eq!(
            streaming_total, batch_total,
            "streaming arena must settle identically to the batch record-bounty"
        );

        // The marginal invariant: the frontier is bought exactly once.
        let final_best = arena.best_micros.unwrap();
        assert_eq!(
            streaming_total,
            (final_best - arena.baseline_micros) as u128 * wei_per_micro,
            "total record pay must equal wei_per_micro * (final_best - baseline)"
        );
        assert_eq!(final_best, 399_000);

        // And the per-record breakdown matches beat-for-beat.
        let record_payouts: Vec<u128> = arena
            .leaderboard()
            .iter()
            .filter(|e| e.kind == EntryKind::Record)
            .map(|e| e.paid_wei)
            .collect();
        let batch_payouts: Vec<u128> = batch.iter().map(|p| p.wei).collect();
        assert_eq!(record_payouts, batch_payouts);
    }

    /// `Lift` is constructed directly here only to document the unit contract; the
    /// arena itself always derives micros from a measured lift via `to_micros`.
    #[test]
    fn micros_unit_contract_is_documented() {
        let lift = Lift {
            delta: 0.399,
            ci_lower: 0.39,
            ci_upper: 0.41,
            n: 100,
        };
        assert_eq!(to_micros(lift.delta), 399_000);
    }
}
