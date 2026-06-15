//! # autoresearch-sandbox — the operator compute seam
//!
//! This crate wires the *operator-hosted execution* model the rest of the market
//! assumes but did not yet have: the place where a **researcher-submitted method**
//! actually runs, next to the **proposer's sealed target**, on **operator-provided
//! sandboxed compute**.
//!
//! ## The product model (who does what)
//!
//! - **Proposer** (customer, pays): brings a target to improve and a hidden held-out
//!   test plus a bounty. The target and data are **sealed** — they never leave the
//!   operator side.
//! - **Researcher** (competitor, earns): brings a **method** — an auto-research
//!   agent / code that knows how to improve things. The researcher **submits the
//!   method** ([`SandboxProvisionReq`] / [`SandboxHost::run_method`] take a
//!   `method`/`ArtifactRef`); they do **not** bring compute and do **not** run it
//!   themselves.
//! - **Operator** (the platform, earns fees): **provides the sandboxed compute** and
//!   **runs the researcher's method inside the sandbox** against the proposer's
//!   sealed target, then the Referee scores the result. We are the compute *and* the
//!   referee. The method runs next to the data in the operator sandbox, never on the
//!   researcher's machine. Under [`PrivacyTier::WhiteBoxNoEgress`](autoresearch_runtime::privacy::PrivacyTier::WhiteBoxNoEgress)
//!   (M4) the sandbox has **no egress**, so the method can improve the target but
//!   cannot exfiltrate the proposer's data.
//!
//! ## The TEE / no-TEE toggle ([`SandboxBackend`])
//!
//! Whether a competition runs in a plain Docker sandbox (no-TEE) or a sealed TEE
//! enclave is a **single field** — [`SandboxProvisionReq::backend`]. Flip
//! [`SandboxBackend::Docker`] to [`SandboxBackend::Tee`] and nothing else about the
//! engine, method, or scorer changes (the "easy toggle" guarantee, exercised both
//! ways in the tests). [`SandboxBackend::Local`] is the in-process test/dev backend.
//!
//! ## Two backends, one trait
//!
//! - [`LocalSandboxHost`] (DEFAULT, deterministic, in-process — **a stand-in**): runs
//!   the method as in-process logic so the config-opt vertical works end-to-end with
//!   no Docker, no network, no clock. It honors the [`SandboxBackend`] field for the
//!   toggle test (records `is_tee`, captures a **synthetic structural** attestation
//!   when [`SandboxBackend::Tee`]) but does the compute locally. This is what keeps
//!   the six default gates green.
//! - `SandboxRuntimeHost` (the real operator compute) lives in the **separate,
//!   workspace-excluded** crate `autoresearch-sandbox-runtime`: it calls the real
//!   agent-sandbox-blueprint `sandbox-runtime` API (`create_sidecar` / exec /
//!   `delete_sidecar`) against THIS crate's [`SandboxHost`] trait. It is excluded from
//!   the default workspace because the git dep pins `blueprint-sdk = "=0.2.0-alpha.6"`,
//!   which conflicts with this workspace's `blueprint-sdk` — placing the dep in any
//!   default member breaks the default `cargo build` at resolution time. Keeping it
//!   excluded is what lets the six default gates cover this crate fully while the real
//!   backend is still genuinely wired and compile-attemptable. See that crate's
//!   `sandbox_runtime_host.rs` and `README` note for the honest compile status.
//!
//! ## Honesty (do not overclaim)
//!
//! [`LocalSandboxHost`] is a **stand-in**: it does the compute in-process and its TEE
//! attestation is **synthetic and structural-only** — it never executes a genuine
//! enclave and [`verify_structural`](autoresearch_runtime::attestation::verify_structural)
//! can never return `Verified` (PRIVACY §12). The real execution is the feature-gated
//! `SandboxRuntimeHost`. We do not pretend a Docker/TEE sandbox ran when it did not.

#![forbid(unsafe_code)]

pub mod engine;
pub mod host;
pub mod local;

pub use engine::{ResolveCandidate, SandboxCandidate, SandboxMethodEngine};
pub use host::{
    EgressGatedRef, SandboxBackend, SandboxError, SandboxHandle, SandboxHost, SandboxProvisionReq,
};
pub use local::{LocalMethod, LocalSandboxHost};
