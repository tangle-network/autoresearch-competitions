//! Slashing: turning a [`DisputeOutcome`] into an exact, fund-conserving movement
//! of the researcher's and challenger's stakes.
//!
//! This is the economic settlement of a dispute (MECHANISM.md §7.1). The two
//! parties to a challenge each posted stake; the committee verdict
//! ([`crate::dispute::committee_verdict`]) decides who was right. This module
//! computes — in exact integer wei — exactly how the two stakes are slashed,
//! redistributed, refunded, and (optionally) burned.
//!
//! The load-bearing property, asserted in every test, is **conservation**:
//!
//! ```text
//! researcher_stake + challenger_stake
//!   == researcher_slashed + challenger_slashed           (slashed out of the parties)
//!   ... where every slashed wei reappears as exactly one of:
//!        challenger_reward + researcher_refund + challenger_refund + burned
//! ```
//!
//! Funds are only ever *moved*, never minted. The on-chain `resolveDispute` in
//! `CompetitionManager.sol` enforces the same arithmetic so it can never pay out
//! more than the two stakes locked for the dispute.

use crate::dispute::DisputeOutcome;

/// How a dispute redistributes the loser's slashed stake. Set per competition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlashPolicy {
    /// The challenger's reward on a successful (Overturned) challenge, as a fraction
    /// of the researcher's slashed stake, in basis points (10_000 = 100%). The
    /// remainder after the reward goes to the validators or is burned per
    /// [`SlashPolicy::burn_remainder`]. MECHANISM.md §7.1: "the reward if upheld
    /// should exceed `challenger_stake`", funded from the slashed party's stake.
    pub challenger_reward_bps: u32,
    /// When `true`, any slashed stake not paid to the winning party is burned
    /// (removed from supply) rather than (in this off-chain accounting) handed to
    /// the validators. Either way it is accounted for — conservation holds.
    pub burn_remainder: bool,
}

impl SlashPolicy {
    /// Reward share denominator: 10_000 bps = 100%.
    pub const BPS_DENOM: u32 = 10_000;
}

/// The exact disposition of the two stakes after a dispute resolves. Every field is
/// integer wei; the conservation invariant ties them together (see module docs and
/// [`SlashResolution::conserves`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlashResolution {
    /// Researcher stake removed (slashed). Non-zero only on Overturned.
    pub researcher_slashed: u128,
    /// Challenger stake removed (slashed). Non-zero only on Upheld.
    pub challenger_slashed: u128,
    /// Paid to the challenger out of the researcher's slashed stake (Overturned).
    pub challenger_reward: u128,
    /// Researcher stake returned to the researcher (Upheld / Inconclusive).
    pub researcher_refund: u128,
    /// Challenger stake returned to the challenger (Overturned / Inconclusive).
    pub challenger_refund: u128,
    /// Slashed stake neither refunded nor paid as reward: validator share or burn.
    pub burned: u128,
}

impl SlashResolution {
    /// Total wei removed from the two parties (the "in" side of conservation).
    #[must_use]
    pub fn total_slashed(&self) -> u128 {
        // Saturating: a pathological resolution cannot wrap the slashed total.
        self.researcher_slashed
            .saturating_add(self.challenger_slashed)
    }

    /// Total wei that flowed back out as everything except a party's own refund:
    /// the cross-party reward + burn/validator share. On a conserving resolution
    /// this equals [`Self::total_slashed`] — every slashed wei reappears here.
    #[must_use]
    pub fn total_redistributed(&self) -> u128 {
        self.total_slashed()
    }

