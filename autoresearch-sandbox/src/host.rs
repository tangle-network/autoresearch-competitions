//! The host seam: the [`SandboxBackend`] toggle, the provision request, the handle,
//! and the [`SandboxHost`] trait every operator-compute backend implements.

use std::future::Future;

use autoresearch_runtime::attestation::{AttestationReport, TeeType};
use autoresearch_runtime::privacy::EgressPolicy;
use autoresearch_runtime::traits::EngineContext;
use autoresearch_runtime::types::ArtifactRef;

/// The TEE / no-TEE toggle — **one field** selects how the operator runs a method.
///
/// This is the single switch the product model hinges on: flip
/// [`SandboxBackend::Docker`] to [`SandboxBackend::Tee`] and the same method, engine,
/// and scorer run unchanged, just inside a sealed enclave instead of a plain
/// container. A backend maps onto the real `sandbox-runtime` `runtime_backend`
/// (`"docker"` / `"firecracker"` / `"tee"`) plus a `tee_required` bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxBackend {
    /// In-process, deterministic, no Docker / no network. The test/dev backend
    /// ([`crate::LocalSandboxHost`]) runs the method as in-process logic. Not
    /// confidential; the operator host sees everything.
    Local,
    /// A real, plain Docker sandbox (no-TEE). The operator provides the compute and
    /// runs the method in an isolated container, but the container is **not**
    /// confidential against the host operator.
    Docker,
    /// A sealed TEE enclave of the given [`TeeType`]. The operator runs the method
    /// inside a confidential VM; inputs are sealed and a (structural) attestation is
    /// captured. The confidentiality boundary is the enclave-vs-host boundary (B4),
    /// not researcher-vs-data — that is the no-egress control (PRIVACY §5.3, M4).
    Tee(TeeType),
}

impl SandboxBackend {
    /// Whether this backend runs inside a TEE enclave. The load-bearing predicate the
    /// toggle test asserts on.
    #[must_use]
    pub fn is_tee(&self) -> bool {
        matches!(self, SandboxBackend::Tee(_))
    }

    /// The [`TeeType`] this backend attests to. [`TeeType::None`] for the non-TEE
    /// backends ([`SandboxBackend::Local`], [`SandboxBackend::Docker`]).
    #[must_use]
    pub fn tee_type(&self) -> TeeType {
        match self {
            SandboxBackend::Tee(t) => *t,
            SandboxBackend::Local | SandboxBackend::Docker => TeeType::None,
        }
    }

    /// Whether this backend requires the proposer's inputs to be **sealed** before
    /// they enter the sandbox. Only the TEE backend does: sealed inputs are decrypted
    /// only inside the attested enclave (the `sandbox-runtime` sealed-secrets path).
    /// Docker and Local receive the (already operator-side) handle directly.
    #[must_use]
    pub fn requires_sealed_inputs(&self) -> bool {
        self.is_tee()
    }

    /// The `sandbox-runtime` `runtime_backend` selector this maps to (`"docker"` /
    /// `"tee"`). [`SandboxBackend::Local`] has no real-runtime mapping (it never
    /// reaches `sandbox-runtime`) and reports `"local"`.
    #[must_use]
    pub fn runtime_backend(&self) -> &'static str {
        match self {
            SandboxBackend::Local => "local",
            SandboxBackend::Docker => "docker",
            SandboxBackend::Tee(_) => "tee",
        }
    }

    /// Select the operator-compute backend a competition should run on, from its
    /// privacy tier and the TEE the referee/operator must attest to.
    ///
    /// This is the single decision the TEE/no-TEE toggle encodes at the competition
    /// level, kept consistent everywhere it is made:
    ///
    /// - A tier whose safety relies on attestation
    ///   ([`PrivacyTier::requires_attestation`] — `WhiteBoxNoEgress` /
    ///   `AttestedHarness`) with a real `required_tee` (`!= None`) selects
    ///   [`SandboxBackend::Tee`]: the method runs in a sealed enclave with sealed
    ///   inputs, no-egress enforced (PRIVACY §5.3, M4).
    /// - Any other tier (black-box / redacted, or no TEE demanded) selects
    ///   [`SandboxBackend::Docker`]: a plain operator container — there is no
    ///   proprietary data on the researcher side to protect (PRIVACY §3).
    ///
    /// A test/dev caller substitutes [`SandboxBackend::Local`] for the Docker arm to
    /// keep the suite Docker-free; the toggle field is the only thing that changes.
    #[must_use]
    pub fn for_competition(
        tier: autoresearch_runtime::privacy::PrivacyTier,
        required_tee: TeeType,
    ) -> SandboxBackend {
        if tier.requires_attestation() && required_tee != TeeType::None {
            SandboxBackend::Tee(required_tee)
        } else {
            SandboxBackend::Docker
        }
    }
}

