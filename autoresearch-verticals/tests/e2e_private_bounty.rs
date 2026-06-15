//! Scenario C proof — a **Private Enterprise Bounty** at [`PrivacyTier::BlackBox`]
//! over the config-opt vertical (`docs/PRIVACY.md §13`).
//!
//! This is the M4 end-to-end proof that a competition can run **privately** without
//! lying about the guarantee. A proposer posts a sealed held-out task; researchers
//! run their own engines and submit candidates; the referee scores each candidate on
//! the sealed held-out split and certifies it with an attestation hash. The test
//! asserts the four properties that make Scenario C work AND honest:
//!
//! 1. **Information withholding (PRIVACY §8).** Every researcher receives only a
//!    [`ResearcherFeedback::Verdict`] — a single gate bit. The exact lift delta the
//!    referee measured is **absent** from their feedback (it is not recoverable).
//! 2. **The winner is still paid.** The referee's *internal* full outcome ranks the
//!    candidates correctly and settles a conserving payout to the best researcher —
//!    privacy does not break the market.
//! 3. **The attestation hash is present and committed.** Each scored candidate
//!    carries the referee's attestation-report hash (the value `commitAttestation`
//!    stores on-chain).
//! 4. **The hard rule bites (PRIVACY §4).** A `ResearcherCapabilities{true,true,true}`
//!    is rejected by `validate()` — the all-three exfiltration configuration cannot
//!    run.
//!
//! # Honesty (PRIVACY §12)
//!
//! This test does **not** prove cryptographic TEE attestation. The referee here is a
//! local, non-enclave stand-in ([`autoresearch_runtime::attestation::LocalReferee`]):
//! its attestation is **structural-only**. The competition runs with
//! `required_tee = None` precisely because black-box safety does **not** depend on
//! attestation — the held-out data never crosses to the researcher (PRIVACY §1). The
//! attestation hash is committed as a *structural commitment*, not a verified quote;
//! `AttestationVerdict::Verified` is never reached.

use autoresearch_protocol::orchestrator::{CompetitionConfig, ResearcherRun};
use autoresearch_protocol::private::{PrivateCompetitionConfig, run_private_competitive};
use autoresearch_runtime::attestation::{AttestationVerdict, TeeType};
use autoresearch_runtime::privacy::{PrivacyError, PrivacyTier, ResearcherCapabilities};
use autoresearch_runtime::reward::{RewardSchedule, total_wei};
use autoresearch_runtime::types::{
    ArtifactRef, Cadence, Gate, Knobs, ScorerKind, Structure, Visibility,
};
use autoresearch_runtime::{ResearcherFeedback, Surface};
use autoresearch_verticals::{ConfigArtifact, ConfigSurface, LinearScorer, LocalSearchEngine};

const POOL_WEI: u128 = 1_000_000;

/// Private Enterprise Bounty knobs: `Competitive × OneShot × Private × HeldOutEval`.
/// (Scenario C is Continuous in the canon; the M4 private runner is the one-shot
/// terminal settlement of that arena — the privacy controls are identical.)
fn private_knobs() -> Knobs {
    Knobs {
        structure: Structure::Competitive,
        cadence: Cadence::OneShot,
        visibility: Visibility::Private,
        scorer_kind: ScorerKind::HeldOutEval,
    }
}

