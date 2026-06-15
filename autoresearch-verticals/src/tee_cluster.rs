//! TEE-backed training-cluster stand-in and the proof that the privacy tier binds
//! to the cluster that runs the training (PRIVACY §7, §12; `docs/DISTRIBUTED-TRAINING.md`).
//!
//! Phase 5 adds the one missing piece on top of the [`crate::distributed_training`]
//! vertical: a [`TrainingCluster`] that declares it runs inside a **sealed (TEE)
//! environment**, so an attestation-mandating private *training* competition has a
//! backend that clears the tier→sandbox binding the private runner enforces.
//!
//! # Why a wrapper, not a new cluster
//!
//! Sealing is orthogonal to *how* the recipe is trained: a real deployment runs the
//! exact same `prime`/Psyche training job, only inside a confidential VM. So
//! [`TeeSimCluster`] **wraps** any inner [`TrainingCluster`] and delegates
//! [`TrainingCluster::train`] to it unchanged — the only thing it overrides is
//! [`TrainingCluster::provides_sealed_isolation`], which it reports `true`. That is
//! exactly the production shape: `TeeSimCluster<PrimeCluster>` would be "the prime
//! trainer, in an enclave," with no change to the training path.
//!
//! # Honest seam — structural-only attestation (PRIVACY §12)
//!
//! [`TeeSimCluster::attestation_report`] mirrors
//! [`autoresearch_runtime::attestation::LocalReferee::attestation_report`]: it emits a
//! shape-valid [`AttestationReport`] whose canonical [`AttestationReport::hash`] is a
//! deterministic commitment — and **nothing more**. It is the *same honest gap* as the
//! existing referee attestation:
//!
//! - The report's `tee_type` is [`TeeType::None`] — this is a LOCAL stand-in, not a
//!   genuine enclave.
//! - [`verify_structural`] can therefore never return
//!   [`AttestationVerdict::Verified`] for it; the strongest reachable verdict is
//!   [`AttestationVerdict::StructurallyValid`], and only against a `None` requirement.
//! - There is **no** hardware quote-signature verification, **no** measurement pinning,
//!   and **no** nonce binding (the unimplemented §12 work). A malicious host could forge
//!   a structurally-correct report. We do not pretend otherwise.
//!
//! So [`TeeSimCluster::provides_sealed_isolation`] returning `true` is an *interface*
//! claim that lets the engine clear the protocol's tier→sandbox binding — it is **not**
//! a cryptographic guarantee. Closing that gap is the same §12 swap that turns
//! [`LocalReferee`] into a real-TEE referee.
//!
//! # What this module proves
//!
//! The [`tests`] module drives the **real** [`run_private_competitive`] runner with a
//! [`DistributedTrainingEngine`] over each cluster and shows the tier→cluster binding:
//!
//! - `DistributedTrainingEngine<LocalSimCluster>` (unsealed) on an attestation-mandating
//!   tier is rejected fail-closed with [`PrivacyError::AttestationRequired`] — an
//!   unsealed cluster cannot serve a private training competition.
//! - `DistributedTrainingEngine<TeeSimCluster<LocalSimCluster>>` (sealed) **clears** that
//!   binding (it is NOT rejected with `AttestationRequired`); it then reaches the honest
//!   §12 attestation seam, where the local referee cannot satisfy a real TEE and the run
//!   fails closed with [`PrivacyError::AttestationInvalid`]. Reaching that later guard is
//!   itself the proof the sealed cluster passed the binding the unsealed one failed.

use std::future::Future;

use autoresearch_runtime::attestation::{AttestationReport, TeeType};
use autoresearch_runtime::traits::EngineError;

use crate::distributed_training::{TrainedArtifact, TrainingCluster, TrainingRecipe};

