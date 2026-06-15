//! `SandboxRuntimeHost` — the real operator compute, backed by `sandbox-runtime`.
//!
//! Implements [`autoresearch_sandbox::SandboxHost`] by calling the agent-sandbox-blueprint
//! `sandbox-runtime` crate: it provisions a real sandbox (plain Docker for no-TEE, a
//! sealed TEE enclave for TEE), runs the researcher's submitted method inside it via
//! the sidecar exec API against the proposer's sealed target, reads the produced
//! candidate back out, and destroys the sandbox.
//!
//! See the crate docs for the workspace-exclusion rationale and the honest compile
//! status. The TEE/no-TEE toggle threads through `runtime_config`:
//! [`SandboxBackend::Tee`] maps to `runtime_backend = "tee"` + a required `TeeConfig`;
//! [`SandboxBackend::Docker`] to `"docker"` with no `TeeConfig`.

use std::future::Future;

use autoresearch_runtime::attestation::{
    AttestationReport as DomainAttestation, TeeType as DomainTeeType,
};
use autoresearch_runtime::traits::EngineContext;
use autoresearch_runtime::types::ArtifactRef;

use autoresearch_sandbox::{
    SandboxBackend, SandboxError, SandboxHandle, SandboxHost, SandboxProvisionReq,
};

use sandbox_runtime::runtime::{
    CreateSandboxParams, SandboxRecord, create_sidecar, delete_sidecar,
};
use sandbox_runtime::tee::{
    AttestationReport as RuntimeAttestation, TeeConfig, TeeType as RuntimeTeeType,
};

use ai_agent_sandbox_blueprint_lib::{SandboxExecRequest, run_exec_request};

/// The real operator-compute backend, backed by `sandbox-runtime`.
///
/// `image` is the sidecar container image the method runs in. `owner` is the
/// operator / on-chain address recorded on the sandbox for ownership checks.
pub struct SandboxRuntimeHost {
    /// Sidecar container image the researcher method executes inside.
    pub image: String,
    /// Operator / on-chain owner address recorded on the sandbox.
    pub owner: String,
}

impl SandboxRuntimeHost {
    /// Build the real backend with the given sidecar image and operator owner address.
    #[must_use]
    pub fn new(image: impl Into<String>, owner: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            owner: owner.into(),
        }
    }

    /// Map the domain TEE/no-TEE toggle onto the `sandbox-runtime` `runtime_backend`
    /// metadata value and `TeeConfig`. The one place the toggle becomes the real
    /// backend's `tee_required` + backend selector.
    fn runtime_config(backend: SandboxBackend) -> (String, Option<TeeConfig>) {
        match backend {
            SandboxBackend::Docker | SandboxBackend::Local => ("docker".to_string(), None),
            SandboxBackend::Tee(tee) => (
                "tee".to_string(),
                Some(TeeConfig {
                    required: true,
                    tee_type: domain_tee_to_runtime(tee),
                    attestation_nonce: None,
                }),
            ),
        }
    }

    /// Build the `sandbox-runtime` create request from a provision request. The egress
    /// policy is carried as metadata for the sidecar's broker; the sealed-target /
    /// dev-split handles travel as sealed env so they are decrypted only inside the
    /// (enclave) sandbox.
    fn create_params(&self, req: &SandboxProvisionReq) -> CreateSandboxParams {
        let (runtime_backend, tee_config) = Self::runtime_config(req.backend);

        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "runtime_backend".to_string(),
            serde_json::Value::String(runtime_backend),
        );
        if let Some(egress) = &req.egress {
            metadata.insert(
                "egress_default_deny".to_string(),
                serde_json::Value::Bool(egress.default_deny),
            );
            metadata.insert(
                "egress_allowlist".to_string(),
                serde_json::Value::Array(
                    egress
                        .allowlist
                        .iter()
                        .map(|h| serde_json::Value::String(h.clone()))
                        .collect(),
                ),
            );
        }

        let mut env = serde_json::Map::new();
        env.insert(
            "AUTORESEARCH_SEALED_TARGET".to_string(),
            serde_json::Value::String(req.sealed_target.0.clone()),
        );
        if let Some(dev) = &req.dev_split {
            env.insert(
                "AUTORESEARCH_DEV_SPLIT".to_string(),
                serde_json::Value::String(dev.0.clone()),
            );
        }

        CreateSandboxParams {
            name: "autoresearch-method".to_string(),
            image: self.image.clone(),
            agent_identifier: "autoresearch-method".to_string(),
            env_json: serde_json::Value::Object(env).to_string(),
            metadata_json: serde_json::Value::Object(metadata).to_string(),
            capabilities_json: r#"["all_harness"]"#.to_string(),
            owner: self.owner.clone(),
            tee_config,
            cpu_cores: req.cpu_cores,
            memory_mb: req.memory_mb,
            disk_gb: req.disk_gb,
            max_lifetime_seconds: 3_600,
            idle_timeout_seconds: 0,
            ..Default::default()
        }
    }
}

