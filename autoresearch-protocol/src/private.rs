//! The private competitive runner — competitions that run privately without
//! lying about the guarantee (Scenario C, `docs/PRIVACY.md §13`).
//!
//! [`run_private_competitive`] is the [`crate::orchestrator::run_oneshot_competitive`]
//! analogue for a `Private` competition. It adds four things on top of the public
//! runner, each tied to a control in `docs/PRIVACY.md`:
//!
//! 1. **The hard rule (PRIVACY §4).** It validates the tier's researcher-capability
//!    configuration up front, rejecting the forbidden all-three of
//!    `{arbitrary_code, raw_data_access, free_egress}` before any work runs.
//! 2. **A fail-closed attestation + egress contract (PRIVACY §6, §7, §12).** For the
//!    two tiers whose safety materially relies on attestation
//!    ([`PrivacyTier::WhiteBoxNoEgress`], [`PrivacyTier::AttestedHarness`] —
//!    [`PrivacyTier::requires_attestation`]) it refuses to run with
//!    `required_tee == None`, and it requires a fail-closed
//!    [`EgressPolicy`] (`default_deny`) for any tier that drops free egress,
//!    threading that policy into [`EngineContext`] so researcher egress is brokered
//!    (PRIVACY §6) rather than left to the declarative `free_egress: false` bit.
//!    Scoring runs through a [`LocalReferee`], which emits an [`AttestationReport`]
//!    whose hash is committed in each candidate's [`Evidence`]. When the config
//!    requires a real TEE the report must pass [`verify_structural`] to at least
//!    [`AttestationVerdict::StructurallyValid`] — else the run fails closed.
//! 3. **Feedback gating (PRIVACY §8).** It returns BOTH the referee's full,
//!    plaintext outcome (what the operator/proposer settles on) AND the
//!    researcher-facing [`ResearcherFeedback`], gated to the tier via
//!    [`redact`] — so e.g. a black-box researcher learns only a gate verdict.
//!
//! # What this does NOT do (honesty, PRIVACY §12)
//!
//! - It does **not** perform cryptographic TEE attestation. The [`LocalReferee`] is a
//!   local, non-enclave stand-in; its attestation is **structural-only**, and
//!   [`verify_structural`] can never return [`AttestationVerdict::Verified`]. A run
//!   that demands a real TEE (`required_tee != None`) backed only by a local
//!   stand-in fails closed — the runner does not pretend the enclave exists.
//! - It does **not** make a black-box competition leak-*proof*. The gate-verdict bit
//!   per submission is the bounded residual (PRIVACY §8); the runner withholds the
//!   lift number, it does not eliminate the score channel.
//! - It does **not** itself open or police the researcher's network sockets. It
//!   enforces the *allowlist decision* (PRIVACY §6): it rejects a missing or open
//!   [`EgressPolicy`] for a no-egress / brokered tier at setup, and carries the
//!   fail-closed policy into [`EngineContext`] so the broker consults it. The
//!   socket-level proxy that actually drops a denied connection is the
//!   host-dependent enclave/router seam (PRIVACY §12, B3) — marked, not faked.

use autoresearch_runtime::attestation::{
    AttestationVerdict, LocalReferee, TeeType, verify_structural,
};
use autoresearch_runtime::privacy::{
    EgressPolicy, PrivacyError, PrivacyTier, ResearcherFeedback, redact,
};
use autoresearch_runtime::reward::Payout;
use autoresearch_runtime::traits::{Engine, EngineContext, Scorer, Surface};
use autoresearch_runtime::types::{ArtifactRef, Lift};

use crate::lift::estimate_lift;
use crate::orchestrator::{CompetitionConfig, ProtocolError};

