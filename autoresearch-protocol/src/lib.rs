//! # autoresearch-protocol
//!
//! Off-chain orchestration for the autoresearch-competitions market. This crate
//! sits between the harness-agnostic [`autoresearch_runtime`] domain model and the
//! concrete verticals: it owns the competition lifecycle state machine, the lift
//! estimator, the one-shot competitive runner that wires the
//! `Surface`/`Scorer`/`Engine` seams into a measured ranking and conserving payouts,
//! and the [`collaborative`] runner — the other half of the four-knob model — which
//! pools contributors onto one shared artifact and pays by held-out-gated,
//! single-permutation marginal contribution (a first-difference estimator over a
//! canonical fold order, not a permutation-invariant Shapley value; see
//! [`collaborative`]) (`docs/MECHANISM.md §6`).
//!
//! The chain remains the source of truth for money and commitments; this crate is
//! the operator/Referee working logic that computes *what* to pay. See
//! `docs/ARCHITECTURE.md` for the on-chain/off-chain split.

#![forbid(unsafe_code)]

pub mod collaborative;
pub mod continuous;
pub mod dispute;
pub mod lift;
pub mod orchestrator;
pub mod private;
pub mod slash;
pub mod stake;
pub mod store;

pub use collaborative::{CollaborativeOutcome, Contribution, attribute_shares, run_collaborative};
pub use continuous::{
    ContinuousArena, ContinuousSchedule, EntryKind, LeaderboardEntry, SubmitOutcome, to_micros,
};
pub use dispute::{DisputeOutcome, ValidatorVerdict, collect_verdicts, committee_verdict};
pub use lift::estimate_lift;
pub use orchestrator::{
    CompetitionConfig, CompetitionOutcome, ProtocolError, ResearcherRun, run_oneshot_competitive,
    settle_terminal_or_topk,
};
pub use private::{
    PrivateCompetitionConfig, PrivateOutcome, ResearcherView, run_private_competitive,
};
pub use slash::{SlashPolicy, SlashResolution, resolve_dispute};
pub use stake::{StakeError, StakeLedger, StakePolicy};
pub use store::{CandidateRecord, CompetitionStore, Researcher, Status, StoreError};