/// A request to provision an operator sandbox to run one researcher method against
/// one proposer sealed target.
///
/// The [`backend`](Self::backend) field is the TEE/no-TEE toggle. `sealed_target` and
/// `dev_split` are **opaque sealed handles** — the bytes never live here (PRIVACY §1).
#[derive(Clone, Debug)]
pub struct SandboxProvisionReq {
    /// The TEE/no-TEE toggle (and Local for tests). The one field that flips a
    /// competition between a plain Docker sandbox and a sealed enclave.
    pub backend: SandboxBackend,
    /// The proposer's baseline/target to improve, as a sealed handle. Decrypted only
    /// inside the sandbox (and only inside the enclave when `backend.is_tee()`).
    pub sealed_target: ArtifactRef,
    /// The proposer's dev split handle, if the privacy tier permits the method to see
    /// dev-split signal. `None` for black-box-style flows.
    pub dev_split: Option<EgressGatedRef>,
    /// The brokered-egress policy the sandbox runs under (PRIVACY §6). For a TEE +
    /// white-box-no-egress competition this is [`EgressPolicy::no_egress`]; an open
    /// policy on a TEE backend is rejected fail-closed by [`SandboxProvisionReq::validate`].
    pub egress: Option<EgressPolicy>,
    /// Spend ceiling for this run, in wei-equivalent budget units (mapped to the
    /// sandbox's resource/lifetime limits by the real backend).
    pub budget_wei: u128,
    /// CPU cores to allocate to the sandbox.
    pub cpu_cores: u64,
    /// Memory (MB) to allocate to the sandbox.
    pub memory_mb: u64,
    /// Disk (GB) to allocate to the sandbox.
    pub disk_gb: u64,
}

/// A dev-split handle paired with the egress policy it is reachable under. Kept as a
/// distinct type so a `Some(dev_split)` cannot be constructed without acknowledging
/// the egress decision that governs it.
pub type EgressGatedRef = ArtifactRef;

impl SandboxProvisionReq {
    /// A minimal provision request with default resource limits, no dev split, and the
    /// given egress policy. Resource limits default to a small sandbox; raise them via
    /// the public fields when a method needs more.
    #[must_use]
    pub fn new(backend: SandboxBackend, sealed_target: ArtifactRef, budget_wei: u128) -> Self {
        Self {
            backend,
            sealed_target,
            dev_split: None,
            egress: None,
            budget_wei,
            cpu_cores: 2,
            memory_mb: 2_048,
            disk_gb: 10,
        }
    }

    /// Set the dev-split handle the method may read.
    #[must_use]
    pub fn with_dev_split(mut self, dev_split: Option<ArtifactRef>) -> Self {
        self.dev_split = dev_split;
        self
    }

    /// Set the brokered-egress policy.
    #[must_use]
    pub fn with_egress(mut self, egress: Option<EgressPolicy>) -> Self {
        self.egress = egress;
        self
    }

    /// Fail-closed validation of the provision request (PRIVACY §5.3, §6, M4).
    ///
    /// The TEE toggle must thread consistently with the egress decision: a TEE backend
    /// that is being used for the white-box-no-egress case must run **no-egress**. We
    /// enforce the host-independent half of that here — a TEE provision carrying an
    /// **open** egress policy (`default_deny == false`) is rejected, because an open
    /// socket out of a confidential enclave defeats the very confidentiality the TEE
    /// backend is selected for. A `None` egress on a TEE backend is also rejected: the
    /// no-egress decision must be explicit, never defaulted-open.
    ///
    /// Docker / Local backends do not gate egress here (a public competition has no
    /// proprietary data to protect; PRIVACY §3) — the egress policy, if any, is still
    /// carried into the method's [`EngineContext`] for the broker to consult.
    ///
    /// # Errors
    /// [`SandboxError::EgressNotFailClosed`] if a TEE backend has a missing or open
    /// egress policy.
    pub fn validate(&self) -> Result<(), SandboxError> {
        if self.backend.is_tee() {
            match &self.egress {
                None => return Err(SandboxError::EgressNotFailClosed),
                Some(p) if !p.default_deny => return Err(SandboxError::EgressNotFailClosed),
                Some(_) => {}
            }
        }
        Ok(())
    }
}