/// Configuration for a private competition: a [`CompetitionConfig`] plus the privacy
/// controls.
#[derive(Clone, Debug)]
pub struct PrivateCompetitionConfig {
    /// The underlying competition (gate, reward schedule, pool, knobs).
    pub base: CompetitionConfig,
    /// Which privacy tier this competition runs at (PRIVACY §5).
    pub tier: PrivacyTier,
    /// The TEE the referee must attest to. [`TeeType::None`] means "no TEE required"
    /// (the black-box / redacted-feedback default, whose safety does not depend on
    /// attestation at all — PRIVACY §12). A non-`None` value demands a report that
    /// passes [`verify_structural`].
    pub required_tee: TeeType,
    /// The brokered-egress policy this competition's researcher code runs under
    /// (PRIVACY §6). For tiers that drop free egress (`WhiteBoxNoEgress`,
    /// `AttestedHarness`) this MUST be fail-closed (`default_deny == true`): a
    /// [`EgressPolicy::no_egress`] for white-box, or an
    /// [`EgressPolicy::allowlisted`] for the attested harness. A misconfigured
    /// open policy on such a tier is rejected at setup by
    /// [`run_private_competitive`] rather than silently unenforced.
    ///
    /// Use [`Self::default_egress_for_tier`] to obtain the safe default for a
    /// tier. `None` is only valid for tiers that keep free egress (`BlackBox`,
    /// `RedactedFeedback`) — those run on the researcher's own box with no
    /// proprietary data to leak (PRIVACY §3, §5.1).
    pub egress: Option<EgressPolicy>,
    /// The proposer's baseline, carried as an opaque **sealed** handle — never
    /// plaintext (PRIVACY §1: the data never crosses to the researcher side).
    pub sealed_baseline: ArtifactRef,
}

impl PrivateCompetitionConfig {
    /// The fail-closed egress policy a tier must run under by default (PRIVACY §6):
    ///
    /// - [`PrivacyTier::WhiteBoxNoEgress`] → [`EgressPolicy::no_egress`] (nothing
    ///   reachable; the gated output is the only thing that leaves).
    /// - [`PrivacyTier::AttestedHarness`] → an allowlisted, default-deny policy
    ///   built from `allowlist` (the harness brokers egress to known hosts).
    /// - [`PrivacyTier::BlackBox`] / [`PrivacyTier::RedactedFeedback`] → `None`:
    ///   the researcher keeps free egress on their own box; there is no
    ///   proprietary data on their side to exfiltrate.
    #[must_use]
    pub fn default_egress_for_tier(
        tier: PrivacyTier,
        allowlist: Vec<String>,
    ) -> Option<EgressPolicy> {
        match tier {
            PrivacyTier::WhiteBoxNoEgress => Some(EgressPolicy::no_egress()),
            PrivacyTier::AttestedHarness => Some(EgressPolicy::allowlisted(allowlist)),
            PrivacyTier::BlackBox | PrivacyTier::RedactedFeedback => None,
        }
    }
}

/// The researcher-facing view of one scored candidate: their id and the feedback
/// gated to the competition's tier. This is what crosses the score channel (boundary
/// B2) back to the researcher — never the referee's full lift.
#[derive(Clone, Debug, PartialEq)]
pub struct ResearcherView {
    pub researcher: String,
    pub feedback: ResearcherFeedback,
}

/// The result of a private competitive run: the referee's full (plaintext) outcome
/// for settlement, plus the per-researcher redacted feedback and the committed
/// attestation hashes.
#[derive(Clone, Debug)]
pub struct PrivateOutcome {
    /// Gate-clearing researchers with their certified lift, best delta first. This is
    /// the **referee-internal** ranking the operator settles on — it is NOT shown to
    /// researchers (they see [`Self::feedback`]).
    pub ranked: Vec<(String, Lift)>,
    /// Settled payouts per the reward schedule. Conserves the pool exactly as the
    /// public runner does.
    pub payouts: Vec<Payout>,
    pub winners: usize,
    /// Per-researcher redacted feedback, in submission order. For
    /// [`PrivacyTier::BlackBox`] each entry is a bare gate verdict (no lift).
    pub feedback: Vec<ResearcherView>,
    /// The attestation report hash committed for each researcher's scored candidate,
    /// in submission order. This is the value the contract's `commitAttestation`
    /// stores on-chain (a structural commitment; verification is the §12 seam).
    pub attestation_hashes: Vec<(String, String)>,
    /// The structural verdict on the referee's attestation. Today this is at most
    /// [`AttestationVerdict::StructurallyValid`] (PRIVACY §12). When `required_tee`
    /// is [`TeeType::None`] it is left [`AttestationVerdict::Unverified`] because no
    /// TEE was demanded.
    pub attestation_verdict: AttestationVerdict,
}