/// Map the domain [`DomainTeeType`] onto the `sandbox-runtime` [`RuntimeTeeType`]. The
/// runtime enum is coarser (TDX covers Phala/GCP-TDX; SEV covers Azure SNP).
fn domain_tee_to_runtime(tee: DomainTeeType) -> RuntimeTeeType {
    match tee {
        DomainTeeType::None => RuntimeTeeType::None,
        DomainTeeType::PhalaTdx | DomainTeeType::GcpConfidential => RuntimeTeeType::Tdx,
        DomainTeeType::AwsNitro => RuntimeTeeType::Nitro,
        DomainTeeType::AzureSnp => RuntimeTeeType::Sev,
    }
}

/// Map the `sandbox-runtime` [`RuntimeTeeType`] back to the domain [`DomainTeeType`]
/// when capturing the deploy-time attestation into the handle.
fn runtime_tee_to_domain(tee: &RuntimeTeeType) -> DomainTeeType {
    match tee {
        RuntimeTeeType::None => DomainTeeType::None,
        RuntimeTeeType::Tdx => DomainTeeType::PhalaTdx,
        RuntimeTeeType::Nitro => DomainTeeType::AwsNitro,
        RuntimeTeeType::Sev => DomainTeeType::AzureSnp,
    }
}

/// Convert a `sandbox-runtime` attestation report into the domain report captured on
/// the handle. **Structural-only**: the evidence/measurement bytes are carried but the
/// signature is not verified here (PRIVACY §12).
fn capture_attestation(report: &RuntimeAttestation) -> DomainAttestation {
    DomainAttestation {
        tee_type: runtime_tee_to_domain(&report.tee_type),
        evidence: report.evidence.clone(),
        measurement: report.measurement.clone(),
        nonce: None,
    }
}

impl SandboxHost for SandboxRuntimeHost {
    fn provision(
        &self,
        req: &SandboxProvisionReq,
    ) -> impl Future<Output = Result<SandboxHandle, SandboxError>> + Send {
        // Validate fail-closed (open-egress TEE provision rejected) before allocating.
        let params = req.validate().map(|()| self.create_params(req));
        let backend = req.backend;
        async move {
            let params = params?;
            let (record, attestation): (SandboxRecord, Option<RuntimeAttestation>) =
                create_sidecar(&params, None)
                    .await
                    .map_err(|e| SandboxError::Provision(e.to_string()))?;

            Ok(SandboxHandle {
                id: record.id.clone(),
                sidecar_url: record.sidecar_url.clone(),
                backend,
                tee_attestation: attestation.as_ref().map(capture_attestation),
            })
        }
    }

    fn run_method(
        &self,
        handle: &SandboxHandle,
        method: &ArtifactRef,
        _ctx: &EngineContext,
    ) -> impl Future<Output = Result<ArtifactRef, SandboxError>> + Send {
        let sidecar_url = handle.sidecar_url.clone();
        let sandbox_id = handle.id.clone();
        let method = method.clone();
        async move {
            // The sidecar token is operator-owned (never on-chain calldata); look it up.
            let record = sandbox_runtime::runtime::get_sandbox_by_url(&sidecar_url)
                .map_err(|e| SandboxError::Execution(e.to_string()))?;

            // Run the researcher's submitted method inside the sandbox. The sealed
            // target + dev split are present as sealed env; the candidate is written to
            // a known path inside the sandbox.
            let exec = SandboxExecRequest {
                sidecar_url: sidecar_url.clone(),
                command: format!(
                    "autoresearch-run-method --method {} --out /candidate.json",
                    method.0
                ),
                cwd: "/workspace".to_string(),
                env_json: String::new(),
                timeout_ms: 600_000,
            };
            let resp = run_exec_request(&exec, &record.token)
                .await
                .map_err(SandboxError::Execution)?;
            if resp.exit_code != 0 {
                return Err(SandboxError::Execution(format!(
                    "method exited {}: {}",
                    resp.exit_code, resp.stderr
                )));
            }

            // The produced candidate is sealed out under a ref bound to the sandbox id.
            // A concrete vertical reads /candidate.json back and deserializes it (the
            // candidate-readback seam); here we return the ref the operator stored it
            // under.
            Ok(ArtifactRef(format!(
                "sandbox-candidate:{sandbox_id}:/candidate.json"
            )))
        }
    }

    fn teardown(
        &self,
        handle: SandboxHandle,
    ) -> impl Future<Output = Result<(), SandboxError>> + Send {
        async move {
            let record = sandbox_runtime::runtime::get_sandbox_by_url(&handle.sidecar_url)
                .map_err(|e| SandboxError::Teardown(e.to_string()))?;
            delete_sidecar(&record, None)
                .await
                .map_err(|e| SandboxError::Teardown(e.to_string()))?;
            Ok(())
        }
    }
}