/// A handle to a provisioned operator sandbox. Returned by [`SandboxHost::provision`]
/// and consumed by [`SandboxHost::run_method`] / [`SandboxHost::teardown`].
#[derive(Clone, Debug)]
pub struct SandboxHandle {
    /// Operator-local sandbox id.
    pub id: String,
    /// The sidecar URL the operator reaches the running sandbox at (the real backend's
    /// `SandboxRecord::sidecar_url`; an in-process marker for [`crate::LocalSandboxHost`]).
    pub sidecar_url: String,
    /// The backend this sandbox was provisioned with — carries the TEE toggle decision
    /// through the handle so the engine and tests can read `handle.backend.is_tee()`.
    pub backend: SandboxBackend,
    /// The (structural) attestation report captured at provision time, when
    /// `backend.is_tee()`. `None` for Docker / Local. **Structural-only** today: it
    /// proves an enclave of the right *shape*, NOT genuine hardware (PRIVACY §12).
    pub tee_attestation: Option<AttestationReport>,
}

impl SandboxHandle {
    /// Whether this sandbox is a TEE enclave (mirrors `backend.is_tee()`).
    #[must_use]
    pub fn is_tee(&self) -> bool {
        self.backend.is_tee()
    }
}

/// Errors from the operator-compute seam. All fail-closed: a misconfiguration that
/// would weaken the guarantee is rejected, never silently downgraded.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// A TEE backend was provisioned without a fail-closed (`default_deny`) egress
    /// policy. Confidential compute with an open socket out is exfiltration-by-egress;
    /// rejected at provision time (PRIVACY §5.3, §6, M4).
    #[error(
        "TEE sandbox requires a fail-closed (no-egress / default-deny) egress policy; \
         an open or missing policy is rejected (PRIVACY §5.3, M4)"
    )]
    EgressNotFailClosed,
    /// The sandbox could not be provisioned (resource exhaustion, backend
    /// unavailable, image pull failure, …).
    #[error("sandbox provisioning failed: {0}")]
    Provision(String),
    /// The method failed to run inside the sandbox (non-zero exit, agent error,
    /// timeout, …).
    #[error("method execution inside the sandbox failed: {0}")]
    Execution(String),
    /// The produced candidate could not be read back / sealed out of the sandbox.
    #[error("could not read the produced candidate from the sandbox: {0}")]
    Output(String),
    /// Tearing the sandbox down failed.
    #[error("sandbox teardown failed: {0}")]
    Teardown(String),
    /// The backend is not available in this build / environment (e.g. the real
    /// `sandbox-runtime` backend without the feature, or no TEE backend configured).
    #[error("sandbox backend unavailable: {0}")]
    Unavailable(String),
}

/// An operator-compute backend: provisions a sandbox, runs a researcher's submitted
/// method inside it against the proposer's sealed target, and tears it down.
///
/// `provision` + `run_method` + `teardown` is the operator's job. The researcher
/// supplies only the `method` ([`ArtifactRef`]); the operator owns the lifecycle.
///
/// Returns `impl Future + Send` (not `async fn`) so implementations are usable from
/// multi-threaded executors and behind `dyn` adapters, matching the rest of the
/// runtime's trait style.
pub trait SandboxHost {
    /// Provision a sandbox for one method run. Maps [`SandboxBackend::Tee`] to a
    /// `tee_required` enclave with sealed inputs and a captured (structural)
    /// attestation; [`SandboxBackend::Docker`] to a plain container; and
    /// [`SandboxBackend::Local`] to in-process. Must `validate` the request
    /// (fail-closed on an open-egress TEE provision) before allocating anything.
    fn provision(
        &self,
        req: &SandboxProvisionReq,
    ) -> impl Future<Output = Result<SandboxHandle, SandboxError>> + Send;

    /// Run the researcher's submitted `method` inside the provisioned sandbox against
    /// the proposer's sealed target + dev split (carried on `ctx`), returning a
    /// reference to the produced candidate artifact. The method never sees the host;
    /// the candidate is the only thing that leaves (output-gated, PRIVACY §5.3).
    fn run_method(
        &self,
        handle: &SandboxHandle,
        method: &ArtifactRef,
        ctx: &EngineContext,
    ) -> impl Future<Output = Result<ArtifactRef, SandboxError>> + Send;

