//! Reward schedules and their settlement logic.
//!
//! A [`RewardSchedule`] maps certified results to payouts. The four shapes match
//! `docs/MECHANISM.md §5`. All money math is integer (wei); all lift math is in
//! integer **micro-units** (1e-6 of a score point) so that settlement is exact
//! and reproducible — floats never decide payouts.
//!
//! The load-bearing property, proven in the tests below, is the
//! **marginal-improvement invariant** for [`RewardSchedule::RecordBounty`]:
//! across a monotonic sequence of record-beats, the total paid equals
//! `wei_per_micro * (final_best - baseline)`. The frontier is bought exactly
//! once — never twice for the same gain — which is what keeps a continuous
//! leaderboard moving without overpaying.

use serde::{Deserialize, Serialize};

/// A computed payout to a single researcher.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payout {
    pub researcher: String,
    pub wei: u128,
}

/// How certified results convert into payouts. `(proposed)` naming.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RewardSchedule {
    /// Winner-take-all (or proposer-defined single recipient) at the deadline.
    TerminalPrize,
    /// Split the pool across the ranked top-k by basis-point weights.
    /// `weights_bps` must be non-empty and sum to at most 10_000 (enforced by
    /// [`RewardSchedule::validate`]); any rounding dust is dropped (not minted).
    SnapshotTopK { weights_bps: Vec<u32> },
    /// Continuous king-of-the-hill: pay each record-beat for its *marginal* lift
    /// over the prior best, once that margin clears `epsilon_micros`.
    RecordBounty {
        epsilon_micros: i64,
        wei_per_micro: u128,
    },
    /// Continuous: pay the holder of the top spot per epoch held.
    TimeAtTopStreaming { wei_per_epoch: u128 },
}

/// Basis points denominator: 10_000 bps = 100% of the pool.
pub const BPS_DENOM: u32 = 10_000;

/// A reward schedule that cannot conserve the pool (e.g. `SnapshotTopK` weights that
/// sum to more than 100%, or an empty weight set). Rejecting these at construction /
/// validation keeps settlement from ever emitting more than the escrowed pool, which
/// on-chain would make `distribute` revert permanently and strand the escrow.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RewardError {
    #[error("SnapshotTopK weights_bps must be non-empty")]
    EmptyWeights,
    #[error("SnapshotTopK weights_bps sum to {sum} bps, exceeding the {BPS_DENOM} bps pool")]
    WeightsExceedPool { sum: u64 },
}