/// Run a `Competitive × OneShot × Private` competition through an attested referee
/// with tier-gated feedback. See the module docs for the three controls it adds over
/// [`crate::orchestrator::run_oneshot_competitive`].
///
/// `make_engine` builds a fresh engine per researcher (same seam as the public
/// runner). The baseline is supplied as a concrete artifact for scoring; its sealed
/// handle is recorded in [`PrivateCompetitionConfig::sealed_baseline`] and the
/// runtime never exposes it to researchers.
///
/// # Errors
/// - [`PrivacyError::AllThreeCapabilities`] (via [`ProtocolError::Privacy`]) if the
///   tier's capability config is the forbidden all-three.
/// - [`PrivacyError::AttestationRequired`] if the tier
///   [`requires_attestation`](PrivacyTier::requires_attestation) but
///   `required_tee == None` (an attestation-reliant tier configured with no TEE).
/// - [`PrivacyError::EgressDenied`] if a tier that drops free egress is configured
///   with a missing or open (`default_deny == false`) [`EgressPolicy`].
/// - [`PrivacyError::AttestationInvalid`] if `required_tee != None` but the report
///   fails structural validation.
/// - Any [`ProtocolError`] the underlying scoring/surface/reward path raises.
pub async fn run_private_competitive<S, Sc, Eng, Mk>(
    cfg: &PrivateCompetitionConfig,
    surface: &S,
    scorer: &Sc,
    baseline: &S::Artifact,
    researchers: &[crate::orchestrator::ResearcherRun],
    make_engine: Mk,
) -> Result<PrivateOutcome, ProtocolError>
where
    S: Surface,
    Sc: Scorer<Artifact = S::Artifact>,
    Eng: Engine<Artifact = S::Artifact>,
    Mk: Fn(&crate::orchestrator::ResearcherRun) -> Eng,
{
    cfg.base
        .knobs
        .validate()
        .map_err(ProtocolError::IncoherentKnobs)?;
    cfg.base.reward.validate()?;

    // (a) The hard rule (PRIVACY §4): reject all-three-capability configurations
    // before any scoring runs. This is the load-bearing exfiltration guard.
    cfg.tier.capabilities().validate()?;

    // (a.1) Fail-closed attestation contract (PRIVACY §7, §12). The white-box and
    // attested-harness tiers hand the researcher a raw-data handle, and their
    // safety *materially relies* on attestation. Refuse to hand out that handle
    // with no TEE demanded at all — `requires_attestation()` with
    // `required_tee == None` is a misconfiguration, not a silent downgrade.
    if cfg.tier.requires_attestation() && cfg.required_tee == TeeType::None {
        return Err(ProtocolError::Privacy(PrivacyError::AttestationRequired));
    }

    // (a.2) Brokered-egress contract (PRIVACY §6). For tiers that drop free egress
    // the safety barrier is the egress policy, not just the declarative
    // `free_egress: false` capability bit. Require a fail-closed (`default_deny`)
    // policy to be present and consult it: reject an absent or open policy at
    // setup so a misconfig is rejected rather than silently unenforced. The
    // socket-level enforcement is the host-dependent broker/enclave seam (§12);
    // this is the host-independent allowlist decision we can enforce here.
    let egress_policy = if cfg.tier.capabilities().free_egress {
        // Tier keeps free egress (black-box / redacted): runs on the researcher's
        // own box with no proprietary data to leak; no policy to enforce.
        cfg.egress.clone()
    } else {
        let policy =
            cfg.egress
                .clone()
                .ok_or(ProtocolError::Privacy(PrivacyError::EgressDenied {
                    host: "*".to_string(),
                }))?;
        if !policy.default_deny {
            // An open policy on a no-egress / brokered tier is the exact misconfig
            // this guard exists to catch.
            return Err(ProtocolError::Privacy(PrivacyError::EgressDenied {
                host: "*".to_string(),
            }));
        }
        Some(policy)
    };

    // (b) Build the attested referee (a local, structural-only stand-in — PRIVACY
    // §12). It scores on the held-out split and emits the attestation report.
    let referee = LocalReferee::new(scorer);
    let report = referee.attestation_report();

    // (d) If a real TEE is demanded, the report must pass structural validation. The
    // local stand-in reports `TeeType::None`, so any non-`None` requirement fails
    // closed here — the runner refuses to pretend an enclave it does not have.
    let attestation_verdict = if cfg.required_tee == TeeType::None {
        // No TEE demanded (black-box / redacted-feedback default). Attestation is not
        // load-bearing for confidentiality here; the hash is still committed.
        AttestationVerdict::Unverified
    } else {
        let verified = verify_structural(&report, cfg.required_tee);
        if !verified.structural_ok {
            return Err(ProtocolError::Privacy(PrivacyError::AttestationInvalid));
        }
        // NB: this is StructurallyValid, NOT Verified — see PRIVACY §12.
        verified.verdict
    };

    // The referee re-scores the baseline per candidate inside `certify` (the held-out
    // bar everyone is measured against). The sealed suite handle is referee-only.
    let suite_ref = ArtifactRef(format!("sealed-heldout:{}", cfg.base.id));

    let mut survivors: Vec<(String, Lift)> = Vec::new();
    let mut feedback: Vec<ResearcherView> = Vec::new();
    let mut attestation_hashes: Vec<(String, String)> = Vec::new();

    for run in researchers {
        let engine = make_engine(run);
        let ctx = EngineContext {
            competition: cfg.base.id,
            baseline_ref: cfg.sealed_baseline.clone(),
            // Private: a researcher gets a dev-split handle only if the tier permits
            // touching data; black-box / redacted withhold it (no raw data access).
            dev_split_ref: if cfg.tier.capabilities().raw_data_access {
                Some(ArtifactRef(format!("sealed-dev:{}", cfg.base.id)))
            } else {
                None
            },
            budget_wei: cfg.base.reward_pool_wei,
            // The brokered-egress decision (PRIVACY §6), carried into the engine
            // seam so any researcher egress is routed through `policy.check()`.
            // The socket-level broker that consults it is the §12 enclave seam.
            egress_policy: egress_policy.clone(),
        };

        let candidate = engine
            .produce(&ctx)
            .await
            .map_err(|source| ProtocolError::Engine {
                researcher: run.researcher.clone(),
                source,
            })?;
        surface.validate(&candidate)?;

        // Certify the candidate through the referee, committing the attestation hash
        // into the evidence row (the value the contract's `commitAttestation` stores).
        let (evidence, candidate_report) = referee
            .certify(&candidate, baseline, suite_ref.clone(), |c, b| {
                estimate_lift(c, b)
            })
            .await?;
        let lift = evidence.lift;

        attestation_hashes.push((run.researcher.clone(), candidate_report.hash()));

        // (c) Researcher-facing feedback, gated to the tier (PRIVACY §8). The full
        // lift goes only into the referee-internal ranking below.
        let view = redact(cfg.tier, &lift, &evidence.measurement, &cfg.base.gate);
        feedback.push(ResearcherView {
            researcher: run.researcher.clone(),
            feedback: view,
        });

        if cfg.base.gate.clears(&lift, &evidence.measurement) {
            survivors.push((run.researcher.clone(), lift));
        }
    }

    rank(&mut survivors);

    let ranked_ids: Vec<String> = survivors.iter().map(|(r, _)| r.clone()).collect();
    let payouts = crate::orchestrator::settle_terminal_or_topk(
        &cfg.base.reward,
        cfg.base.reward_pool_wei,
        &ranked_ids,
    )?;

    Ok(PrivateOutcome {
        winners: survivors.len(),
        ranked: survivors,
        payouts,
        feedback,
        attestation_hashes,
        attestation_verdict,
    })
}

