//! The three pluggable interfaces that make the market general:
//!
//! - [`Surface`]  — what may change, and how a candidate artifact is applied.
//! - [`Scorer`]   — how "better" is measured on a held-out split.
//! - [`Engine`]   — how a researcher produces candidate artifacts.
//!
//! Auto-research-for-agents is the first vertical because the Improvement-Plane
//! already provides a [`Scorer`]. Everything else (model fine-tuning, algorithm
//! superoptimization, the quantum private-oracle case) is a different
//! `Surface`/`Scorer`/`Engine` triple over the same competition machinery.
//!
//! These signatures are `(proposed)`; they pin the *shape* of the seams for M0
//! and will be refined as the first concrete adapters land in M1+.

use std::future::Future;

use crate::types::{ArtifactRef, CompetitionId, Measurement, Split};

/// Error producing or applying an artifact on a surface.
#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    #[error("artifact failed surface validation: {0}")]
    Invalid(String),
    #[error("delta could not be applied to base: {0}")]
    Apply(String),
    #[error("surface i/o error: {0}")]
    Io(String),
}

/// Error scoring an artifact.
#[derive(Debug, thiserror::Error)]
pub enum ScorerError {
    #[error("scorer backend unavailable: {0}")]
    Unavailable(String),
    #[error("insufficient statistical power: have n={have}, need n={need}")]
    InsufficientPower { have: u32, need: u32 },
    #[error("artifact rejected by scorer: {0}")]
    Rejected(String),
    #[error("scoring i/o error: {0}")]
    Io(String),
}

/// Error running a research engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine exhausted its budget before producing a candidate")]
    BudgetExhausted,
    #[error("engine backend error: {0}")]
    Backend(String),
    #[error("engine i/o error: {0}")]
    Io(String),
}

/// A `Surface` declares what part of the target may change and how a candidate
/// artifact is validated and applied onto a base. The associated `Artifact`
/// type is the strongly-typed in-memory representation (e.g. an agent profile
/// delta, a set of model weights, a source patch, a config).
pub trait Surface {
    /// The artifact representation this surface operates on.
    type Artifact;

    /// Stable identifier for the surface kind (e.g. `"agent-profile"`, `"weights"`).
    fn id(&self) -> &str;

    /// Reject artifacts that touch out-of-bounds fields or violate the surface's
    /// shape contract before any (expensive) scoring is attempted.
    fn validate(&self, artifact: &Self::Artifact) -> Result<(), SurfaceError>;

    /// Apply a candidate delta onto a base artifact, yielding the artifact that
    /// will be scored. For full-replacement surfaces this returns the delta.
    fn apply_delta(
        &self,
        base: &Self::Artifact,
        delta: &Self::Artifact,
    ) -> Result<Self::Artifact, SurfaceError>;

    /// Produce a content-addressed / sealed reference for an artifact. The
    /// resulting `ArtifactRef` is what flows through the ledger; the bytes do not.
    fn to_ref(&self, artifact: &Self::Artifact) -> Result<ArtifactRef, SurfaceError>;
}

/// A `Scorer` is the referee's measuring instrument. It runs an artifact against
/// a [`Split`] and returns a certified [`Measurement`]. Implementations may wrap
/// a held-out eval suite, a private reference oracle, privileged hardware, or a
/// human panel — the rest of the system is agnostic to which.
///
/// Returns `impl Future + Send` rather than `async fn` so implementations are
/// usable from multi-threaded executors and behind `dyn` adapters.
pub trait Scorer {
    /// The artifact representation this scorer evaluates.
    type Artifact;

    /// Stable identifier for the scorer kind.
    fn id(&self) -> &str;

    /// Score `artifact` on `split`. Only the Referee should ever pass
    /// [`Split::HeldOut`]; researcher-facing calls use [`Split::Dev`].
    fn score(
        &self,
        artifact: &Self::Artifact,
        split: Split,
    ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send;
}

/// A shared reference to a scorer is itself a scorer (forwarding impl). This lets a
/// referee wrapper (`autoresearch_runtime::attestation::LocalReferee`) hold a
/// `&Scorer` borrowed from a caller that only has a reference, without taking
/// ownership of the (dataset-holding) scorer.
impl<T: Scorer> Scorer for &T {
    type Artifact = T::Artifact;

    fn id(&self) -> &str {
        (**self).id()
    }

    fn score(
        &self,
        artifact: &Self::Artifact,
        split: Split,
    ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send {
        (**self).score(artifact, split)
    }
}

/// Context handed to an [`Engine`] when it is asked to produce a candidate:
/// what competition it is for, what to start from, what dev signal it may see,
/// how much it is allowed to spend, and what egress it may make.
#[derive(Clone, Debug)]
pub struct EngineContext {
    pub competition: CompetitionId,
    /// The baseline artifact to improve upon.
    pub baseline_ref: ArtifactRef,
    /// Dev-split reference the engine may evaluate against (privacy-tier gated).
    pub dev_split_ref: Option<ArtifactRef>,
    /// Spend ceiling for this production run, in wei-equivalent budget units.
    pub budget_wei: u128,
    /// The egress policy the engine's network access is brokered through
    /// (PRIVACY §6). `None` means an unrestricted (public-competition) context;
    /// `Some(_)` carries the allowlist decision — empty allowlist + `default_deny`
    /// is the white-box no-egress case (PRIVACY §5.3). The actual socket-level
    /// enforcement is the host-dependent broker/enclave seam (PRIVACY §12); this
    /// field carries the host-independent decision into the engine/referee seam.
    pub egress_policy: Option<crate::privacy::EgressPolicy>,
}

/// An `Engine` is the researcher's product: the automated process that searches
/// the surface for a better artifact (a sandboxed agent self-improvement loop, a
/// distributed-training run, a black-box optimizer, or a passthrough for a human
/// submission). The market never inspects *how* an engine works — only the
/// scored outcome of what it produces.
pub trait Engine {
    /// The artifact representation this engine emits.
    type Artifact;

    /// Stable identifier for the engine kind.
    fn id(&self) -> &str;

    /// Produce a candidate artifact for the given context.
    fn produce(
        &self,
        ctx: &EngineContext,
    ) -> impl Future<Output = Result<Self::Artifact, EngineError>> + Send;

    /// Whether this engine runs the researcher's method inside a sealed, TEE-isolated
    /// sandbox — so the method can touch the proposer's data without the researcher
    /// ever seeing it. Defaults to `false`: a plain in-process / non-TEE engine seals
    /// nothing. The private runner *requires* this for privacy tiers that mandate
    /// attestation (`PrivacyTier::requires_attestation`), so an unsealed engine cannot
    /// be used to run a white-box / attested-harness competition — the tier→sandbox
    /// binding is enforced at the protocol seam, not left to caller convention.
    fn provides_sealed_isolation(&self) -> bool {
        false
    }
}
