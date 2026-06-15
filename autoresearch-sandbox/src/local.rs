//! [`LocalSandboxHost`] — the DEFAULT, deterministic, in-process operator-compute
//! backend.
//!
//! **This is a stand-in, not a real sandbox.** It runs the researcher's method as
//! plain in-process logic — no Docker, no network, no clock — so the config-opt
//! vertical (and the six default gates) work end-to-end with nothing installed. It
//! honors the [`SandboxBackend`] toggle for the test that proves the switch
//! (recording `is_tee`, capturing a **synthetic structural** attestation when
//! [`SandboxBackend::Tee`]) but does the compute locally. The real execution lives in
//! the feature-gated [`crate::sandbox_runtime_host`].
//!
//! ## How a method runs in-process
//!
//! The [`SandboxHost`] trait is artifact-opaque: it speaks [`ArtifactRef`]. To run a
//! real method in-process and hand back a real typed candidate, [`LocalSandboxHost`]
//! is generic over a [`LocalMethod`] — the deterministic in-process stand-in for
//! "the researcher's submitted method, executed by the operator." The method consumes
//! the run context (which carries the sealed target + dev-split refs and the egress
//! decision) and produces the candidate. An in-process artifact store maps the opaque
//! [`ArtifactRef`] the trait returns back to the materialized candidate so
//! [`SandboxMethodEngine`](crate::SandboxMethodEngine) can resolve it.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;

use autoresearch_runtime::attestation::{AttestationReport, TeeType};
use autoresearch_runtime::traits::EngineContext;
use autoresearch_runtime::types::ArtifactRef;

use crate::host::{SandboxBackend, SandboxError, SandboxHandle, SandboxHost, SandboxProvisionReq};

/// A deterministic in-process method: the stand-in for the researcher-submitted
/// method that the operator runs inside the sandbox.
///
/// `Artifact` is the typed candidate this method produces (e.g.
/// `autoresearch_verticals::ConfigArtifact`). `run` is handed the submitted method's
/// reference and the run context (sealed-target / dev-split handles, budget, egress)
/// and returns the produced candidate. It must be pure and deterministic — same
/// inputs, byte-identical output — so the default suite stays reproducible.
pub trait LocalMethod: Send + Sync {
    /// The typed candidate artifact this method produces.
    type Artifact: Clone + Send + Sync;

    /// Run the method against the (sealed) target + dev split carried on `ctx`,
    /// returning the produced candidate. `method` is the researcher's submitted-method
    /// reference; the in-process stand-in keys its deterministic search off it (and
    /// off `ctx`) exactly as the real method would read its own code + the inputs.
    ///
    /// # Errors
    /// [`SandboxError::Execution`] if the method cannot produce a candidate.
    fn run(
        &self,
        method: &ArtifactRef,
        ctx: &EngineContext,
    ) -> Result<Self::Artifact, SandboxError>;
}

/// The default, deterministic, in-process operator-compute backend. Generic over the
/// in-process [`LocalMethod`] it executes. Holds an artifact store so the opaque
/// [`ArtifactRef`] the [`SandboxHost`] trait returns resolves back to the typed
/// candidate.
///
/// **Stand-in.** No container is created; `is_tee` and the synthetic attestation are
/// recorded for the toggle test, but no genuine enclave runs and the compute is
/// in-process and fully visible to the host.
pub struct LocalSandboxHost<M: LocalMethod> {
    method: M,
    /// Maps produced [`ArtifactRef`]s to their materialized typed candidates, so the
    /// engine can resolve the ref the trait hands back. Interior-mutable behind a
    /// `Mutex` so `run_method` can record under the shared `&self` the trait gives.
    store: Mutex<HashMap<String, M::Artifact>>,
    /// Monotonic counter for deterministic, collision-free produced refs.
    counter: Mutex<u64>,
}

impl<M: LocalMethod> LocalSandboxHost<M> {
    /// Wrap an in-process [`LocalMethod`] as the local operator-compute backend.
    #[must_use]
    pub fn new(method: M) -> Self {
        Self {
            method,
            store: Mutex::new(HashMap::new()),
            counter: Mutex::new(0),
        }
    }

