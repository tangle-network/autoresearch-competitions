//! [`SandboxMethodEngine`] — the researcher method, run by the operator.
//!
//! This is the [`Engine`](autoresearch_runtime::traits::Engine) the orchestrator
//! drives, but instead of running the search in-process on the researcher's behalf it
//! hands the work to the operator-provided sandbox: it **provisions** a sandbox (with
//! the chosen [`SandboxBackend`] and the egress decision from the run context),
//! **runs the researcher's submitted method** inside it against the proposer's sealed
//! target + dev split, returns the produced candidate, and **tears the sandbox down**.
//!
//! It replaces the in-process stand-in (the shared `GenericEngine` running on the
//! researcher's side) with **operator-hosted execution**: the researcher submits a
//! method ([`method_ref`](SandboxMethodEngine::method_ref)); the operator provides and
//! runs the sandbox. With [`SandboxBackend::Tee`] + no-egress the method can improve
//! the target but cannot exfiltrate the proposer's data.

use std::future::Future;

use autoresearch_runtime::traits::{Engine, EngineContext, EngineError};
use autoresearch_runtime::types::ArtifactRef;

use crate::host::{SandboxBackend, SandboxError, SandboxHost, SandboxProvisionReq};

/// Resolve a candidate [`ArtifactRef`] produced inside a sandbox back into the typed
/// artifact the orchestrator scores.
///
/// A real backend reads the candidate bytes out of the sandbox (output-gated) and
/// deserializes them; the in-process [`LocalSandboxHost`](crate::LocalSandboxHost)
/// looks the materialized candidate up in its store. Either way this is the seam where
/// the opaque ref the [`SandboxHost`] trait speaks becomes a `Surface::Artifact`.
pub trait ResolveCandidate {
    /// The typed artifact this host resolves candidate refs into.
    type Artifact;

    /// Resolve `candidate` produced by [`SandboxHost::run_method`] into the typed
    /// artifact. `None` if the ref is unknown to this host.
    fn resolve_candidate(&self, candidate: &ArtifactRef) -> Option<Self::Artifact>;
}

impl<M> ResolveCandidate for crate::LocalSandboxHost<M>
where
    M: crate::local::LocalMethod,
{
    type Artifact = M::Artifact;

    fn resolve_candidate(&self, candidate: &ArtifactRef) -> Option<Self::Artifact> {
        self.resolve(candidate)
    }
}

/// The candidate an operator-hosted method run produces: the typed artifact plus the
/// opaque reference it was produced under inside the sandbox. The engine's
/// `Artifact = A` (the typed candidate) so it drops straight into the orchestrator;
/// this wrapper is the resolved value the engine returns.
pub type SandboxCandidate<A> = A;

/// The researcher method, executed by the operator on sandboxed compute.
///
/// Holds the researcher's submitted method reference, the [`SandboxBackend`] toggle,
/// and the operator's [`SandboxHost`]. As an [`Engine`] its `produce` is the full
/// operator job: provision → run the method → resolve the candidate → tear down.
///
/// `H` is the operator-compute backend; it must both run a method ([`SandboxHost`])
/// and resolve the produced candidate into the orchestrator's artifact type
/// ([`ResolveCandidate`]).
pub struct SandboxMethodEngine<H>
where
    H: SandboxHost + ResolveCandidate,
{
    /// The researcher's submitted method — the auto-research agent / code the operator
    /// runs. The researcher submits THIS; they do not bring compute.
    pub method_ref: ArtifactRef,
    /// The TEE/no-TEE toggle for this run. Flip [`SandboxBackend::Docker`] to
    /// [`SandboxBackend::Tee`] and nothing else changes.
    pub backend: SandboxBackend,
    /// The operator-provided sandbox host (the compute).
    host: H,
    /// Resource limits forwarded into the provision request.
    cpu_cores: u64,
    memory_mb: u64,
    disk_gb: u64,
}

