//! End-to-end proof of the operator-compute seam: the OPERATOR runs a
//! RESEARCHER-SUBMITTED method on sandboxed compute, against the proposer's sealed
//! target, and produces a REAL improved candidate — with the TEE/no-TEE toggle as one
//! field, tested both ways, fully deterministic and with no Docker.
//!
//! The product model under test:
//! - The researcher SUBMITS a method (`method_ref`). They do not bring compute.
//! - The operator PROVIDES the sandbox ([`LocalSandboxHost`], the in-process stand-in)
//!   and RUNS the method inside it via [`SandboxMethodEngine`].
//! - The Referee ([`LinearScorer`] on the held-out split) scores the result.
//!
//! The "method" here is the deterministic config-opt search ([`LocalSearchEngine`]):
//! it improves a [`ConfigArtifact`] toward the ground-truth separator, so the operator
//! flow yields a real held-out lift over the baseline — nothing mocked.

use std::future::Future;

use autoresearch_protocol::{CompetitionConfig, ResearcherRun, run_oneshot_competitive};
use autoresearch_runtime::attestation::{TeeType, verify_structural};
use autoresearch_runtime::privacy::EgressPolicy;
use autoresearch_runtime::reward::{RewardSchedule, total_wei};
use autoresearch_runtime::traits::EngineContext;
use autoresearch_runtime::types::{
    ArtifactRef, Cadence, CompetitionId, Gate, Knobs, ScorerKind, Structure, Visibility,
};
use autoresearch_sandbox::{
    LocalMethod, LocalSandboxHost, SandboxBackend, SandboxError, SandboxHandle, SandboxHost,
    SandboxMethodEngine, SandboxProvisionReq,
};
use autoresearch_verticals::{ConfigArtifact, ConfigSurface, LinearScorer, LocalSearchEngine};

const POOL_WEI: u128 = 1_000_000;

fn knobs() -> Knobs {
    Knobs {
        structure: Structure::Competitive,
        cadence: Cadence::OneShot,
        visibility: Visibility::Public,
        scorer_kind: ScorerKind::HeldOutEval,
    }
}

/// The in-process stand-in for the researcher's submitted method, executed by the
/// operator. It runs the deterministic config-opt search seeded by the submitted
/// method's reference, so it produces a genuinely improved [`ConfigArtifact`] just as
/// the real method would when run inside the sandbox.
struct ConfigSearchMethod;

impl LocalMethod for ConfigSearchMethod {
    type Artifact = ConfigArtifact;

    fn run(
        &self,
        method: &ArtifactRef,
        _ctx: &EngineContext,
    ) -> Result<Self::Artifact, SandboxError> {
        // Derive the search seed deterministically from the submitted method ref, so
        // two distinct submitted methods produce distinct (but each good) candidates —
        // exactly the spread `run_oneshot_competitive` needs to rank.
        let seed = seed_from_ref(method);
        Ok(LocalSearchEngine::new(seed).produce_candidate())
    }
}

/// Deterministic seed from a method reference (the researcher's submitted-method id).
fn seed_from_ref(method: &ArtifactRef) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in method.0.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

fn method_ref(seed: u64) -> ArtifactRef {
    ArtifactRef(format!("method:config-search:{seed}"))
}

fn run_ctx(id: CompetitionId) -> EngineContext {
    EngineContext {
        competition: id,
        baseline_ref: ArtifactRef("sealed:baseline".into()),
        dev_split_ref: Some(ArtifactRef("sealed:dev".into())),
        budget_wei: POOL_WEI,
        egress_policy: None,
    }
}

// ---------------------------------------------------------------------------
// 1. The operator runs the researcher's method and produces REAL lift.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn operator_runs_submitted_method_and_produces_real_lift() {
    let surface = ConfigSurface;
    let scorer = LinearScorer::new();
    let baseline = ConfigArtifact::baseline();

    // Five researchers, each SUBMITTING a distinct method (distinct seed). The operator
    // runs each method inside a (local stand-in) sandbox via SandboxMethodEngine.
    let researchers: Vec<ResearcherRun> = (1u64..=5)
        .map(|seed| ResearcherRun {
            researcher: format!("0xresearcher{seed}"),
            seed,
        })
        .collect();

    let cfg = CompetitionConfig {
        id: 1,
        gate: Gate::default(),
        reward: RewardSchedule::SnapshotTopK {
            weights_bps: vec![5_000, 3_000, 2_000],
        },
        reward_pool_wei: POOL_WEI,
        knobs: knobs(),
    };

    // The operator's compute: one host shared across researchers (one operator, many
    // method runs). Public competition => Docker (no-TEE) backend; here Local stands in.
    let outcome =
        run_oneshot_competitive(&cfg, &surface, &scorer, &baseline, &researchers, |run| {
            SandboxMethodEngine::new(
                method_ref(run.seed),
                SandboxBackend::Local,
                LocalSandboxHost::new(ConfigSearchMethod),
            )
        })
        .await
        .expect("operator-hosted competition should run");

    // A real, large held-out improvement flowed all the way through the operator path.
    assert!(outcome.winners >= 1, "expected gate-clearing winners");
    let top_delta = outcome.ranked[0].1.delta;
    assert!(
        top_delta > 0.30,
        "operator-run method must yield real held-out lift, got {top_delta}"
    );
    // Payouts conserve the pool, exactly as the in-process path.
    assert!(total_wei(&outcome.payouts) <= POOL_WEI);
}