    /// Resolve a produced [`ArtifactRef`] back to its materialized typed candidate.
    /// `None` if the ref was not produced by this host (a programming error in the
    /// engine wiring, surfaced rather than silently mis-scored).
    #[must_use]
    pub fn resolve(&self, candidate: &ArtifactRef) -> Option<M::Artifact> {
        self.store
            .lock()
            .expect("local sandbox store mutex poisoned")
            .get(&candidate.0)
            .cloned()
    }

    /// The synthetic structural attestation a TEE-backed local provision captures.
    ///
    /// **Honest stand-in (PRIVACY §12).** This is NOT a hardware quote: the evidence /
    /// measurement blobs are local-provenance markers, and although the report's
    /// `tee_type` is set to the requested enclave type so the toggle reads as TEE,
    /// `verify_structural` can still never return `Verified`. It exists only to thread
    /// the attestation-capture path through the handle for the toggle test.
    fn synthetic_attestation(tee_type: TeeType) -> AttestationReport {
        AttestationReport {
            tee_type,
            // A stable, non-empty marker so the report is structurally well-formed
            // (passes the shape check) — NOT a genuine enclave quote.
            evidence: format!("local-sandbox-stand-in:{tee_type:?}").into_bytes(),
            measurement: b"local-sandbox:no-enclave".to_vec(),
            nonce: None,
        }
    }
}

impl<M: LocalMethod> SandboxHost for LocalSandboxHost<M> {
    fn provision(
        &self,
        req: &SandboxProvisionReq,
    ) -> impl Future<Output = Result<SandboxHandle, SandboxError>> + Send {
        // Validate fail-closed (open-egress TEE provision is rejected) BEFORE
        // "allocating" anything, exactly as the real backend must.
        let result = req.validate().map(|()| {
            let id = {
                let mut c = self.counter.lock().expect("counter mutex poisoned");
                *c += 1;
                format!("local-sandbox-{:08x}", *c)
            };
            let tee_attestation = match req.backend {
                SandboxBackend::Tee(t) => Some(Self::synthetic_attestation(t)),
                SandboxBackend::Docker | SandboxBackend::Local => None,
            };
            SandboxHandle {
                sidecar_url: format!("inproc://{id}"),
                id,
                backend: req.backend,
                tee_attestation,
            }
        });
        std::future::ready(result)
    }

    fn run_method(
        &self,
        handle: &SandboxHandle,
        method: &ArtifactRef,
        ctx: &EngineContext,
    ) -> impl Future<Output = Result<ArtifactRef, SandboxError>> + Send {
        // Run the method in-process (the stand-in for operator-hosted execution),
        // materialize the candidate, and hand back an opaque ref the engine resolves.
        let result = self.method.run(method, ctx).map(|artifact| {
            let n = {
                let mut c = self.counter.lock().expect("counter mutex poisoned");
                *c += 1;
                *c
            };
            // Bind the ref to the sandbox id so two sandboxes never collide, and to a
            // monotonic counter so repeated runs in one sandbox are distinct.
            let candidate_ref = format!("sandbox-candidate:{}:{n:08x}", handle.id);
            self.store
                .lock()
                .expect("local sandbox store mutex poisoned")
                .insert(candidate_ref.clone(), artifact);
            ArtifactRef(candidate_ref)
        });
        std::future::ready(result)
    }

    fn teardown(
        &self,
        handle: SandboxHandle,
    ) -> impl Future<Output = Result<(), SandboxError>> + Send {
        // Release the in-process state for this sandbox: drop every candidate keyed to
        // it, modelling the real backend reclaiming the container's storage.
        let prefix = format!("sandbox-candidate:{}:", handle.id);
        self.store
            .lock()
            .expect("local sandbox store mutex poisoned")
            .retain(|k, _| !k.starts_with(&prefix));
        std::future::ready(Ok(()))
    }
}