    /// Conservation check. Two independent equalities must hold so funds are only
    /// moved, never minted:
    ///
    /// 1. **Total conservation:** the two posted stakes equal everything that flows
    ///    back out — `reward + researcher_refund + challenger_refund + burned`.
    /// 2. **No self-refund of slashed stake:** a party is never refunded more than
    ///    they posted minus what was slashed from them. A researcher slashed their
    ///    full stake gets a zero refund of *their own* stake (they may still receive
    ///    the challenger's slashed stake, but that is the challenger's wei moving, not
    ///    a refund of the researcher's slashed wei).
    ///
    /// Together these mean: slashed wei can only flow to the *other* party (as reward
    /// or award) or be burned — never back to the party it was slashed from.
    #[must_use]
    pub fn conserves(&self, researcher_stake: u128, challenger_stake: u128) -> bool {
        // Use checked addition throughout: a stake pair (or set of out-flows) whose
        // sum exceeds u128::MAX cannot conserve, so fail closed (non-conserving)
        // rather than wrap/panic. This is the saturating-arithmetic contract the
        // module docstring promises — a pathological value can never wrap.
        let Some(total_in) = researcher_stake.checked_add(challenger_stake) else {
            return false;
        };
        let Some(total_out) = self
            .challenger_reward
            .checked_add(self.researcher_refund)
            .and_then(|s| s.checked_add(self.challenger_refund))
            .and_then(|s| s.checked_add(self.burned))
        else {
            return false;
        };
        if total_in != total_out {
            return false;
        }
        // What each party keeps of their OWN stake after slashing.
        let researcher_kept = researcher_stake.saturating_sub(self.researcher_slashed);
        let challenger_kept = challenger_stake.saturating_sub(self.challenger_slashed);
        // A party must be refunded at least the wei they kept of their own stake, and
        // anything above that is a cross-party RECEIPT funded by the other's slash.
        if self.researcher_refund < researcher_kept || self.challenger_refund < challenger_kept {
            return false;
        }
        let researcher_received = self.researcher_refund - researcher_kept;
        let challenger_received = match self
            .challenger_reward
            .checked_add(self.challenger_refund - challenger_kept)
        {
            Some(v) => v,
            None => return false,
        };
        // The total slashed must exactly fund every cross-party receipt plus the burn:
        // slashed wei flows only to the OTHER party or is burned — never self-refunded.
        let Some(total_received) = researcher_received
            .checked_add(challenger_received)
            .and_then(|s| s.checked_add(self.burned))
        else {
            return false;
        };
        self.total_slashed() == total_received
    }
}

/// Resolve a dispute into the exact stake movements (MECHANISM.md §7.1).
///
/// Rules:
/// - [`DisputeOutcome::Overturned`] (challenge was right, certification was wrong):
///   the **researcher** is slashed their full stake; the challenger gets a reward of
///   `min(researcher_stake, reward_cap)` where `reward_cap = researcher_stake *
///   challenger_reward_bps / 10_000`; the challenger's own stake is refunded; the
///   remainder of the researcher's slashed stake (after the reward) is burned /
///   handed to validators per policy.
/// - [`DisputeOutcome::Upheld`] (challenge was wrong, certification stands): the
///   **challenger** is slashed their full stake (lost to the researcher or burned
///   per policy); the researcher's stake is refunded in full.
/// - [`DisputeOutcome::Inconclusive`]: no fault proven — both stakes are refunded,
///   nothing slashed. Honest losing is never slashable.
///
/// The result conserves funds exactly (see [`SlashResolution::conserves`]).
#[must_use]
pub fn resolve_dispute(
    outcome: DisputeOutcome,
    researcher_stake: u128,
    challenger_stake: u128,
    policy: &SlashPolicy,
) -> SlashResolution {
    match outcome {
        DisputeOutcome::Overturned => {
            // The researcher's full stake is slashed. The challenger earns a capped
            // reward from it and gets their own stake back; the rest is burned /
            // sent to validators.
            let reward_cap = mul_bps(researcher_stake, policy.challenger_reward_bps);
            let challenger_reward = researcher_stake.min(reward_cap);
            let burned = researcher_stake - challenger_reward;
            SlashResolution {
                researcher_slashed: researcher_stake,
                challenger_slashed: 0,
                challenger_reward,
                researcher_refund: 0,
                challenger_refund: challenger_stake,
                burned,
            }
        }
        DisputeOutcome::Upheld => {
            // The challenger's full stake is slashed. Per policy it is either burned
            // or awarded to the wronged researcher; the researcher's stake refunds.
            // We model the redistribution to the researcher as a "reward" to the
            // researcher when not burning — but to keep the slashed-stake accounting
            // unambiguous (slashed == reward + burned, where `reward` is the
            // challenger's), an Upheld slash that pays the researcher is booked as a
            // refund-on-top to the researcher, and the burn path books it as burned.
            let (researcher_extra, burned) = if policy.burn_remainder {
                (0, challenger_stake)
            } else {
                (challenger_stake, 0)
            };
            SlashResolution {
                researcher_slashed: 0,
                challenger_slashed: challenger_stake,
                challenger_reward: 0,
                // Saturating: consistent with the module's saturating-arithmetic
                // contract — a stake pair summing past u128::MAX cannot wrap.
                researcher_refund: researcher_stake.saturating_add(researcher_extra),
                challenger_refund: 0,
                burned,
            }
        }
        DisputeOutcome::Inconclusive => SlashResolution {
            researcher_slashed: 0,
            challenger_slashed: 0,
            challenger_reward: 0,
            researcher_refund: researcher_stake,
            challenger_refund: challenger_stake,
            burned: 0,
        },
    }
}

