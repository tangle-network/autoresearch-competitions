//! Researcher staking: the slashable collateral that gates submission.
//!
//! At `JOIN` a researcher posts **stake** before they may submit (MECHANISM.md §3).
//! Stake is the load-bearing primitive for spam- and sybil-resistance and the
//! collateral a `CHALLENGE` slashes when a researcher is caught miscertifying,
//! plagiarising, or exfiltrating (MECHANISM.md §7).
//!
//! This is the off-chain accounting mirror of the on-chain `stakes` mapping in
//! `CompetitionManager.sol`: the chain holds the real wei; this ledger is the
//! operator/Referee working view of who has posted enough to be eligible. Every
//! mutation is exact integer wei math — no float ever touches a balance.
//!
//! # Sizing intent (MECHANISM.md §3)
//!
//! There is no single right stake; the Proposer (or a network default) sets a
//! [`StakePolicy::min_stake_wei`] against the heuristic
//!
//! ```text
//! stake >= max(k * scoring_cost_per_candidate, leakage_deposit)
//! ```
//!
//! where `k` covers a few wasted Referee scoring runs a spammer would impose, and
//! `leakage_deposit` is the privacy-tier-specific over-query bond. For a public
//! arena where scoring is one cheap eval run, `k * scoring_cost` dominates and the
//! stake is small; for a private enterprise oracle where each query leaks signal
//! about the sealed held-out set, `leakage_deposit` dominates. This module only
//! enforces the *floor*; computing the two terms is a per-competition policy
//! concern the Proposer supplies.

use std::collections::HashMap;

use autoresearch_runtime::types::CompetitionId;

/// The staking policy for a competition: the minimum a researcher must post to be
/// eligible to join and submit. Set per MECHANISM.md §3 sizing intent (see module
/// docs): `min_stake_wei >= max(k * scoring_cost_per_candidate, leakage_deposit)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StakePolicy {
    /// The eligibility floor in wei. A join below this is rejected.
    pub min_stake_wei: u128,
}

impl StakePolicy {
    /// A policy whose floor is the §3 heuristic `max(k * scoring_cost, leakage_deposit)`.
    ///
    /// `k` covers a few wasted scoring runs; `scoring_cost_per_candidate` is the
    /// Referee's per-candidate cost; `leakage_deposit` is the privacy-tier over-query
    /// bond. Saturating multiply so a pathological `k`/`scoring_cost` cannot wrap.
    #[must_use]
    pub fn sized(k: u32, scoring_cost_per_candidate: u128, leakage_deposit: u128) -> Self {
        let spam_floor = scoring_cost_per_candidate.saturating_mul(u128::from(k));
        Self {
            min_stake_wei: spam_floor.max(leakage_deposit),
        }
    }

    /// Whether `posted` clears the eligibility floor.
    #[must_use]
    pub fn admits(&self, posted: u128) -> bool {
        posted >= self.min_stake_wei
    }
}

/// Errors from illegal ledger operations. All are caller bugs or adversarial
/// inputs, never transient — fail closed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StakeError {
    #[error(
        "stake {posted} for researcher {researcher} in competition {competition} is below the policy floor {floor}"
    )]
    BelowPolicy {
        competition: CompetitionId,
        researcher: String,
        posted: u128,
        floor: u128,
    },
    #[error("no stake recorded for researcher {researcher} in competition {competition}")]
    NoStake {
        competition: CompetitionId,
        researcher: String,
    },
    #[error(
        "cannot withdraw {requested} for researcher {researcher} in competition {competition}: balance is {balance}"
    )]
    Insufficient {
        competition: CompetitionId,
        researcher: String,
        requested: u128,
        balance: u128,
    },
}

/// The key into the ledger: one balance per (competition, researcher) pair.
type Key = (CompetitionId, String);