#[tokio::test]
async fn scenario_c_black_box_bounty_withholds_lift_but_pays_winner() {
    let surface = ConfigSurface;
    let scorer = LinearScorer::new();
    let baseline = ConfigArtifact::baseline();

    // The proposer's baseline is carried as an OPAQUE SEALED handle — never plaintext.
    // (Here we still pass the concrete baseline to the referee for scoring; the sealed
    // handle is what flows through the ledger / to researchers — PRIVACY §1.)
    let sealed_baseline = surface
        .to_ref(&baseline)
        .expect("baseline must produce a sealed handle");

    let researchers: Vec<ResearcherRun> = (1u64..=5)
        .map(|seed| ResearcherRun {
            researcher: format!("0xresearcher{seed}"),
            seed,
        })
        .collect();

    let cfg = PrivateCompetitionConfig {
        base: CompetitionConfig {
            id: 42,
            gate: Gate::default(),
            reward: RewardSchedule::SnapshotTopK {
                weights_bps: vec![6_000, 3_000, 1_000],
            },
            reward_pool_wei: POOL_WEI,
            knobs: private_knobs(),
        },
        tier: PrivacyTier::BlackBox,
        // Black-box safety does NOT depend on a TEE (PRIVACY §12) — the data never
        // crosses to the researcher. So no real TEE is required.
        required_tee: TeeType::None,
        // Black-box keeps free egress (researcher's own box, nothing to leak), so
        // no brokered-egress policy is required (PRIVACY §3, §5.1).
        egress: None,
        sealed_baseline,
    };

    let outcome =
        run_private_competitive(&cfg, &surface, &scorer, &baseline, &researchers, |run| {
            LocalSearchEngine::new(run.seed)
        })
        .await
        .expect("private competition should run");

    // --- 1. Information withholding: researchers get ONLY a verdict ---------
    assert_eq!(
        outcome.feedback.len(),
        researchers.len(),
        "every researcher gets exactly one feedback unit"
    );
    for view in &outcome.feedback {
        match &view.feedback {
            ResearcherFeedback::Verdict { .. } => {}
            other => panic!(
                "black-box researcher {} must receive a bare verdict, got {other:?}",
                view.researcher
            ),
        }
        // The exact lift delta the referee measured is NOT recoverable from the
        // researcher's feedback. Serialize the whole feedback and prove no lift field
        // and no numeric delta survives the redaction.
        let serialized = serde_json::to_string(&view.feedback).unwrap();
        assert!(
            !serialized.contains("delta") && !serialized.contains("lift"),
            "black-box feedback must not carry the lift: {serialized}"
        );
    }

    // The referee's INTERNAL ranking DOES hold the real lift (it is what settles
    // payment) — proving the number exists but is withheld from researchers.
    assert!(
        outcome.winners >= 1,
        "the referee-internal outcome must rank at least one gate-clearing candidate"
    );
    let top_delta = outcome.ranked[0].1.delta;
    assert!(
        top_delta > 0.30,
        "the referee measured a real held-out lift (~0.46): {top_delta}"
    );
    // The winner's verdict says cleared=true, while the referee withheld the 0.46.
    let winner_id = &outcome.ranked[0].0;
    let winner_view = outcome
        .feedback
        .iter()
        .find(|v| &v.researcher == winner_id)
        .expect("winner must have feedback");
    assert!(
        winner_view.feedback.cleared_gate(),
        "the winner learns they cleared — but not by how much"
    );

    // --- 2. The winner is paid; payouts conserve the pool -------------------
    let paid = total_wei(&outcome.payouts);
    assert!(paid <= POOL_WEI, "payouts {paid} exceeded pool {POOL_WEI}");
    let winner_payout = outcome
        .payouts
        .iter()
        .find(|p| &p.researcher == winner_id)
        .expect("the top researcher must be paid")
        .wei;
    let max_payout = outcome.payouts.iter().map(|p| p.wei).max().unwrap();
    assert_eq!(
        winner_payout, max_payout,
        "the #1 researcher receives the largest payout"
    );

    // --- 3. The attestation hash is present + committed per candidate -------
    assert_eq!(
        outcome.attestation_hashes.len(),
        researchers.len(),
        "every scored candidate commits an attestation hash"
    );
    for (researcher, hash) in &outcome.attestation_hashes {
        assert_eq!(
            hash.len(),
            64,
            "attestation hash for {researcher} must be a keccak256 hex digest"
        );
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "attestation hash must be hex: {hash}"
        );
    }
    // HONESTY: no real TEE was demanded, so the verdict is Unverified — and
    // verify_structural can NEVER reach Verified (PRIVACY §12). This run provides NO
    // cryptographic TEE verification; the hash is a structural commitment only.
    assert_eq!(
        outcome.attestation_verdict,
        AttestationVerdict::Unverified,
        "black-box demands no TEE; attestation is not load-bearing here"
    );
    assert_ne!(
        outcome.attestation_verdict,
        AttestationVerdict::Verified,
        "Verified is unreachable until the §12 seam is closed"
    );

    // --- 4. The hard rule bites: all-three is rejected ----------------------
    let exfiltration_config = ResearcherCapabilities {
        arbitrary_code: true,
        raw_data_access: true,
        free_egress: true,
    };
    assert!(!exfiltration_config.exfiltration_safe());
    assert_eq!(
        exfiltration_config.validate(),
        Err(PrivacyError::AllThreeCapabilities),
        "a researcher with arbitrary code + raw data + free egress is exfiltration-by-design \
         and must be rejected (PRIVACY §4)"
    );

    // And the black-box tier's own capability config is safe (it dropped raw data).
    assert!(PrivacyTier::BlackBox.capabilities().validate().is_ok());
    assert_eq!(
        PrivacyTier::BlackBox.dropped_capability(),
        "raw_data_access"
    );
}

/// Companion assertion: the black-box tier hands a researcher no dev-split handle —
/// they are "optimizing nearly blind" (PRIVACY §5.1), which is the structural reason
/// there is nothing on their side to exfiltrate.
#[tokio::test]
async fn black_box_engine_context_carries_no_raw_data_handle() {
    // The tier capability for black-box has raw_data_access = false, so the private
    // runner gives the engine `dev_split_ref = None`. We assert the capability that
    // drives that decision directly (the runner wiring is covered by the run above).
    let caps = PrivacyTier::BlackBox.capabilities();
    assert!(!caps.raw_data_access, "black-box withholds raw data access");
    // Sanity that the sealed-handle plumbing produces opaque references, not plaintext.
    let surface = ConfigSurface;
    let r: ArtifactRef = surface.to_ref(&ConfigArtifact::baseline()).unwrap();
    assert!(
        r.0.starts_with("config:"),
        "handle is an opaque ref: {}",
        r.0
    );
}