impl RewardSchedule {
    /// Reject schedules that cannot conserve the reward pool.
    ///
    /// For [`RewardSchedule::SnapshotTopK`] this enforces the conservation invariant
    /// at the source: the weights must be non-empty and sum to at most `BPS_DENOM`
    /// (10_000 bps). All other schedules are structurally conserving and pass.
    pub fn validate(&self) -> Result<(), RewardError> {
        match self {
            RewardSchedule::SnapshotTopK { weights_bps } => {
                if weights_bps.is_empty() {
                    return Err(RewardError::EmptyWeights);
                }
                // Sum in u64 so a pathological vector cannot overflow u32.
                let sum: u64 = weights_bps.iter().map(|w| u64::from(*w)).sum();
                if sum > u64::from(BPS_DENOM) {
                    return Err(RewardError::WeightsExceedPool { sum });
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// A single event in which a researcher set a new best score, in micro-units.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordBeat {
    pub researcher: String,
    /// The new best score this submission achieved, in micro-units (1e-6 point).
    pub new_best_micros: i64,
}

/// Settle a [`RewardSchedule::RecordBounty`] over an ordered sequence of beats.
///
/// A beat pays iff it improves on the *current* best by at least
/// `epsilon_micros`; partial or non-improving submissions pay nothing and do not
/// move the bar. Payout for a qualifying beat is `wei_per_micro * marginal`,
/// where `marginal = new_best - current_best`.
pub fn settle_record_bounty(
    baseline_micros: i64,
    beats: &[RecordBeat],
    epsilon_micros: i64,
    wei_per_micro: u128,
) -> Vec<Payout> {
    let mut current_best = baseline_micros;
    let mut payouts: Vec<Payout> = Vec::new();
    for beat in beats {
        // Saturating sub + mul mirror the streaming arena (`continuous.rs`): both
        // `new_best_micros` and `current_best` (the caller-controlled `baseline_micros`
        // or a prior best) can sit at the i64 extremes, so a plain `-` or `*` would
        // overflow — panic under debug, wrap under release. Capping keeps this batch
        // settler bit-for-bit equivalent to the arena across the full i64/u128 range,
        // preserving the batch==streaming recomputability guarantee at the extremes.
        let marginal = beat.new_best_micros.saturating_sub(current_best);
        if marginal >= epsilon_micros && marginal > 0 {
            let wei = (marginal as u128).saturating_mul(wei_per_micro);
            payouts.push(Payout {
                researcher: beat.researcher.clone(),
                wei,
            });
            current_best = beat.new_best_micros;
        }
    }
    payouts
}

/// Settle a [`RewardSchedule::SnapshotTopK`]. `ranked` is best-first; `weights_bps`
/// gives the basis-point share of `pool_wei` for each rank. Recipients beyond the
/// shorter of the two lists get nothing. Integer division floors each share, so the
/// sum never exceeds `pool_wei` (dust is left in escrow, not minted).
///
/// Conservation is enforced two ways: callers should reject over-weighted schedules
/// up front via [`RewardSchedule::validate`], and this function additionally clamps
/// each share to the running remainder so the total can never exceed `pool_wei`
/// regardless of caller input (defense in depth — an over-weighted vector here would
/// otherwise mint more than the escrow and make the on-chain `distribute` revert
/// permanently, stranding funds).
pub fn settle_snapshot_topk(pool_wei: u128, ranked: &[String], weights_bps: &[u32]) -> Vec<Payout> {
    let k = ranked.len().min(weights_bps.len());
    let mut payouts = Vec::with_capacity(k);
    let mut remaining = pool_wei;
    for i in 0..k {
        let raw_share = pool_wei.saturating_mul(u128::from(weights_bps[i])) / 10_000u128;
        // Cap each share at what is left so total payouts never exceed the pool.
        let share = raw_share.min(remaining);
        if share > 0 {
            remaining -= share;
            payouts.push(Payout {
                researcher: ranked[i].clone(),
                wei: share,
            });
        }
    }
    payouts
}

/// Settle a [`RewardSchedule::TerminalPrize`]: the whole pool to the single winner.
pub fn settle_terminal_prize(pool_wei: u128, winner: Option<&str>) -> Vec<Payout> {
    match winner {
        Some(w) if pool_wei > 0 => vec![Payout {
            researcher: w.to_string(),
            wei: pool_wei,
        }],
        _ => Vec::new(),
    }
}

/// Settle a [`RewardSchedule::TimeAtTopStreaming`]: `wei_per_epoch` for each epoch
/// a researcher held the top spot. `holders` pairs each researcher with the count
/// of epochs they held #1.
pub fn settle_time_at_top(holders: &[(String, u64)], wei_per_epoch: u128) -> Vec<Payout> {
    holders
        .iter()
        .filter_map(|(r, epochs)| {
            let wei = u128::from(*epochs) * wei_per_epoch;
            (wei > 0).then(|| Payout {
                researcher: r.clone(),
                wei,
            })
        })
        .collect()
}

/// Total wei across a payout set — convenience for invariants and escrow checks.
pub fn total_wei(payouts: &[Payout]) -> u128 {
    payouts.iter().map(|p| p.wei).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat(r: &str, micros: i64) -> RecordBeat {
        RecordBeat {
            researcher: r.to_string(),
            new_best_micros: micros,
        }
    }

    /// The marginal-improvement invariant: for a monotonic sequence of beats
    /// that each clear epsilon, total paid == wei_per_micro * (final - baseline).
    /// The frontier is bought exactly once.
    #[test]
    fn record_bounty_buys_the_frontier_exactly_once() {
        let baseline = 100_000; // 0.10
        let wei_per_micro = 1_000_000_000u128;
        let beats = vec![
            beat("alice", 150_000), // +0.05
            beat("bob", 210_000),   // +0.06
            beat("alice", 399_000), // +0.189  (the 39.9%-ahead echo)
        ];
        let payouts = settle_record_bounty(baseline, &beats, 1_000, wei_per_micro);
        let final_best = 399_000;
        assert_eq!(
            total_wei(&payouts),
            (final_best - baseline) as u128 * wei_per_micro,
            "total paid must equal wei_per_micro * total lift over baseline"
        );
        // Each beat was paid its own marginal — no double-pay.
        assert_eq!(payouts.len(), 3);
        assert_eq!(payouts[0].wei, 50_000u128 * wei_per_micro);
        assert_eq!(payouts[1].wei, 60_000u128 * wei_per_micro);
        assert_eq!(payouts[2].wei, 189_000u128 * wei_per_micro);
    }

    /// A submission that does not clear epsilon over the current best pays nothing
    /// and does not move the bar, so a later qualifying beat is measured from the
    /// unchanged best.
    #[test]
    fn record_bounty_ignores_sub_epsilon_beats() {
        let baseline = 0;
        let wei_per_micro = 1;
        let beats = vec![
            beat("alice", 100_000), // +0.1, qualifies
            beat("bob", 105_000),   // +0.005 over alice, below epsilon -> 0
            beat("carol", 130_000), // +0.03 over alice, qualifies
        ];
        let payouts = settle_record_bounty(baseline, &beats, 10_000, wei_per_micro);
        assert_eq!(payouts.len(), 2);
        assert_eq!(payouts[0].researcher, "alice");
        assert_eq!(payouts[1].researcher, "carol");
        // carol's marginal is measured from alice's 100_000, not bob's 105_000.
        assert_eq!(payouts[1].wei, 30_000u128);
        assert_eq!(total_wei(&payouts), 130_000u128);
    }

    /// A regressing submission never pays.
    #[test]
    fn record_bounty_never_pays_regressions() {
        let payouts = settle_record_bounty(500_000, &[beat("alice", 400_000)], 0, 1_000_000);
        assert!(payouts.is_empty());
    }

    /// Regression: an extreme baseline/beat gap and a large rate must NOT overflow.
    /// The marginal sub and the owed mul both saturate (mirroring the streaming arena),
    /// so a beat at `i64::MAX` over a baseline at `i64::MIN` caps at `i64::MAX` micros
    /// and the wei product caps at `u128::MAX` — no panic under debug, no wrap under
    /// release.
    #[test]
    fn record_bounty_saturates_extreme_marginal_without_overflow() {
        // baseline at the floor: new_best - baseline overflows a plain i64 subtraction.
        let payouts = settle_record_bounty(i64::MIN, &[beat("alice", i64::MAX)], 1, u128::MAX);
        assert_eq!(payouts.len(), 1);
        // marginal saturated to i64::MAX, then (i64::MAX as u128) * u128::MAX saturated
        // to u128::MAX.
        assert_eq!(payouts[0].wei, u128::MAX);
    }

    #[test]
    fn snapshot_topk_splits_pool_and_floors_dust() {
        let pool = 1_000_000u128;
        let ranked = vec!["a".into(), "b".into(), "c".into()];
        // 50% / 30% / 20%
        let payouts = settle_snapshot_topk(pool, &ranked, &[5_000, 3_000, 2_000]);
        assert_eq!(total_wei(&payouts), 1_000_000u128);
        assert_eq!(payouts[0].wei, 500_000);
        assert_eq!(payouts[1].wei, 300_000);
        assert_eq!(payouts[2].wei, 200_000);
        // Never mints more than the pool.
        assert!(total_wei(&payouts) <= pool);
    }

    #[test]
    fn snapshot_topk_pays_only_as_many_as_ranked() {
        let payouts = settle_snapshot_topk(1_000u128, &["solo".into()], &[6_000, 4_000]);
        assert_eq!(payouts.len(), 1);
        assert_eq!(payouts[0].wei, 600);
    }

    /// Conservation under adversarial / misconfigured weights: even when the weights
    /// sum to far more than 100% the settled total can never exceed the pool. The
    /// running-remainder clamp is the defense-in-depth backstop behind `validate`.
    #[test]
    fn snapshot_topk_never_exceeds_pool_for_overweighted_inputs() {
        let pool = 1_000_000u128;
        let ranked = vec!["a".into(), "b".into(), "c".into()];

        // 60% + 50% = 110% of the pool.
        let over_two = settle_snapshot_topk(pool, &ranked, &[6_000, 5_000]);
        assert!(
            total_wei(&over_two) <= pool,
            "110% weights must not exceed pool"
        );

        // 300% of the pool.
        let over_three = settle_snapshot_topk(pool, &ranked, &[10_000, 10_000, 10_000]);
        assert!(
            total_wei(&over_three) <= pool,
            "300% weights must not exceed pool"
        );
        // The first recipient takes the whole pool; the rest get the empty remainder.
        assert_eq!(over_three[0].wei, pool);
        assert_eq!(total_wei(&over_three), pool);
    }

    #[test]
    fn reward_schedule_validate_accepts_conserving_weights() {
        assert!(
            RewardSchedule::SnapshotTopK {
                weights_bps: vec![5_000, 3_000, 2_000],
            }
            .validate()
            .is_ok()
        );
        // Under-100% (dust left in escrow) is allowed.
        assert!(
            RewardSchedule::SnapshotTopK {
                weights_bps: vec![5_000, 3_000],
            }
            .validate()
            .is_ok()
        );
        // Non-SnapshotTopK schedules are structurally conserving.
        assert!(RewardSchedule::TerminalPrize.validate().is_ok());
    }

    #[test]
    fn reward_schedule_validate_rejects_overweighted_and_empty() {
        assert_eq!(
            RewardSchedule::SnapshotTopK {
                weights_bps: vec![6_000, 5_000],
            }
            .validate(),
            Err(RewardError::WeightsExceedPool { sum: 11_000 })
        );
        assert_eq!(
            RewardSchedule::SnapshotTopK {
                weights_bps: vec![10_000, 10_000, 10_000],
            }
            .validate(),
            Err(RewardError::WeightsExceedPool { sum: 30_000 })
        );
        assert_eq!(
            RewardSchedule::SnapshotTopK {
                weights_bps: vec![]
            }
            .validate(),
            Err(RewardError::EmptyWeights)
        );
    }

    #[test]
    fn terminal_prize_all_or_nothing() {
        assert_eq!(
            settle_terminal_prize(42, Some("winner")),
            vec![Payout {
                researcher: "winner".into(),
                wei: 42
            }]
        );
        assert!(settle_terminal_prize(42, None).is_empty());
        assert!(settle_terminal_prize(0, Some("winner")).is_empty());
    }

    #[test]
    fn time_at_top_pays_per_epoch_held() {
        let holders = vec![("alice".to_string(), 3u64), ("bob".to_string(), 0u64)];
        let payouts = settle_time_at_top(&holders, 100);
        assert_eq!(payouts.len(), 1);
        assert_eq!(payouts[0].researcher, "alice");
        assert_eq!(payouts[0].wei, 300);
    }
}