/// A sealed (TEE) training-cluster stand-in that wraps any inner [`TrainingCluster`].
///
/// It trains **exactly** as the inner cluster does — `train` is a straight delegation —
/// and adds one thing: it declares [`TrainingCluster::provides_sealed_isolation`] is
/// `true`, so a [`DistributedTrainingEngine`](crate::distributed_training::DistributedTrainingEngine)
/// over it clears the private runner's tier→sandbox binding. In production this is
/// `TeeSimCluster<PrimeCluster>` — the same trainer, inside a confidential VM.
///
/// **Honesty (PRIVACY §12):** the sealing is an interface claim plus a *structural-only*
/// attestation (see [`Self::attestation_report`]); it is not a verified enclave. The
/// report's [`TeeType`] is [`TeeType::None`] and carries the same unverified gap as
/// [`autoresearch_runtime::attestation::LocalReferee`].
#[derive(Clone, Copy, Debug, Default)]
pub struct TeeSimCluster<C> {
    inner: C,
}

impl<C> TeeSimCluster<C> {
    /// Wrap `inner` as a sealed (TEE) cluster. Training is delegated to `inner`
    /// unchanged; only the isolation property and the attestation report are added.
    #[must_use]
    pub fn new(inner: C) -> Self {
        Self { inner }
    }

    /// The inner (unsealed) cluster this wraps.
    pub fn inner(&self) -> &C {
        &self.inner
    }
}

impl<C: TrainingCluster> TeeSimCluster<C> {
    /// The structural-only attestation report for this sealed cluster, mirroring
    /// [`autoresearch_runtime::attestation::LocalReferee::attestation_report`].
    ///
    /// **This is NOT a hardware quote (PRIVACY §12).** The `tee_type` is
    /// [`TeeType::None`] (a local stand-in), the `evidence`/`measurement` blobs are
    /// stable local-provenance markers, and there is no nonce. Its
    /// [`AttestationReport::hash`] is a deterministic commitment a disputer can pin —
    /// it does not prove a genuine enclave ran. Closing this gap is the same §12 swap
    /// (DCAP/KDS/NSM quote verification + measurement pinning + nonce binding) that
    /// makes [`autoresearch_runtime::attestation::AttestationVerdict::Verified`]
    /// reachable.
    #[must_use]
    pub fn attestation_report(&self) -> AttestationReport {
        AttestationReport {
            // Local stand-in: honestly NOT a genuine TEE. A real `TeeSimCluster` swap
            // would carry the vendor's `TeeType` and a true hardware quote here.
            tee_type: TeeType::None,
            // Stable, non-empty local-provenance markers so the report is structurally
            // well-formed (the shape `verify_structural` checks). NOT a hardware quote.
            evidence: format!("tee-sim-cluster:{}", self.inner.id()).into_bytes(),
            measurement: b"local-stand-in:no-enclave".to_vec(),
            nonce: None,
        }
    }
}

impl<C: TrainingCluster + Send + Sync> TrainingCluster for TeeSimCluster<C> {
    fn id(&self) -> &str {
        // Stable, distinct id so a sealed cluster is identifiable in provenance while
        // its training behaviour is byte-identical to the inner cluster's.
        "tee-sim-cluster"
    }

    fn train(
        &self,
        recipe: &TrainingRecipe,
        seed: u64,
    ) -> impl Future<Output = Result<TrainedArtifact, EngineError>> + Send {
        // Sealing does not change training: delegate to the inner cluster unchanged.
        self.inner.train(recipe, seed)
    }