impl<H> SandboxMethodEngine<H>
where
    H: SandboxHost + ResolveCandidate,
{
    /// Build the engine for a researcher method on a chosen backend, hosted by `host`.
    /// Resource limits default to a small sandbox; override with
    /// [`SandboxMethodEngine::with_limits`].
    #[must_use]
    pub fn new(method_ref: ArtifactRef, backend: SandboxBackend, host: H) -> Self {
        Self {
            method_ref,
            backend,
            host,
            cpu_cores: 2,
            memory_mb: 2_048,
            disk_gb: 10,
        }
    }

    /// Override the sandbox resource limits.
    #[must_use]
    pub fn with_limits(mut self, cpu_cores: u64, memory_mb: u64, disk_gb: u64) -> Self {
        self.cpu_cores = cpu_cores;
        self.memory_mb = memory_mb;
        self.disk_gb = disk_gb;
        self
    }

    /// The provision request this engine builds from a run context: it carries the
    /// backend toggle, the sealed target (`ctx.baseline_ref`), the dev-split handle,
    /// and the egress decision (`ctx.egress_policy`) straight through. For a TEE
    /// backend the egress must already be fail-closed — [`SandboxProvisionReq::validate`]
    /// (called inside `provision`) rejects an open one.
    fn provision_req(&self, ctx: &EngineContext) -> SandboxProvisionReq {
        SandboxProvisionReq {
            backend: self.backend,
            sealed_target: ctx.baseline_ref.clone(),
            dev_split: ctx.dev_split_ref.clone(),
            egress: ctx.egress_policy.clone(),
            budget_wei: ctx.budget_wei,
            cpu_cores: self.cpu_cores,
            memory_mb: self.memory_mb,
            disk_gb: self.disk_gb,
        }
    }
}

/// Map a [`SandboxError`] from the operator-compute seam onto the orchestrator's
/// [`EngineError`] so a sandbox failure aborts the run exactly like any engine
/// failure (the runner treats it as a per-researcher engine error).
fn to_engine_error(err: SandboxError) -> EngineError {
    match err {
        // A fail-closed egress rejection is a misconfiguration of the run, not a
        // backend hiccup; surface it as a backend error with its precise message.
        SandboxError::EgressNotFailClosed
        | SandboxError::Provision(_)
        | SandboxError::Execution(_)
        | SandboxError::Output(_)
        | SandboxError::Teardown(_)
        | SandboxError::Unavailable(_) => EngineError::Backend(err.to_string()),
    }
}

impl<H> Engine for SandboxMethodEngine<H>
where
    H: SandboxHost + ResolveCandidate + Sync,
    <H as ResolveCandidate>::Artifact: Send,
{
    type Artifact = <H as ResolveCandidate>::Artifact;

    fn id(&self) -> &str {
        "sandbox-method"
    }

    /// Sealed iff the backend is a TEE enclave. This is what lets the protocol's
    /// private runner require sealing for the white-box / attested-harness tiers:
    /// flipping the backend to [`SandboxBackend::Tee`] is what makes the method-run
    /// safe, so the binding is a tested invariant, not a convention.
    fn provides_sealed_isolation(&self) -> bool {
        self.backend.is_tee()
    }

    fn produce(
        &self,
        ctx: &EngineContext,
    ) -> impl Future<Output = Result<Self::Artifact, EngineError>> + Send {
        let req = self.provision_req(ctx);
        // Own the context so the returned future does not borrow the caller's `ctx`
        // (it must live across `run_method`'s await; cloning keeps `produce` callable
        // from wrappers that build a transient context).
        let ctx = ctx.clone();
        async move {
            // 1. Operator provisions the sandbox (TEE toggle + egress threaded in;
            //    fail-closed on an open-egress TEE provision).
            let handle = self.host.provision(&req).await.map_err(to_engine_error)?;

            // 2. Operator runs the researcher's submitted method inside the sandbox
            //    against the sealed target + dev split.
            let run = self.host.run_method(&handle, &self.method_ref, &ctx).await;

            // 3. Resolve the produced candidate into the typed artifact, then ALWAYS
            //    tear the sandbox down — even on a run failure — so a failing method
            //    never leaks operator compute.
            let resolved = match run {
                Ok(candidate_ref) => self.host.resolve_candidate(&candidate_ref).ok_or_else(|| {
                    EngineError::Backend(format!(
                        "produced candidate {candidate_ref:?} could not be resolved \
                             to a typed artifact (sandbox host wiring error)"
                    ))
                }),
                Err(e) => Err(to_engine_error(e)),
            };

            // Teardown failure must not mask a good candidate, but is surfaced when the
            // run otherwise succeeded.
            let teardown = self.host.teardown(handle).await;
            let candidate = resolved?;
            teardown.map_err(to_engine_error)?;
            Ok(candidate)
        }
    }
}
