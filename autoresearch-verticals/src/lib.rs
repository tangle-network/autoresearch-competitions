//! # autoresearch-verticals
//!
//! Concrete `Surface`/`Scorer`/`Engine` triples for the autoresearch market. Each
//! vertical is one instantiation of the seams in [`autoresearch_runtime`], driven by
//! the runner in [`autoresearch_protocol`].
//!
//! The **linear-classifier demo** ([`LinearScorer`] + [`AdditiveSurface`] in
//! [`scorers`]) is a fully deterministic held-out-accuracy vertical — tune a linear
//! classifier's weights via the shared [`autoresearch_generic_engine::GenericEngine`],
//! producing a real, positive,
//! gate-clearing lift on held-out data with no external dependencies. It is the M1
//! proof that the whole loop — produce, validate, score, gate, rank, settle — works
//! end to end on one box. Its scorer is the local stand-in for the production
//! agent-profile scorer; the trait seams are identical so a production adapter drops
//! in. The improvement search itself is driven by the shared
//! [`autoresearch_generic_engine::GenericEngine`] — there is no parallel engine in
//! this crate.
//!
//! The [`scorers`] module adds the remaining three [`autoresearch_runtime::ScorerKind`]s
//! beyond `HeldOutEval`. Its centerpiece is **Scenario A — the private-oracle (quantum)
//! case** ([`scorers::PrivateOracleScorer`] + [`scorers::BlackBoxOptimizerEngine`]):
//! researchers are scored against a HIDDEN reference they never see and improve only
//! through bounded scalar queries (solve-hard / verify-easy), proven in
//! `tests/e2e_private_oracle.rs`. [`scorers::PrivilegedHardwareScorer`] and
//! [`scorers::HumanPanelScorer`] are honest local STAND-INS for a privileged-hardware
//! backend and an async human panel respectively (marked on the types).
//! [`scorers::KindDispatchScorer`] is the thin dispatch that lets a competition's
//! declared `ScorerKind` select the scorer through the unchanged generic runners.

#![forbid(unsafe_code)]

mod util;

pub mod agent_improvement;
pub mod combinatorial_solver;
pub mod distributed_training;
pub mod forecasting;
pub mod hierarchical;
pub mod nanogpt;
pub mod program_superopt;
pub mod scorers;
pub mod tee_cluster;
pub mod theorem_proving;
pub mod training_market;

pub use distributed_training::{
    DistributedTrainingEngine, DistributedTrainingScorer, DistributedTrainingSurface,
    LocalSimCluster, TrainedArtifact, TrainingCluster, TrainingRecipe,
};
pub use hierarchical::HierarchicalCluster;
pub use nanogpt::{FixedConfigEngine, NanoGptConfig, NanoGptScorer, NanoGptSurface};
pub use scorers::{
    AdditiveSurface, BlackBoxOptimizerEngine, HiddenTargetSurface, HumanPanelScorer,
    KindDispatchScorer, LinearScorer, PrivateOracleScorer, PrivilegedHardwareScorer,
    SharedSearchContributor,
};
pub use tee_cluster::TeeSimCluster;
pub use training_market::{
    ContinuousMarketOutcome, ContinuousTrainingMarket, PanelVerdict, RecipeSubmission,
    RefereeVerdict, RescorePanel, SubmissionResult,
};

// The generic-engine verticals: each is just a `Scorer` over `GenericArtifact`,
// driven by the shared `autoresearch_generic_engine::GenericEngine`.
pub use agent_improvement::AgentProfileScorer;
pub use combinatorial_solver::SolverScorer;
pub use forecasting::ForecastScorer;
pub use program_superopt::ProgramScorer;
pub use theorem_proving::ProofScorer;
