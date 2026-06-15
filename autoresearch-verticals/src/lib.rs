//! # autoresearch-verticals
//!
//! Concrete `Surface`/`Scorer`/`Engine` triples for the autoresearch market. Each
//! vertical is one instantiation of the seams in [`autoresearch_runtime`], driven by
//! the runner in [`autoresearch_protocol`].
//!
//! The first vertical, [`config_opt`], is a fully deterministic config-optimization
//! demo (tune a linear classifier) that produces a real, positive, gate-clearing
//! lift on held-out data with no external dependencies. It is the M1 proof that the
//! whole loop — produce, validate, score, gate, rank, settle — works end to end on
//! one box. Its scorer/engine are the local stand-ins for the production
//! Improvement-Plane scorer and sandbox agent-loop engine; the trait seams are
//! identical so production adapters drop in.
//!
//! The [`scorers`] module adds the remaining three [`autoresearch_runtime::ScorerKind`]s
//! beyond `HeldOutEval`. Its centerpiece is **Scenario A — the private-oracle (quantum)
//! case** ([`PrivateOracleScorer`] + [`BlackBoxOptimizerEngine`]): researchers are
//! scored against a HIDDEN reference they never see and improve only through bounded
//! scalar queries (solve-hard / verify-easy), proven in `tests/e2e_private_oracle.rs`.
//! [`PrivilegedHardwareScorer`] and [`HumanPanelScorer`] are honest local STAND-INS for
//! a privileged-hardware backend and an async human panel respectively (marked on the
//! types). [`KindDispatchScorer`] is the thin dispatch that lets a competition's
//! declared `ScorerKind` select the scorer through the unchanged generic runners.

#![forbid(unsafe_code)]

pub mod config_opt;
pub mod nanogpt;
pub mod scorers;

pub use config_opt::{
    ConfigArtifact, ConfigSurface, LinearScorer, LocalSearchEngine, SharedSearchContributor,
};
pub use nanogpt::{FixedConfigEngine, NanoGptConfig, NanoGptScorer, NanoGptSurface};
pub use scorers::{
    BlackBoxOptimizerEngine, HiddenTargetSurface, HumanPanelScorer, KindDispatchScorer,
    PrivateOracleScorer, PrivilegedHardwareScorer,
};