/// `value * bps / 10_000` in saturating u128 (a pathological `value`/`bps` cannot
/// wrap; the product is at most `value * 10_000`).
fn mul_bps(value: u128, bps: u32) -> u128 {
    value.saturating_mul(u128::from(bps)) / u128::from(SlashPolicy::BPS_DENOM)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESEARCHER_STAKE: u128 = 5_000;
    const CHALLENGER_STAKE: u128 = 500;

    fn policy(reward_bps: u32, burn: bool) -> SlashPolicy {
        SlashPolicy {
            challenger_reward_bps: reward_bps,
            burn_remainder: burn,
        }
    }

    /// Overturned: researcher slashed in full, challenger rewarded a capped share and
    /// refunded their own stake, remainder burned. Conservation holds.
    #[test]
    fn overturned_slashes_researcher_and_rewards_challenger() {
        // 30% reward => 1_500 of the 5_000 slashed; 3_500 burned/validators.
        let p = policy(3_000, true);
        let r = resolve_dispute(
            DisputeOutcome::Overturned,
            RESEARCHER_STAKE,
            CHALLENGER_STAKE,
            &p,
        );
        assert_eq!(r.researcher_slashed, 5_000);
        assert_eq!(r.challenger_slashed, 0);
        assert_eq!(r.challenger_reward, 1_500);
        assert_eq!(r.challenger_refund, 500);
        assert_eq!(r.researcher_refund, 0);
        assert_eq!(r.burned, 3_500);
        // The §7.1 worked example: challenger nets reward (1_500) on top of refund.
        assert!(
            r.challenger_reward > 0,
            "challenging a real fault must be +EV"
        );
        assert!(
            r.conserves(RESEARCHER_STAKE, CHALLENGER_STAKE),
            "Overturned must conserve funds: {r:?}"
        );
    }

    /// The reward is capped at the researcher's stake even with an over-100% bps,
    /// so an over-configured policy can never mint more than the slashed stake.
    #[test]
    fn overturned_reward_is_capped_at_researcher_stake() {
        let p = policy(20_000, true); // 200% — nonsensical, must clamp
        let r = resolve_dispute(
            DisputeOutcome::Overturned,
            RESEARCHER_STAKE,
            CHALLENGER_STAKE,
            &p,
        );
        assert_eq!(
            r.challenger_reward, RESEARCHER_STAKE,
            "reward clamps to 100%"
        );
        assert_eq!(r.burned, 0);
        assert!(r.conserves(RESEARCHER_STAKE, CHALLENGER_STAKE));
    }

    /// Upheld (burn policy): challenger slashed in full and burned, researcher
    /// refunded. Conservation holds.
    #[test]
    fn upheld_slashes_challenger_burn_policy() {
        let p = policy(3_000, true);
        let r = resolve_dispute(
            DisputeOutcome::Upheld,
            RESEARCHER_STAKE,
            CHALLENGER_STAKE,
            &p,
        );
        assert_eq!(r.challenger_slashed, 500);
        assert_eq!(r.researcher_slashed, 0);
        assert_eq!(r.researcher_refund, 5_000); // own stake back, nothing extra
        assert_eq!(r.challenger_refund, 0);
        assert_eq!(r.burned, 500);
        assert!(
            r.conserves(RESEARCHER_STAKE, CHALLENGER_STAKE),
            "Upheld (burn) must conserve funds: {r:?}"
        );
    }

    /// Upheld (award-to-researcher policy): the slashed challenger stake is handed
    /// to the wronged researcher on top of their refund instead of being burned.
    /// Conservation still holds.
    #[test]
    fn upheld_awards_challenger_stake_to_researcher_when_not_burning() {
        let p = policy(3_000, false);
        let r = resolve_dispute(
            DisputeOutcome::Upheld,
            RESEARCHER_STAKE,
            CHALLENGER_STAKE,
            &p,
        );
        assert_eq!(r.challenger_slashed, 500);
        // Researcher gets their own 5_000 back plus the 500 slashed from the challenger.
        assert_eq!(r.researcher_refund, 5_500);
        assert_eq!(r.burned, 0);
        assert!(r.conserves(RESEARCHER_STAKE, CHALLENGER_STAKE));
    }

    /// Inconclusive: nothing slashed, both stakes refunded. Honest losing is never
    /// slashable; an unresolved dispute moves no funds. Conservation holds.
    #[test]
    fn inconclusive_refunds_both_and_slashes_nothing() {
        let p = policy(3_000, true);
        let r = resolve_dispute(
            DisputeOutcome::Inconclusive,
            RESEARCHER_STAKE,
            CHALLENGER_STAKE,
            &p,
        );
        assert_eq!(r.total_slashed(), 0);
        assert_eq!(r.researcher_refund, 5_000);
        assert_eq!(r.challenger_refund, 500);
        assert_eq!(r.burned, 0);
        assert!(
            r.conserves(RESEARCHER_STAKE, CHALLENGER_STAKE),
            "Inconclusive must conserve funds: {r:?}"
        );
    }

    /// Conservation holds across the full outcome cross-product and a range of
    /// stakes/policies — the invariant is structural, not a fixture coincidence.
    #[test]
    fn conservation_holds_for_all_outcomes_and_policies() {
        let outcomes = [
            DisputeOutcome::Overturned,
            DisputeOutcome::Upheld,
            DisputeOutcome::Inconclusive,
        ];
        // The last pair sums to exactly u128::MAX (the boundary): conservation must
        // still hold there with no wrap.
        let stakes = [
            (0u128, 0u128),
            (1, 1),
            (5_000, 500),
            (7, 1_000_000),
            (u128::MAX / 2, 3),
            (u128::MAX - 1, 1),
        ];
        for outcome in outcomes {
            for &(rs, cs) in &stakes {
                for &bps in &[0u32, 2_500, 10_000, 30_000] {
                    for &burn in &[true, false] {
                        let p = policy(bps, burn);
                        let r = resolve_dispute(outcome, rs, cs, &p);
                        assert!(
                            r.conserves(rs, cs),
                            "conservation broke: outcome={outcome:?} rs={rs} cs={cs} bps={bps} burn={burn} => {r:?}"
                        );
                    }
                }
            }
        }
    }

    /// Boundary: stake pairs whose sum exceeds u128::MAX cannot conserve (you cannot
    /// refund more distinct wei than can exist). `resolve_dispute` and `conserves`
    /// must NOT panic or silently wrap — they fail closed: the resolution arithmetic
    /// saturates and `conserves` returns false rather than minting via overflow.
    /// This is unreachable with real wei (total ETH supply << 2^128) but locks the
    /// "a pathological value cannot wrap" contract in the docstrings.
    #[test]
    fn over_max_stake_sum_fails_closed_without_panic() {
        let outcomes = [
            DisputeOutcome::Overturned,
            DisputeOutcome::Upheld,
            DisputeOutcome::Inconclusive,
        ];
        // Each pair sums strictly past u128::MAX.
        let over_max = [(u128::MAX, 1u128), (u128::MAX, u128::MAX)];
        for outcome in outcomes {
            for &(rs, cs) in &over_max {
                for &burn in &[true, false] {
                    let p = policy(3_000, burn);
                    // Must not panic (saturating arithmetic inside resolve_dispute).
                    let r = resolve_dispute(outcome, rs, cs, &p);
                    // total_in = rs + cs overflows u128, so the resolution cannot
                    // conserve: conserves() detects the overflow and fails closed.
                    assert!(
                        !r.conserves(rs, cs),
                        "an over-u128::MAX stake sum must fail conservation, not wrap: \
                         outcome={outcome:?} rs={rs} cs={cs} burn={burn} => {r:?}"
                    );
                }
            }
        }
    }
}
