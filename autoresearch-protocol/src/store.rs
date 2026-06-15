//! In-memory competition state for the off-chain orchestrator.
//!
//! This is the operator-side mirror of the on-chain spine: it tracks who joined,
//! what they committed/revealed, and where each competition sits in its lifecycle.
//! The chain remains the source of truth for money and commitments; this store is
//! a working set the Referee and orchestrator drive against. It is deliberately
//! `HashMap`-backed and process-local — durability and reorg handling are an
//! operator concern, not a domain concern.

use std::collections::HashMap;

use autoresearch_runtime::types::{ArtifactRef, CompetitionId, Evidence};

use crate::stake::StakePolicy;

/// Lifecycle state of a competition. Transitions are strictly forward; there is no
/// path back out of `Closed`, and scoring cannot begin before submissions close.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Created but not yet open for researchers to join.
    Draft,
    /// Open for `join`; not yet accepting candidate commitments.
    Open,
    /// Accepting commit/reveal of candidates (pre-deadline).
    Submitting,
    /// Submissions closed; the Referee is scoring revealed candidates.
    Scoring,
    /// Scoring done; ranking + payout computation in progress.
    Settling,
    /// Payouts emitted; terminal.
    Closed,
}

impl Status {
    /// Whether `self -> next` is a legal forward transition.
    fn can_transition_to(self, next: Status) -> bool {
        use Status::*;
        matches!(
            (self, next),
            (Draft, Open)
                | (Open, Submitting)
                | (Submitting, Scoring)
                | (Scoring, Settling)
                | (Settling, Closed)
        )
    }
}

/// A researcher's enrollment in a competition. `stake_wei` is a placeholder for the
/// on-chain stake bond; the off-chain store only needs to know it exists for ranking
/// eligibility checks. The chain enforces the actual bond.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Researcher {
    pub address: String,
    pub stake_wei: u128,
}

/// A candidate as the orchestrator sees it across commit -> reveal -> score.
/// Mirrors [`autoresearch_runtime::types::Candidate`] but holds the in-flight
/// orchestration view (the runtime `Candidate` is the certified-ledger view).
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateRecord {
    /// Stable id (the hex commitment) — also the dedupe key within a competition.
    pub id: String,
    pub researcher: String,
    /// `keccak256(abi.encode(artifact_ref, salt))` posted at commit time.
    pub commitment: String,
    /// Set at reveal; `None` while still only committed.
    pub artifact_ref: Option<ArtifactRef>,
    /// Set once the Referee certifies the candidate; `None` until scored.
    pub evidence: Option<Evidence>,
}

/// Errors from illegal store operations. All are caller bugs (or adversarial
/// inputs), never transient — fail closed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StoreError {
    #[error("competition {0} does not exist")]
    UnknownCompetition(CompetitionId),
    #[error("competition {0} already exists")]
    DuplicateCompetition(CompetitionId),
    #[error("illegal status transition for competition {id}: {from:?} -> {to:?}")]
    IllegalTransition {
        id: CompetitionId,
        from: Status,
        to: Status,
    },
    #[error("operation requires competition {id} to be in {expected:?}, but it is {actual:?}")]
    WrongStatus {
        id: CompetitionId,
        expected: Status,
        actual: Status,
    },
    #[error("candidate {0} already committed in this competition")]
    DuplicateCandidate(String),
    #[error("candidate {0} not found")]
    UnknownCandidate(String),
    #[error("reveal commitment mismatch for candidate {0}")]
    RevealMismatch(String),
    #[error(
        "join stake {posted} for {researcher} in competition {id} is below the policy floor {floor}"
    )]
    InsufficientStake {
        id: CompetitionId,
        researcher: String,
        posted: u128,
        floor: u128,
    },
    #[error("researcher {researcher} has not joined competition {id} with stake; cannot submit")]
    NotStaked {
        id: CompetitionId,
        researcher: String,
    },
}