// ---------------------------------------------------------------------------
// 2. The TEE/no-TEE toggle: one field, both ways, same engine/method.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tee_toggle_provisions_with_sealed_inputs_and_captured_attestation() {
    let host = LocalSandboxHost::new(ConfigSearchMethod);

    // TEE backend: is_tee, requires sealed inputs, captures a structural attestation.
    let tee = SandboxBackend::Tee(TeeType::PhalaTdx);
    assert!(tee.is_tee());
    assert!(tee.requires_sealed_inputs());

    let req = SandboxProvisionReq::new(tee, ArtifactRef("sealed:target".into()), POOL_WEI)
        .with_egress(Some(EgressPolicy::no_egress()));
    let handle = host.provision(&req).await.expect("TEE provision");
    assert!(handle.is_tee(), "TEE handle must report is_tee");
    let report = handle
        .tee_attestation
        .as_ref()
        .expect("TEE provision must capture an attestation");
    // It is at least structurally valid — but NEVER Verified (PRIVACY §12).
    let v = verify_structural(report, TeeType::PhalaTdx);
    assert!(v.is_structurally_valid());
    assert_ne!(
        v.verdict,
        autoresearch_runtime::attestation::AttestationVerdict::Verified,
        "the local stand-in attestation is structural-only, never Verified"
    );
    host.teardown(handle).await.unwrap();
}

#[tokio::test]
async fn non_tee_backends_have_no_sealing_and_no_attestation() {
    let host = LocalSandboxHost::new(ConfigSearchMethod);
    for backend in [SandboxBackend::Docker, SandboxBackend::Local] {
        assert!(!backend.is_tee(), "{backend:?} must not be a TEE backend");
        assert!(!backend.requires_sealed_inputs());
        let req = SandboxProvisionReq::new(backend, ArtifactRef("target".into()), POOL_WEI);
        let handle = host.provision(&req).await.expect("non-TEE provision");
        assert!(!handle.is_tee());
        assert!(
            handle.tee_attestation.is_none(),
            "{backend:?} must not capture an attestation"
        );
        host.teardown(handle).await.unwrap();
    }
}

#[tokio::test]
async fn the_same_method_works_under_both_backends_with_one_field_changed() {
    // The "easy toggle" guarantee: the SAME engine/method produces the SAME candidate
    // under Docker and under TEE — only the `backend` field differs.
    let surface = ConfigSurface;
    let scorer = LinearScorer::new();
    let baseline = ConfigArtifact::baseline();

    async fn winners_under(
        backend: SandboxBackend,
        surface: &ConfigSurface,
        scorer: &LinearScorer,
        baseline: &ConfigArtifact,
    ) -> f64 {
        // TEE requires a fail-closed egress; supply no-egress for the TEE arm. The
        // public/Docker arm has no proprietary data to protect, so no policy is needed.
        let researchers = vec![ResearcherRun {
            researcher: "0xr".into(),
            seed: 7,
        }];
        let cfg = CompetitionConfig {
            id: 9,
            gate: Gate::default(),
            reward: RewardSchedule::TerminalPrize,
            reward_pool_wei: POOL_WEI,
            knobs: knobs(),
        };
        // For TEE we wrap the engine through a private-style context with no-egress via
        // the public runner by threading egress through the engine's provision: here we
        // exercise the engine directly so the toggle is the ONLY change.
        let out = run_oneshot_competitive(&cfg, surface, scorer, baseline, &researchers, |run| {
            let host = LocalSandboxHost::new(ConfigSearchMethod);
            let engine = SandboxMethodEngine::new(method_ref(run.seed), backend, host);
            TeeAwareEngine { inner: engine }
        })
        .await
        .expect("toggle run");
        out.ranked.first().map(|(_, l)| l.delta).unwrap_or(0.0)
    }

    let docker_delta = winners_under(SandboxBackend::Docker, &surface, &scorer, &baseline).await;
    let tee_delta = winners_under(
        SandboxBackend::Tee(TeeType::PhalaTdx),
        &surface,
        &scorer,
        &baseline,
    )
    .await;

    assert!(docker_delta > 0.30, "Docker arm must produce real lift");
    // Byte-identical lift: flipping the one toggle field changes WHERE the method runs,
    // not WHAT it produces (the determinism the easy-toggle guarantee promises).
    assert_eq!(
        docker_delta, tee_delta,
        "the same method must yield the same lift under Docker and TEE"
    );
}