/// In-memory stake ledger keyed by `(competition, researcher)`. Balances are exact
/// integer wei. This mirrors the on-chain `stakes[competitionId][researcher]`
/// mapping; the chain is the source of truth for funds, this is the working view.
#[derive(Clone, Debug, Default)]
pub struct StakeLedger {
    balances: HashMap<Key, u128>,
}

impl StakeLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Post (add to) a researcher's stake, enforcing the policy floor on the
    /// *resulting* balance. Posting accumulates: a researcher may top up across
    /// calls, and eligibility is checked against the total, not a single deposit.
    ///
    /// # Errors
    /// [`StakeError::BelowPolicy`] if the resulting balance is below the floor.
    pub fn post_stake(
        &mut self,
        competition: CompetitionId,
        researcher: impl Into<String>,
        amount: u128,
        policy: &StakePolicy,
    ) -> Result<u128, StakeError> {
        let researcher = researcher.into();
        let key = (competition, researcher.clone());
        let entry = self.balances.entry(key).or_insert(0);
        let new_balance = entry.saturating_add(amount);
        if !policy.admits(new_balance) {
            // Do not mutate on rejection: a sub-floor post leaves the ledger unchanged.
            return Err(StakeError::BelowPolicy {
                competition,
                researcher,
                posted: new_balance,
                floor: policy.min_stake_wei,
            });
        }
        *entry = new_balance;
        Ok(new_balance)
    }

    /// The current stake balance for a researcher (0 if none recorded).
    #[must_use]
    pub fn balance(&self, competition: CompetitionId, researcher: &str) -> u128 {
        self.balances
            .get(&(competition, researcher.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// Withdraw `amount` of a researcher's stake (e.g. after settlement, un-slashed).
    /// Returns the remaining balance.
    ///
    /// # Errors
    /// [`StakeError::NoStake`] if nothing is recorded; [`StakeError::Insufficient`]
    /// if `amount` exceeds the balance.
    pub fn withdraw(
        &mut self,
        competition: CompetitionId,
        researcher: &str,
        amount: u128,
    ) -> Result<u128, StakeError> {
        let key = (competition, researcher.to_string());
        let balance = *self.balances.get(&key).ok_or_else(|| StakeError::NoStake {
            competition,
            researcher: researcher.to_string(),
        })?;
        if amount > balance {
            return Err(StakeError::Insufficient {
                competition,
                researcher: researcher.to_string(),
                requested: amount,
                balance,
            });
        }
        let remaining = balance - amount;
        *self.balances.get_mut(&key).expect("checked above") = remaining;
        Ok(remaining)
    }

    /// Slash up to `amount` of a researcher's stake, returning the amount actually
    /// removed (clamped to the balance — a slash never goes negative, never mints).
    /// Slashing is for *cheating*; honest losing is never slashable (MECHANISM.md §7).
    ///
    /// # Errors
    /// [`StakeError::NoStake`] if nothing is recorded to slash.
    pub fn slash(
        &mut self,
        competition: CompetitionId,
        researcher: &str,
        amount: u128,
    ) -> Result<u128, StakeError> {
        let key = (competition, researcher.to_string());
        let balance = self
            .balances
            .get_mut(&key)
            .ok_or_else(|| StakeError::NoStake {
                competition,
                researcher: researcher.to_string(),
            })?;
        let removed = amount.min(*balance);
        *balance -= removed;
        Ok(removed)
    }

    /// Total stake held across every (competition, researcher) — a convenience for
    /// conservation checks across the ledger.
    #[must_use]
    pub fn total_staked(&self) -> u128 {
        self.balances.values().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(min: u128) -> StakePolicy {
        StakePolicy { min_stake_wei: min }
    }

    #[test]
    fn sized_policy_takes_the_max_of_spam_floor_and_leakage() {
        // Public arena: scoring cheap, k*scoring_cost dominates.
        let public = StakePolicy::sized(5, 100, 50);
        assert_eq!(public.min_stake_wei, 500);
        // Private oracle: leakage_deposit dominates.
        let private = StakePolicy::sized(3, 100, 10_000);
        assert_eq!(private.min_stake_wei, 10_000);
    }

    #[test]
    fn post_stake_accumulates_and_clears_floor() {
        let mut ledger = StakeLedger::new();
        let p = policy(1_000);
        // A single post that clears the floor.
        let bal = ledger.post_stake(1, "0xalice", 1_000, &p).unwrap();
        assert_eq!(bal, 1_000);
        assert_eq!(ledger.balance(1, "0xalice"), 1_000);
        // Topping up accumulates.
        let bal = ledger.post_stake(1, "0xalice", 500, &p).unwrap();
        assert_eq!(bal, 1_500);
    }

    #[test]
    fn post_stake_below_floor_is_rejected_and_does_not_mutate() {
        let mut ledger = StakeLedger::new();
        let p = policy(1_000);
        let err = ledger.post_stake(1, "0xbob", 999, &p).unwrap_err();
        assert_eq!(
            err,
            StakeError::BelowPolicy {
                competition: 1,
                researcher: "0xbob".into(),
                posted: 999,
                floor: 1_000,
            }
        );
        // Rejected post leaves no balance behind.
        assert_eq!(ledger.balance(1, "0xbob"), 0);
    }

    #[test]
    fn withdraw_reduces_balance_and_rejects_overdraw() {
        let mut ledger = StakeLedger::new();
        let p = policy(1_000);
        ledger.post_stake(1, "0xalice", 1_500, &p).unwrap();

        let remaining = ledger.withdraw(1, "0xalice", 500).unwrap();
        assert_eq!(remaining, 1_000);
        assert_eq!(ledger.balance(1, "0xalice"), 1_000);

        // Cannot withdraw more than held.
        let err = ledger.withdraw(1, "0xalice", 2_000).unwrap_err();
        assert_eq!(
            err,
            StakeError::Insufficient {
                competition: 1,
                researcher: "0xalice".into(),
                requested: 2_000,
                balance: 1_000,
            }
        );

        // Unknown researcher: no stake recorded.
        assert_eq!(
            ledger.withdraw(1, "0xnobody", 1).unwrap_err(),
            StakeError::NoStake {
                competition: 1,
                researcher: "0xnobody".into(),
            }
        );
    }

    #[test]
    fn slash_clamps_to_balance_and_never_goes_negative() {
        let mut ledger = StakeLedger::new();
        let p = policy(1_000);
        ledger.post_stake(1, "0xcheat", 1_000, &p).unwrap();

        // Slashing more than held removes only what is there (never mints debt).
        let removed = ledger.slash(1, "0xcheat", 5_000).unwrap();
        assert_eq!(removed, 1_000);
        assert_eq!(ledger.balance(1, "0xcheat"), 0);

        // A partial slash on a fresh balance.
        ledger.post_stake(1, "0xcheat", 1_000, &p).unwrap();
        let removed = ledger.slash(1, "0xcheat", 400).unwrap();
        assert_eq!(removed, 400);
        assert_eq!(ledger.balance(1, "0xcheat"), 600);
    }

    #[test]
    fn slash_unknown_researcher_errors() {
        let mut ledger = StakeLedger::new();
        assert_eq!(
            ledger.slash(1, "0xghost", 1).unwrap_err(),
            StakeError::NoStake {
                competition: 1,
                researcher: "0xghost".into(),
            }
        );
    }

    #[test]
    fn ledger_keys_separate_competitions_and_researchers() {
        let mut ledger = StakeLedger::new();
        let p = policy(100);
        ledger.post_stake(1, "0xalice", 100, &p).unwrap();
        ledger.post_stake(2, "0xalice", 300, &p).unwrap();
        ledger.post_stake(1, "0xbob", 200, &p).unwrap();
        assert_eq!(ledger.balance(1, "0xalice"), 100);
        assert_eq!(ledger.balance(2, "0xalice"), 300);
        assert_eq!(ledger.balance(1, "0xbob"), 200);
        assert_eq!(ledger.total_staked(), 600);
    }
}