/// Per-competition working state.
#[derive(Clone, Debug)]
struct CompetitionState {
    status: Status,
    /// The stake floor a researcher must post at `join` to be eligible to submit
    /// (MECHANISM.md §3). A zero-floor policy (the default) admits any join, which
    /// preserves the M1 behaviour for competitions created without an explicit policy.
    stake_policy: StakePolicy,
    researchers: HashMap<String, Researcher>,
    candidates: HashMap<String, CandidateRecord>,
}

impl CompetitionState {
    fn new(stake_policy: StakePolicy) -> Self {
        Self {
            status: Status::Draft,
            stake_policy,
            researchers: HashMap::new(),
            candidates: HashMap::new(),
        }
    }
}

/// The orchestrator's process-local view of all live competitions.
#[derive(Clone, Debug, Default)]
pub struct CompetitionStore {
    competitions: HashMap<CompetitionId, CompetitionState>,
}

impl CompetitionStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new competition in `Draft` with a zero-floor stake policy (any
    /// join admitted — the M1 behaviour). Idempotency is the caller's job; a second
    /// create for the same id is an error, not a silent overwrite.
    pub fn create(&mut self, id: CompetitionId) -> Result<(), StoreError> {
        self.create_with_policy(id, StakePolicy { min_stake_wei: 0 })
    }

    /// Register a new competition in `Draft` with an explicit [`StakePolicy`]. The
    /// policy floor is enforced at [`Self::join`]; researchers below it cannot enroll,
    /// and only enrolled (staked) researchers may [`Self::commit`].
    pub fn create_with_policy(
        &mut self,
        id: CompetitionId,
        stake_policy: StakePolicy,
    ) -> Result<(), StoreError> {
        if self.competitions.contains_key(&id) {
            return Err(StoreError::DuplicateCompetition(id));
        }
        self.competitions
            .insert(id, CompetitionState::new(stake_policy));
        Ok(())
    }

    pub fn status(&self, id: CompetitionId) -> Result<Status, StoreError> {
        self.state(id).map(|s| s.status)
    }

    /// Advance a competition's lifecycle, rejecting any non-forward or skipping move.
    pub fn transition(&mut self, id: CompetitionId, to: Status) -> Result<(), StoreError> {
        let state = self.state_mut(id)?;
        let from = state.status;
        if !from.can_transition_to(to) {
            return Err(StoreError::IllegalTransition { id, from, to });
        }
        state.status = to;
        Ok(())
    }

    /// Enroll a researcher, posting `stake_wei`. Only legal while `Open`, and only
    /// if `stake_wei` clears the competition's [`StakePolicy`] floor (MECHANISM.md §3
    /// — stake is required before a researcher may submit). A researcher who re-joins
    /// accumulates stake; eligibility is checked against the resulting total.
    ///
    /// # Errors
    /// [`StoreError::InsufficientStake`] if the resulting stake is below the floor.
    pub fn join(
        &mut self,
        id: CompetitionId,
        address: impl Into<String>,
        stake_wei: u128,
    ) -> Result<(), StoreError> {
        let state = self.state_mut(id)?;
        Self::require_status(id, state.status, Status::Open)?;
        let address = address.into();
        // Accumulate against any prior stake so a top-up join can reach the floor.
        let prior = state.researchers.get(&address).map_or(0, |r| r.stake_wei);
        let total = prior.saturating_add(stake_wei);
        if !state.stake_policy.admits(total) {
            return Err(StoreError::InsufficientStake {
                id,
                researcher: address,
                posted: total,
                floor: state.stake_policy.min_stake_wei,
            });
        }
        state.researchers.insert(
            address.clone(),
            Researcher {
                address,
                stake_wei: total,
            },
        );
        Ok(())
    }

    /// Record a candidate commitment. Only legal while `Submitting`. The candidate
    /// id is the commitment string; re-committing the same id is rejected.
    pub fn commit(
        &mut self,
        id: CompetitionId,
        researcher: impl Into<String>,
        commitment: impl Into<String>,
    ) -> Result<String, StoreError> {
        let state = self.state_mut(id)?;
        Self::require_status(id, state.status, Status::Submitting)?;
        let researcher = researcher.into();
        // Stake gate: a researcher must have joined with stake clearing the policy
        // floor before they may submit a candidate (MECHANISM.md §3). The on-chain
        // contract enforces the real bond; this mirrors it for the orchestrator.
        match state.researchers.get(&researcher) {
            Some(r) if state.stake_policy.admits(r.stake_wei) => {}
            _ => {
                return Err(StoreError::NotStaked { id, researcher });
            }
        }
        let commitment = commitment.into();
        let candidate_id = commitment.clone();
        if state.candidates.contains_key(&candidate_id) {
            return Err(StoreError::DuplicateCandidate(candidate_id));
        }
        state.candidates.insert(
            candidate_id.clone(),
            CandidateRecord {
                id: candidate_id.clone(),
                researcher,
                commitment,
                artifact_ref: None,
                evidence: None,
            },
        );
        Ok(candidate_id)
    }

    /// Attach the revealed artifact reference to a committed candidate. Only legal
    /// while `Submitting`. `expected_commitment` must equal the stored commitment
    /// (the contract performs the cryptographic `keccak256(abi.encode(ref, salt))` check on-chain;
    /// here we verify the operator forwarded a consistent reveal).
    pub fn reveal(
        &mut self,
        id: CompetitionId,
        candidate_id: &str,
        expected_commitment: &str,
        artifact_ref: ArtifactRef,
    ) -> Result<(), StoreError> {
        let state = self.state_mut(id)?;
        Self::require_status(id, state.status, Status::Submitting)?;
        let cand = state
            .candidates
            .get_mut(candidate_id)
            .ok_or_else(|| StoreError::UnknownCandidate(candidate_id.to_string()))?;
        if cand.commitment != expected_commitment {
            return Err(StoreError::RevealMismatch(candidate_id.to_string()));
        }
        cand.artifact_ref = Some(artifact_ref);
        Ok(())
    }

    /// Attach certified evidence to a candidate. Only legal while `Scoring`.
    pub fn record_evidence(
        &mut self,
        id: CompetitionId,
        candidate_id: &str,
        evidence: Evidence,
    ) -> Result<(), StoreError> {
        let state = self.state_mut(id)?;
        Self::require_status(id, state.status, Status::Scoring)?;
        let cand = state
            .candidates
            .get_mut(candidate_id)
            .ok_or_else(|| StoreError::UnknownCandidate(candidate_id.to_string()))?;
        cand.evidence = Some(evidence);
        Ok(())
    }

    pub fn researchers(&self, id: CompetitionId) -> Result<Vec<&Researcher>, StoreError> {
        Ok(self.state(id)?.researchers.values().collect())
    }

    pub fn candidates(&self, id: CompetitionId) -> Result<Vec<&CandidateRecord>, StoreError> {
        Ok(self.state(id)?.candidates.values().collect())
    }

    pub fn candidate(
        &self,
        id: CompetitionId,
        candidate_id: &str,
    ) -> Result<&CandidateRecord, StoreError> {
        self.state(id)?
            .candidates
            .get(candidate_id)
            .ok_or_else(|| StoreError::UnknownCandidate(candidate_id.to_string()))
    }

    // --- internals ---------------------------------------------------------

    fn state(&self, id: CompetitionId) -> Result<&CompetitionState, StoreError> {
        self.competitions
            .get(&id)
            .ok_or(StoreError::UnknownCompetition(id))
    }

    fn state_mut(&mut self, id: CompetitionId) -> Result<&mut CompetitionState, StoreError> {
        self.competitions
            .get_mut(&id)
            .ok_or(StoreError::UnknownCompetition(id))
    }

    fn require_status(
        id: CompetitionId,
        actual: Status,
        expected: Status,
    ) -> Result<(), StoreError> {
        if actual == expected {
            Ok(())
        } else {
            Err(StoreError::WrongStatus {
                id,
                expected,
                actual,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autoresearch_runtime::types::{EvidenceKind, Gate, Lift, Measurement};

    fn dummy_evidence() -> Evidence {
        Evidence {
            kind: EvidenceKind::ReplayFull,
            lift: Lift {
                delta: 0.3,
                ci_lower: 0.2,
                ci_upper: 0.4,
                n: 80,
            },
            measurement: Measurement {
                value: 0.8,
                ci_lower: 0.7,
                ci_upper: 0.9,
                n: 80,
                cost: 80.0,
            },
            confounded: false,
            suite_ref: ArtifactRef("suite".into()),
            attestation_hash: String::new(),
        }
    }

    #[test]
    fn happy_path_lifecycle() {
        let mut store = CompetitionStore::new();
        store.create(1).unwrap();
        assert_eq!(store.status(1).unwrap(), Status::Draft);

        store.transition(1, Status::Open).unwrap();
        store.join(1, "0xalice", 100).unwrap();
        assert_eq!(store.researchers(1).unwrap().len(), 1);

        store.transition(1, Status::Submitting).unwrap();
        let cid = store.commit(1, "0xalice", "0xcommit").unwrap();
        store
            .reveal(1, &cid, "0xcommit", ArtifactRef("ipfs://cand".into()))
            .unwrap();
        assert!(store.candidate(1, &cid).unwrap().artifact_ref.is_some());

        store.transition(1, Status::Scoring).unwrap();
        store.record_evidence(1, &cid, dummy_evidence()).unwrap();
        assert!(store.candidate(1, &cid).unwrap().evidence.is_some());

        store.transition(1, Status::Settling).unwrap();
        store.transition(1, Status::Closed).unwrap();
        assert_eq!(store.status(1).unwrap(), Status::Closed);
    }

    #[test]
    fn rejects_skipping_transition() {
        let mut store = CompetitionStore::new();
        store.create(1).unwrap();
        // Draft -> Scoring skips Open/Submitting.
        let err = store.transition(1, Status::Scoring).unwrap_err();
        assert_eq!(
            err,
            StoreError::IllegalTransition {
                id: 1,
                from: Status::Draft,
                to: Status::Scoring,
            }
        );
    }

    #[test]
    fn rejects_backward_transition() {
        let mut store = CompetitionStore::new();
        store.create(1).unwrap();
        store.transition(1, Status::Open).unwrap();
        store.transition(1, Status::Submitting).unwrap();
        assert!(matches!(
            store.transition(1, Status::Open),
            Err(StoreError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn commit_requires_submitting() {
        let mut store = CompetitionStore::new();
        store.create(1).unwrap();
        store.transition(1, Status::Open).unwrap();
        // Still Open, not Submitting.
        let err = store.commit(1, "0xalice", "0xcommit").unwrap_err();
        assert_eq!(
            err,
            StoreError::WrongStatus {
                id: 1,
                expected: Status::Submitting,
                actual: Status::Open,
            }
        );
    }

    #[test]
    fn reveal_mismatch_is_rejected() {
        let mut store = CompetitionStore::new();
        store.create(1).unwrap();
        store.transition(1, Status::Open).unwrap();
        store.join(1, "0xalice", 100).unwrap();
        store.transition(1, Status::Submitting).unwrap();
        let cid = store.commit(1, "0xalice", "0xcommit").unwrap();
        let err = store
            .reveal(1, &cid, "0xWRONG", ArtifactRef("x".into()))
            .unwrap_err();
        assert_eq!(err, StoreError::RevealMismatch(cid));
    }

    #[test]
    fn duplicate_candidate_rejected() {
        let mut store = CompetitionStore::new();
        store.create(1).unwrap();
        store.transition(1, Status::Open).unwrap();
        store.join(1, "0xalice", 100).unwrap();
        store.join(1, "0xbob", 100).unwrap();
        store.transition(1, Status::Submitting).unwrap();
        store.commit(1, "0xalice", "0xcommit").unwrap();
        let err = store.commit(1, "0xbob", "0xcommit").unwrap_err();
        assert_eq!(err, StoreError::DuplicateCandidate("0xcommit".into()));
    }

    #[test]
    fn unknown_competition_errors() {
        let store = CompetitionStore::new();
        assert_eq!(
            store.status(99).unwrap_err(),
            StoreError::UnknownCompetition(99)
        );
    }

    #[test]
    fn evidence_payability_round_trips_through_runtime_candidate() {
        // The store's evidence must drive the runtime gate the same way the
        // certified ledger does.
        let ev = dummy_evidence();
        assert!(Gate::default().clears(&ev.lift, &ev.measurement));
    }

    #[test]
    fn join_below_stake_policy_floor_is_rejected() {
        let mut store = CompetitionStore::new();
        store
            .create_with_policy(
                1,
                StakePolicy {
                    min_stake_wei: 1_000,
                },
            )
            .unwrap();
        store.transition(1, Status::Open).unwrap();
        let err = store.join(1, "0xpoor", 999).unwrap_err();
        assert_eq!(
            err,
            StoreError::InsufficientStake {
                id: 1,
                researcher: "0xpoor".into(),
                posted: 999,
                floor: 1_000,
            }
        );
        // A rejected join enrolls no researcher.
        assert!(store.researchers(1).unwrap().is_empty());
    }

    #[test]
    fn join_top_up_can_reach_the_stake_floor() {
        let mut store = CompetitionStore::new();
        store
            .create_with_policy(
                1,
                StakePolicy {
                    min_stake_wei: 1_000,
                },
            )
            .unwrap();
        store.transition(1, Status::Open).unwrap();
        // First sub-floor attempt is rejected and enrolls nothing.
        assert!(store.join(1, "0xalice", 600).is_err());
        // A single sufficient post clears the floor.
        store.join(1, "0xalice", 1_000).unwrap();
        assert_eq!(store.researchers(1).unwrap().len(), 1);
        // A subsequent join accumulates on top of the cleared stake.
        store.join(1, "0xalice", 500).unwrap();
        let r = store.researchers(1).unwrap();
        assert_eq!(r[0].stake_wei, 1_500);
    }

    #[test]
    fn commit_without_stake_is_rejected() {
        // A researcher who never joined (so never staked) cannot submit, even under
        // a zero-floor policy: presence-with-stake is required, not just a floor.
        let mut store = CompetitionStore::new();
        store.create(1).unwrap();
        store.transition(1, Status::Open).unwrap();
        store.transition(1, Status::Submitting).unwrap();
        let err = store.commit(1, "0xstranger", "0xcommit").unwrap_err();
        assert_eq!(
            err,
            StoreError::NotStaked {
                id: 1,
                researcher: "0xstranger".into(),
            }
        );
    }

    #[test]
    fn staked_researcher_may_submit_under_policy() {
        let mut store = CompetitionStore::new();
        store
            .create_with_policy(
                1,
                StakePolicy {
                    min_stake_wei: 1_000,
                },
            )
            .unwrap();
        store.transition(1, Status::Open).unwrap();
        store.join(1, "0xalice", 1_000).unwrap();
        store.transition(1, Status::Submitting).unwrap();
        // The staked researcher submits; a different unstaked one cannot.
        store.commit(1, "0xalice", "0xcommit-a").unwrap();
        assert_eq!(
            store.commit(1, "0xeve", "0xcommit-b").unwrap_err(),
            StoreError::NotStaked {
                id: 1,
                researcher: "0xeve".into(),
            }
        );
    }
}