    /// Tear the sandbox down, releasing the operator's compute. Consumes the handle.
    fn teardown(
        &self,
        handle: SandboxHandle,
    ) -> impl Future<Output = Result<(), SandboxError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use autoresearch_runtime::privacy::PrivacyTier;

    #[test]
    fn the_toggle_is_one_field() {
        // Docker and TEE differ in exactly one enum field; the predicates follow it.
        assert!(!SandboxBackend::Docker.is_tee());
        assert!(SandboxBackend::Tee(TeeType::PhalaTdx).is_tee());
        assert!(!SandboxBackend::Local.is_tee());
        // Only the TEE backend requires sealed inputs.
        assert!(SandboxBackend::Tee(TeeType::AwsNitro).requires_sealed_inputs());
        assert!(!SandboxBackend::Docker.requires_sealed_inputs());
        assert!(!SandboxBackend::Local.requires_sealed_inputs());
        // Runtime-backend mapping is the real `runtime_backend` selector.
        assert_eq!(SandboxBackend::Docker.runtime_backend(), "docker");
        assert_eq!(
            SandboxBackend::Tee(TeeType::PhalaTdx).runtime_backend(),
            "tee"
        );
        assert_eq!(SandboxBackend::Local.runtime_backend(), "local");
    }

    #[test]
    fn for_competition_selects_tee_only_for_attestation_tiers_with_a_real_tee() {
        // White-box / attested with a real TEE => sealed enclave.
        for tier in [PrivacyTier::WhiteBoxNoEgress, PrivacyTier::AttestedHarness] {
            assert_eq!(
                SandboxBackend::for_competition(tier, TeeType::PhalaTdx),
                SandboxBackend::Tee(TeeType::PhalaTdx),
                "{tier:?} with a real TEE must select the sealed enclave"
            );
            // The same attestation-reliant tier with no TEE demanded falls back to
            // Docker (the protocol's private runner separately rejects that misconfig).
            assert_eq!(
                SandboxBackend::for_competition(tier, TeeType::None),
                SandboxBackend::Docker
            );
        }
        // Black-box / redacted never select TEE (no data on the researcher side).
        for tier in [PrivacyTier::BlackBox, PrivacyTier::RedactedFeedback] {
            assert_eq!(
                SandboxBackend::for_competition(tier, TeeType::PhalaTdx),
                SandboxBackend::Docker,
                "{tier:?} keeps no proprietary data on the researcher side"
            );
        }
    }

    #[test]
    fn validate_is_fail_closed_for_tee_egress() {
        let target = ArtifactRef("sealed".into());
        // TEE + no-egress: ok.
        assert!(
            SandboxProvisionReq::new(SandboxBackend::Tee(TeeType::PhalaTdx), target.clone(), 1)
                .with_egress(Some(EgressPolicy::no_egress()))
                .validate()
                .is_ok()
        );
        // TEE + open egress: rejected.
        assert!(matches!(
            SandboxProvisionReq::new(SandboxBackend::Tee(TeeType::PhalaTdx), target.clone(), 1)
                .with_egress(Some(EgressPolicy {
                    allowlist: vec![],
                    default_deny: false,
                }))
                .validate(),
            Err(SandboxError::EgressNotFailClosed)
        ));
        // TEE + missing egress: rejected (must be explicit).
        assert!(matches!(
            SandboxProvisionReq::new(SandboxBackend::Tee(TeeType::PhalaTdx), target.clone(), 1)
                .validate(),
            Err(SandboxError::EgressNotFailClosed)
        ));
        // Docker + no policy: ok (public, nothing to protect).
        assert!(
            SandboxProvisionReq::new(SandboxBackend::Docker, target.clone(), 1)
                .validate()
                .is_ok()
        );
        // TEE + allowlisted default-deny (attested harness) is fail-closed => ok.
        assert!(
            SandboxProvisionReq::new(SandboxBackend::Tee(TeeType::AzureSnp), target, 1)
                .with_egress(Some(EgressPolicy::allowlisted(vec![
                    "model.endpoint".into()
                ])))
                .validate()
                .is_ok()
        );
    }
}