    fn provides_sealed_isolation(&self) -> bool {
        // The one thing the wrapper adds: it declares the training runs inside a sealed
        // (TEE) environment. This is the interface bit the private runner's tier→sandbox
        // binding consults — an honest *claim* backed by a structural-only attestation
        // (`attestation_report`), NOT a verified enclave (PRIVACY §12).
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use autoresearch_protocol::orchestrator::ProtocolError;
    use autoresearch_protocol::orchestrator::{CompetitionConfig, ResearcherRun};
    use autoresearch_protocol::private::{PrivateCompetitionConfig, run_private_competitive};
    use autoresearch_runtime::attestation::{AttestationVerdict, TeeType, verify_structural};
    use autoresearch_runtime::privacy::{PrivacyError, PrivacyTier};
    use autoresearch_runtime::reward::RewardSchedule;
    use autoresearch_runtime::types::{
        ArtifactRef, Cadence, Gate, Knobs, ScorerKind, Structure, Visibility,
    };

    use crate::distributed_training::{
        DistributedTrainingEngine, DistributedTrainingScorer, DistributedTrainingSurface,
        LocalSimCluster, TrainingRecipe,
    };

    /// A private, attestation-mandating training competition config. `WhiteBoxNoEgress`
    /// is the tier whose safety materially relies on attestation
    /// ([`PrivacyTier::requires_attestation`]); `required_tee` and `egress` are set to
    /// their safe defaults so the ONLY contract left for the test to flex is the
    /// tier→cluster (sandbox) binding.
    fn whitebox_training_cfg() -> PrivateCompetitionConfig {
        let tier = PrivacyTier::WhiteBoxNoEgress;
        PrivateCompetitionConfig {
            base: CompetitionConfig {
                id: 7,
                gate: Gate::default(),
                reward: RewardSchedule::TerminalPrize,
                reward_pool_wei: 1_000_000,
                knobs: Knobs {
                    structure: Structure::Competitive,
                    cadence: Cadence::OneShot,
                    visibility: Visibility::Private,
                    scorer_kind: ScorerKind::HeldOutEval,
                },
            },
            tier,
            // Satisfy guard (a.1) "requires_attestation ⇒ required_tee != None" so the
            // run gets PAST the no-TEE check and reaches the tier→sandbox binding.
            required_tee: TeeType::PhalaTdx,
            // Satisfy guard (a.2) the fail-closed egress contract for a no-egress tier.
            egress: PrivateCompetitionConfig::default_egress_for_tier(tier, vec![]),
            sealed_baseline: ArtifactRef("sealed:training-baseline".into()),
        }
    }

    /// The honest gap, asserted directly: `TeeSimCluster`'s attestation is
    /// structural-only — a `None`-typed report that can never reach `Verified`, the
    /// same gap as the existing `LocalReferee` attestation (PRIVACY §12).
    #[test]
    fn tee_cluster_attestation_is_structural_only() {
        let cluster = TeeSimCluster::new(LocalSimCluster);
        let report = cluster.attestation_report();
        assert_eq!(
            report.tee_type,
            TeeType::None,
            "the stand-in must NOT claim a genuine TEE type"
        );
        // The hash is a real, deterministic 32-byte keccak commitment...
        assert_eq!(report.hash(), report.hash());
        assert_eq!(report.hash().len(), 64, "keccak256 = 64 hex chars");

        // ...but it can never verify as a real TEE. Against a real requirement it fails
        // closed; against `None` the strongest it reaches is StructurallyValid — NEVER
        // Verified (the unimplemented §12 cryptographic checks).
        let v_real = verify_structural(&report, TeeType::PhalaTdx);
        assert_eq!(v_real.verdict, AttestationVerdict::Failed);
        let v_none = verify_structural(&report, TeeType::None);
        assert_eq!(v_none.verdict, AttestationVerdict::StructurallyValid);
        assert_ne!(v_none.verdict, AttestationVerdict::Verified);
        assert!(!v_none.signature_verified && !v_none.measurement_matched);
    }

    /// The wrapper declares sealed isolation while delegating training byte-for-byte to
    /// the inner cluster — sealing changes the isolation property, never the result.
    #[tokio::test]
    async fn sealing_adds_isolation_without_changing_training() {
        let recipe = TrainingRecipe::baseline();
        let inner = LocalSimCluster;
        let sealed = TeeSimCluster::new(LocalSimCluster);

        assert!(!inner.provides_sealed_isolation());
        assert!(sealed.provides_sealed_isolation());

        // Same recipe + seed ⇒ identical trained artifact: delegation is transparent.
        let a = inner.train(&recipe, 11).await.unwrap();
        let b = sealed.train(&recipe, 11).await.unwrap();
        assert_eq!(a, b, "TeeSimCluster must not perturb the training result");
    }

    /// The Phase-5 proof: on an attestation-mandating tier, the **unsealed** training
    /// engine is rejected fail-closed (`AttestationRequired`), while the **sealed**
    /// (TEE) training engine CLEARS that tier→cluster binding.
    ///
    /// Honesty (PRIVACY §12): because the cluster's attestation is a LOCAL stand-in
    /// (`TeeType::None`), the sealed run does not — and must not — fully *succeed*
    /// against a real `required_tee`. It instead proceeds PAST the binding to the
    /// honest attestation seam and fails closed there with `AttestationInvalid`. That
    /// the sealed run fails LATER, with a DIFFERENT error than the unsealed one, is the
    /// proof it passed the binding the unsealed cluster could not.
    #[tokio::test]
    async fn attestation_tier_binds_to_a_sealed_training_cluster() {
        let cfg = whitebox_training_cfg();
        let surface = DistributedTrainingSurface;
        let scorer = DistributedTrainingScorer::new(16);
        let baseline = LocalSimCluster.train_sync(&TrainingRecipe::baseline(), 0);
        let researchers = vec![ResearcherRun {
            researcher: "0xtrainer".into(),
            seed: 1,
        }];
        // A tuned recipe (would clear the gate on a non-private run); the point here is
        // the binding, not the lift.
        let tuned = TrainingRecipe {
            islands: 8,
            inner_steps: 32,
            inner_lr: 3e-3,
            outer_lr: 0.7,
            keep_fraction: 0.2,
        };

        // (1) UNSEALED cluster: the engine reports `provides_sealed_isolation() == false`,
        // so the tier→sandbox binding rejects it before any data handle is built.
        let unsealed =
            run_private_competitive(&cfg, &surface, &scorer, &baseline, &researchers, |run| {
                DistributedTrainingEngine::new(
                    run.researcher.clone(),
                    tuned,
                    run.seed,
                    LocalSimCluster,
                )
            })
            .await;
        assert!(
            matches!(
                unsealed,
                Err(ProtocolError::Privacy(PrivacyError::AttestationRequired))
            ),
            "an unsealed training cluster must be rejected on an attestation tier, got {unsealed:?}"
        );

        // (2) SEALED (TEE) cluster: the engine forwards `provides_sealed_isolation() ==
        // true`, so it CLEARS the tier→sandbox binding. It is therefore NOT rejected with
        // `AttestationRequired`; it reaches the honest §12 attestation seam, where the
        // local stand-in cannot satisfy the real `PhalaTdx` requirement and the run fails
        // closed with `AttestationInvalid`.
        let sealed =
            run_private_competitive(&cfg, &surface, &scorer, &baseline, &researchers, |run| {
                DistributedTrainingEngine::new(
                    run.researcher.clone(),
                    tuned,
                    run.seed,
                    TeeSimCluster::new(LocalSimCluster),
                )
            })
            .await;
        // The discriminating assertion: the sealed run does NOT fail the binding...
        assert!(
            !matches!(
                sealed,
                Err(ProtocolError::Privacy(PrivacyError::AttestationRequired))
            ),
            "a sealed training cluster must CLEAR the tier->sandbox binding, got {sealed:?}"
        );
        // ...it instead fails closed at the honest local-referee attestation seam,
        // proving it got past the binding the unsealed cluster failed at.
        assert!(
            matches!(
                sealed,
                Err(ProtocolError::Privacy(PrivacyError::AttestationInvalid))
            ),
            "a sealed cluster on a LOCAL referee must fail closed at attestation (§12), got {sealed:?}"
        );
    }
}