/// A thin wrapper that supplies a fail-closed no-egress policy for the TEE backend
/// (and leaves Docker/Local untouched), so the SAME `SandboxMethodEngine` can be driven
/// through the public runner under either backend with only the toggle field changed.
/// This is the consistency the engine itself enforces via `SandboxProvisionReq::validate`.
struct TeeAwareEngine {
    inner: SandboxMethodEngine<LocalSandboxHost<ConfigSearchMethod>>,
}

impl autoresearch_runtime::traits::Engine for TeeAwareEngine {
    type Artifact = ConfigArtifact;
    fn id(&self) -> &str {
        "tee-aware-sandbox-method"
    }
    fn produce(
        &self,
        ctx: &EngineContext,
    ) -> impl Future<Output = Result<Self::Artifact, autoresearch_runtime::traits::EngineError>> + Send
    {
        // For a TEE backend, thread a no-egress policy into the context so the engine's
        // provision validates fail-closed; otherwise pass the context through.
        let mut owned = ctx.clone();
        if self.inner.backend.is_tee() {
            owned.egress_policy = Some(EgressPolicy::no_egress());
        }
        // Own the context for the whole future so the RPITIT-captured borrow is local.
        async move { self.inner.produce(&owned).await }
    }
}

// ---------------------------------------------------------------------------
// 3. TEE + no-egress is enforced fail-closed.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tee_provision_with_open_egress_is_rejected_fail_closed() {
    let host = LocalSandboxHost::new(ConfigSearchMethod);
    let tee = SandboxBackend::Tee(TeeType::PhalaTdx);

    // An OPEN egress policy on a TEE backend defeats the confidentiality the TEE is for.
    let open = SandboxProvisionReq::new(tee, ArtifactRef("sealed:target".into()), POOL_WEI)
        .with_egress(Some(EgressPolicy {
            allowlist: vec![],
            default_deny: false,
        }));
    let err = host.provision(&open).await.unwrap_err();
    assert!(
        matches!(err, SandboxError::EgressNotFailClosed),
        "open-egress TEE provision must be rejected, got {err:?}"
    );

    // A MISSING egress policy on a TEE backend is equally rejected (must be explicit).
    let missing = SandboxProvisionReq::new(tee, ArtifactRef("sealed:target".into()), POOL_WEI);
    let err = host.provision(&missing).await.unwrap_err();
    assert!(
        matches!(err, SandboxError::EgressNotFailClosed),
        "missing-egress TEE provision must be rejected, got {err:?}"
    );

    // A no-egress policy on the same TEE backend provisions cleanly.
    let ok = SandboxProvisionReq::new(tee, ArtifactRef("sealed:target".into()), POOL_WEI)
        .with_egress(Some(EgressPolicy::no_egress()));
    let handle = host.provision(&ok).await.expect("no-egress TEE provision");
    assert!(handle.is_tee());
    host.teardown(handle).await.unwrap();

    // Docker is NOT subject to the no-egress requirement (public, no data to protect).
    let docker = SandboxProvisionReq::new(
        SandboxBackend::Docker,
        ArtifactRef("target".into()),
        POOL_WEI,
    );
    assert!(host.provision(&docker).await.is_ok());
}

// ---------------------------------------------------------------------------
// 4. Honesty: LocalSandboxHost is a stand-in; the real backend is separate.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_host_is_an_honest_in_process_stand_in() {
    let host = LocalSandboxHost::new(ConfigSearchMethod);
    let req = SandboxProvisionReq::new(SandboxBackend::Local, ArtifactRef("t".into()), POOL_WEI);
    let handle: SandboxHandle = host.provision(&req).await.unwrap();
    // The "sidecar url" is an in-process marker, NOT a real container endpoint — the
    // structural proof this host runs nothing real.
    assert!(
        handle.sidecar_url.starts_with("inproc://"),
        "local host must use an in-process marker url, got {}",
        handle.sidecar_url
    );
    // It still runs the method end-to-end and produces a real candidate ref.
    let candidate = host
        .run_method(&handle, &method_ref(7), &run_ctx(1))
        .await
        .unwrap();
    assert!(host.resolve(&candidate).is_some());
    host.teardown(handle).await.unwrap();
}
