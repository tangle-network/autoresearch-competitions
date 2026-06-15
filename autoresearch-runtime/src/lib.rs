//! # autoresearch-runtime
//!
//! Core domain model and pluggable interfaces for the autoresearch-competitions
//! blueprint — a decentralized market for *verifiable improvement*.
//!
//! A proposer posts a competition = (`Surface`, `Scorer`, reward, knobs). A crowd
//! of researchers produce candidate artifacts with their own compute and methods;
//! a Referee runs the `Scorer` on a held-out split and certifies the result;
//! payment settles on-chain for proven improvement. The market pays for
//! *outcome*, not *effort*, which is what collapses verification to "run the
//! scorer" and keeps the default mode privacy-easy.
//!
//! This crate is harness-, chain-, and engine-agnostic on purpose. It defines:
//!
//! - the domain types ([`types`]) — knobs, measurements, lift, the gate,
//!   evidence, candidates, and the competition spec;
//! - the three pluggable seams ([`traits`]) — [`Surface`], [`Scorer`], [`Engine`];
//! - the reward schedules and their exact, integer settlement logic ([`reward`]);
//! - the certified-artifact marketplace ([`marketplace`]) — the competition →
//!   marketplace flywheel that turns scored inventory into sellable listings,
//!   reusing the same gate + certified-lift trust primitives a competition uses.
//!
//! The on-chain ABI layer and the operator runtime live in sibling crates and
//! depend on this one. See `SPEC.md`, `docs/ARCHITECTURE.md`, and
//! `docs/MECHANISM.md` for the full design.

#![forbid(unsafe_code)]

pub mod attestation;
pub mod marketplace;
pub mod privacy;
pub mod reward;
pub mod traits;
pub mod types;

pub use attestation::{
    AttestationReport, AttestationVerdict, LocalReferee, TeeType, VerifiedAttestation,
    verify_structural,
};
pub use marketplace::{
    ArtifactListing, CertifiedAttestation, ListingId, MarketError, Marketplace, PricingPolicy,
    Sale, price_by_lift,
};
pub use privacy::{
    EgressPolicy, FeedbackLevel, PrivacyError, PrivacyTier, ResearcherCapabilities,
    ResearcherFeedback, SubmissionBudget, redact,
};
pub use reward::{
    BPS_DENOM, Payout, RecordBeat, RewardError, RewardSchedule, settle_record_bounty,
    settle_snapshot_topk, settle_terminal_prize, settle_time_at_top, total_wei,
};
pub use traits::{Engine, EngineContext, EngineError, Scorer, ScorerError, Surface, SurfaceError};
pub use types::{
    Address, ArtifactRef, Cadence, Candidate, CompetitionId, CompetitionSpec, Evidence,
    EvidenceKind, Gate, Knobs, Lift, Measurement, ScorerKind, Split, Structure, Visibility,
};
