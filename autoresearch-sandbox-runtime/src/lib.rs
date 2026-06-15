//! # autoresearch-sandbox-runtime — the REAL operator compute
//!
//! Implements [`autoresearch_sandbox::SandboxHost`] against the real
//! agent-sandbox-blueprint `sandbox-runtime` crate. This is the production swap for
//! the default in-process [`autoresearch_sandbox::LocalSandboxHost`]: it provisions a
//! real sandbox (plain Docker for no-TEE, a sealed TEE enclave for TEE), runs the
//! researcher's submitted method inside it via the sidecar exec API against the
//! proposer's sealed target, and destroys it on teardown.
//!
//! ## Why this crate is workspace-excluded (and why the default build stays green)
//!
//! The `sandbox-runtime` git dependency pins `blueprint-sdk = "=0.2.0-alpha.6"` (a
//! strict `=`). This blueprint's workspace resolves `blueprint-sdk` to a different,
//! semver-incompatible version (`>=0.2.0-alpha.5`, currently `alpha.9`). Cargo cannot
//! unify two incompatible `blueprint-sdk` versions in one resolution, so putting the
//! git dep in any DEFAULT workspace member breaks the default `cargo build` at the
//! resolution stage — before a single line compiles. (Verified: the resolver errors
//! `failed to select a version for blueprint-sdk ... =0.2.0-alpha.6 ... conflicts with
//! ... 0.2.0-alpha.9`.)
//!
//! Keeping this crate in the workspace `exclude` list isolates that pin: the default
//! six gates (over the five default members) never resolve it, and this crate resolves
//! independently — its graph contains only `autoresearch-runtime` /
//! `autoresearch-sandbox` (neither uses `blueprint-sdk`) plus `sandbox-runtime`, so
//! `blueprint-sdk` resolves to `alpha.6` with no conflict.
//!
//! ## Honest compile status (do NOT claim this runs)
//!
//! The backend ([`sandbox_runtime_host::SandboxRuntimeHost`]) is written against the
//! real, current `sandbox-runtime` API — `CreateSandboxParams`, `create_sidecar`,
//! `delete_sidecar`, `TeeConfig` / `TeeType`, `SandboxExecRequest` +
//! `run_exec_request`. These are NOT stubs. Whether `cargo build --features
//! sandbox-runtime` fully links depends on the upstream crates compiling in this
//! environment (system deps, branch drift); the calling task reports the ground-truth
//! result. Attestation remains structural-only (PRIVACY §12) — capturing a report's
//! bytes is not verifying its hardware quote.

#![forbid(unsafe_code)]

#[cfg(feature = "sandbox-runtime")]
pub mod sandbox_runtime_host;

#[cfg(feature = "sandbox-runtime")]
pub use sandbox_runtime_host::SandboxRuntimeHost;