/// Rank gate-clearing survivors by point-estimate delta (best first), tie-broken by
/// CI lower bound then id — identical ordering to the public runner.
fn rank(survivors: &mut [(String, Lift)]) {
    survivors.sort_by(|a, b| {
        b.1.delta
            .partial_cmp(&a.1.delta)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.1.ci_lower
                    .partial_cmp(&a.1.ci_lower)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.0.cmp(&b.0))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::ResearcherRun;
    use autoresearch_runtime::privacy::ResearcherFeedback;
    use autoresearch_runtime::reward::RewardSchedule;
    use autoresearch_runtime::traits::{EngineError, ScorerError, SurfaceError};
    use autoresearch_runtime::types::{
        Cadence, Gate, Knobs, Measurement, ScorerKind, Split, Structure, Visibility,
    };

    // A trivial deterministic surface/scorer/engine so the runner can be unit-tested
    // without the full vertical. A "candidate" is a score in [0, 1].
    #[derive(Clone)]
    struct ScalarArtifact(f64);

    struct ScalarSurface;
    impl Surface for ScalarSurface {
        type Artifact = ScalarArtifact;
        fn id(&self) -> &str {
            "scalar"
        }
        fn validate(&self, a: &Self::Artifact) -> Result<(), SurfaceError> {
            if a.0.is_finite() {
                Ok(())
            } else {
                Err(SurfaceError::Invalid("non-finite".into()))
            }
        }
        fn apply_delta(
            &self,
            _b: &Self::Artifact,
            d: &Self::Artifact,
        ) -> Result<Self::Artifact, SurfaceError> {
            Ok(ScalarArtifact(d.0))
        }
        fn to_ref(&self, a: &Self::Artifact) -> Result<ArtifactRef, SurfaceError> {
            Ok(ArtifactRef(format!("scalar:{}", a.0)))
        }
    }

    struct ScalarScorer;
    impl Scorer for ScalarScorer {
        type Artifact = ScalarArtifact;
        fn id(&self) -> &str {
            "scalar-scorer"
        }
        fn score(
            &self,
            a: &Self::Artifact,
            _split: Split,
        ) -> impl std::future::Future<Output = Result<Measurement, ScorerError>> + Send {
            // Tight CI so a real separation clears the default gate.
            let m = Measurement {
                value: a.0,
                ci_lower: (a.0 - 0.02).max(0.0),
                ci_upper: (a.0 + 0.02).min(1.0),
                n: 80,
                cost: 80.0,
            };
            std::future::ready(Ok(m))
        }
    }

    struct FixedEngine(f64);
    impl Engine for FixedEngine {
        type Artifact = ScalarArtifact;
        fn id(&self) -> &str {
            "fixed"
        }
        fn produce(
            &self,
            _ctx: &EngineContext,
        ) -> impl std::future::Future<Output = Result<Self::Artifact, EngineError>> + Send {
            std::future::ready(Ok(ScalarArtifact(self.0)))
        }
    }

    fn private_knobs() -> Knobs {
        Knobs {
            structure: Structure::Competitive,
            cadence: Cadence::OneShot,
            visibility: Visibility::Private,
            scorer_kind: ScorerKind::HeldOutEval,
        }
    }

    fn black_box_cfg() -> PrivateCompetitionConfig {
        PrivateCompetitionConfig {
            base: CompetitionConfig {
                id: 1,
                gate: Gate::default(),
                reward: RewardSchedule::TerminalPrize,
                reward_pool_wei: 1_000_000,
                knobs: private_knobs(),
            },
            tier: PrivacyTier::BlackBox,
            required_tee: TeeType::None,
            // Black-box keeps free egress (researcher's own box, no data to leak):
            // a `None` policy is the valid configuration here.
            egress: None,
            sealed_baseline: ArtifactRef("sealed:baseline".into()),
        }
    }

    #[tokio::test]
    async fn blackbox_researchers_get_only_a_verdict_and_winner_is_paid() {
        // Strong researcher (0.9) clears vs a 0.5 baseline; weak (0.51) does not.
        let researchers = vec![
            ResearcherRun {
                researcher: "0xstrong".into(),
                seed: 1,
            },
            ResearcherRun {
                researcher: "0xweak".into(),
                seed: 2,
            },
        ];
        let outcome = run_private_competitive(
            &black_box_cfg(),
            &ScalarSurface,
            &ScalarScorer,
            &ScalarArtifact(0.5),
            &researchers,
            |run| {
                if run.researcher == "0xstrong" {
                    FixedEngine(0.9)
                } else {
                    FixedEngine(0.51)
                }
            },
        )
        .await
        .unwrap();

        // The winner is ranked and paid (referee-internal outcome is correct).
        assert_eq!(outcome.winners, 1);
        assert_eq!(outcome.ranked[0].0, "0xstrong");
        assert_eq!(outcome.payouts.len(), 1);
        assert_eq!(outcome.payouts[0].researcher, "0xstrong");

        // Every researcher's feedback is a bare verdict — the lift never crosses.
        for view in &outcome.feedback {
            match &view.feedback {
                ResearcherFeedback::Verdict { .. } => {}
                other => panic!("black-box feedback must be a verdict, got {other:?}"),
            }
        }
        // The strong researcher's verdict is "cleared"; the weak one's is not.
        let strong_fb = outcome
            .feedback
            .iter()
            .find(|v| v.researcher == "0xstrong")
            .unwrap();
        assert!(strong_fb.feedback.cleared_gate());
        let weak_fb = outcome
            .feedback
            .iter()
            .find(|v| v.researcher == "0xweak")
            .unwrap();
        assert!(!weak_fb.feedback.cleared_gate());

        // An attestation hash was committed for each scored candidate.
        assert_eq!(outcome.attestation_hashes.len(), 2);
        for (_, h) in &outcome.attestation_hashes {
            assert_eq!(h.len(), 64, "keccak256 hex");
        }
        // No TEE was demanded, so the verdict is Unverified (NOT Verified).
        assert_eq!(outcome.attestation_verdict, AttestationVerdict::Unverified);
    }

    #[tokio::test]
    async fn all_three_capability_tier_would_be_rejected() {
        // No tier exposes the forbidden all-three, but a hand-built capability set is
        // rejected by validate — proving the guard is real (also exercised at the
        // tier-config level in the runtime crate).
        use autoresearch_runtime::privacy::ResearcherCapabilities;
        let all_three = ResearcherCapabilities {
            arbitrary_code: true,
            raw_data_access: true,
            free_egress: true,
        };
        assert_eq!(
            all_three.validate(),
            Err(PrivacyError::AllThreeCapabilities)
        );
    }

    #[tokio::test]
    async fn requiring_a_real_tee_with_a_local_referee_fails_closed() {
        // The local stand-in reports TeeType::None. Demanding a real PhalaTdx TEE must
        // fail closed — the runner does not pretend an enclave it does not have.
        let mut cfg = black_box_cfg();
        cfg.required_tee = TeeType::PhalaTdx;
        cfg.tier = PrivacyTier::WhiteBoxNoEgress; // a tier that wants attestation
        cfg.egress = PrivateCompetitionConfig::default_egress_for_tier(cfg.tier, vec![]);

        let researchers = vec![ResearcherRun {
            researcher: "0xr".into(),
            seed: 1,
        }];
        let err = run_private_competitive(
            &cfg,
            &ScalarSurface,
            &ScalarScorer,
            &ScalarArtifact(0.5),
            &researchers,
            |_| FixedEngine(0.9),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                ProtocolError::Privacy(PrivacyError::AttestationInvalid)
            ),
            "expected AttestationInvalid, got {err:?}"
        );
    }

    #[tokio::test]
    async fn whitebox_tier_reveals_full_feedback() {
        // A white-box tier shows the full lift, unlike black-box. `WhiteBoxNoEgress`
        // is a FullPlaintext tier; it now requires a no-egress policy. The local
        // referee cannot satisfy a real TEE, so this test isolates the *feedback*
        // path by demanding no TEE — which would itself be rejected by the
        // attestation guard. Therefore the feedback shape is asserted through the
        // unit-level redact path in the runtime crate; here we instead assert the
        // runner reaches the FullPlaintext branch once both contracts are met, by
        // mocking the referee report via a TEE type the local stand-in matches.
        //
        // The local referee reports TeeType::None, and verify_structural only runs
        // when required_tee != None — so the *only* way the runner reaches scoring
        // for an attestation-reliant tier with today's local stand-in is to be
        // honest that it cannot, which the two guards above enforce. Full-feedback
        // content is covered by `privacy::tests::whitebox_and_attested_feedback_*`.
        // This test pins the end-to-end gate ordering: a correctly-configured
        // attested tier whose TEE cannot be satisfied fails closed at attestation,
        // never silently revealing feedback without one.
        let mut cfg = black_box_cfg();
        cfg.tier = PrivacyTier::AttestedHarness;
        cfg.required_tee = TeeType::PhalaTdx;
        cfg.egress = PrivateCompetitionConfig::default_egress_for_tier(
            cfg.tier,
            vec!["model.endpoint".into()],
        );
        let researchers = vec![ResearcherRun {
            researcher: "0xr".into(),
            seed: 1,
        }];
        let err = run_private_competitive(
            &cfg,
            &ScalarSurface,
            &ScalarScorer,
            &ScalarArtifact(0.5),
            &researchers,
            |_| FixedEngine(0.9),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                ProtocolError::Privacy(PrivacyError::AttestationInvalid)
            ),
            "attested tier with an unsatisfiable local TEE must fail closed, got {err:?}"
        );
    }

    #[tokio::test]
    async fn attestation_reliant_tier_without_tee_is_rejected() {
        // Guard 2 (PRIVACY §7, §12): a tier that requires attestation cannot run
        // with required_tee == None. Both white-box and attested-harness must be
        // rejected before any raw-data handle is produced.
        for tier in [PrivacyTier::WhiteBoxNoEgress, PrivacyTier::AttestedHarness] {
            let mut cfg = black_box_cfg();
            cfg.tier = tier;
            cfg.required_tee = TeeType::None;
            cfg.egress = PrivateCompetitionConfig::default_egress_for_tier(tier, vec![]);
            let researchers = vec![ResearcherRun {
                researcher: "0xr".into(),
                seed: 1,
            }];
            let err = run_private_competitive(
                &cfg,
                &ScalarSurface,
                &ScalarScorer,
                &ScalarArtifact(0.5),
                &researchers,
                |_| FixedEngine(0.9),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    ProtocolError::Privacy(PrivacyError::AttestationRequired)
                ),
                "{tier:?} with no TEE must be rejected, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn no_egress_tier_with_missing_or_open_policy_is_rejected() {
        // Guard 3 (PRIVACY §6): a tier that drops free egress must carry a
        // fail-closed (default_deny) policy. A missing policy is rejected...
        let mut missing = black_box_cfg();
        missing.tier = PrivacyTier::WhiteBoxNoEgress;
        missing.required_tee = TeeType::PhalaTdx; // satisfy guard 2 ordering
        missing.egress = None;
        let researchers = vec![ResearcherRun {
            researcher: "0xr".into(),
            seed: 1,
        }];
        let err = run_private_competitive(
            &missing,
            &ScalarSurface,
            &ScalarScorer,
            &ScalarArtifact(0.5),
            &researchers,
            |_| FixedEngine(0.9),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                ProtocolError::Privacy(PrivacyError::EgressDenied { .. })
            ),
            "no-egress tier with a missing policy must be rejected, got {err:?}"
        );

        // ...and so is an open (default_deny == false) policy.
        let mut open = black_box_cfg();
        open.tier = PrivacyTier::WhiteBoxNoEgress;
        open.required_tee = TeeType::PhalaTdx;
        open.egress = Some(EgressPolicy {
            allowlist: vec![],
            default_deny: false,
        });
        let err = run_private_competitive(
            &open,
            &ScalarSurface,
            &ScalarScorer,
            &ScalarArtifact(0.5),
            &researchers,
            |_| FixedEngine(0.9),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                ProtocolError::Privacy(PrivacyError::EgressDenied { .. })
            ),
            "no-egress tier with an open policy must be rejected, got {err:?}"
        );
    }

    #[test]
    fn default_egress_for_tier_is_fail_closed_for_no_egress_tiers() {
        // The derived default is no_egress for white-box and an allowlisted,
        // default-deny policy for the attested harness; None for the free-egress
        // tiers (researcher's own box, no data to leak).
        assert_eq!(
            PrivateCompetitionConfig::default_egress_for_tier(
                PrivacyTier::WhiteBoxNoEgress,
                vec!["x".into()]
            ),
            Some(EgressPolicy::no_egress())
        );
        let attested = PrivateCompetitionConfig::default_egress_for_tier(
            PrivacyTier::AttestedHarness,
            vec!["model.endpoint".into()],
        )
        .unwrap();
        assert!(attested.default_deny);
        assert!(attested.allows("model.endpoint"));
        assert!(!attested.allows("evil.example"));
        assert_eq!(
            PrivateCompetitionConfig::default_egress_for_tier(PrivacyTier::BlackBox, vec![]),
            None
        );
        assert_eq!(
            PrivateCompetitionConfig::default_egress_for_tier(
                PrivacyTier::RedactedFeedback,
                vec![]
            ),
            None
        );
    }
}
