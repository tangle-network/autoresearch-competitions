//! TEE attestation — honestly **structural-only** today (PRIVACY §12).
//!
//! This module mirrors the *real* state of the agent-sandbox attestation we build
//! on: reports are collected and **shape-validated**, and a canonical hash is
//! committed on-chain — but the cryptographic verification that would make an
//! attestation *trustworthy* is **not implemented**. We are precise about this on
//! purpose; the credibility of the whole privacy milestone is in not overclaiming.
//!
//! > **"Attestation submitted" does NOT mean "attestation valid."** (PRIVACY §12)
//!
//! Specifically **not implemented** (PRIVACY §12, ARCHITECTURE §7):
//!
//! - **Hardware quote signature verification** — DCAP/KDS (Intel TDX), NSM (AWS
//!   Nitro), or the vendor root-of-trust check that proves the quote came from
//!   genuine TEE silicon.
//! - **Measurement pinning** — asserting the enclave measurement equals an expected,
//!   pinned value so a *modified* image is rejected.
//! - **Nonce binding** — binding a fresh challenge nonce so a captured attestation
//!   cannot be replayed.
//!
//! Consequently [`AttestationVerdict::Verified`] is **not yet reachable** by any code
//! path here. The strongest verdict [`verify_structural`] can return is
//! [`AttestationVerdict::StructurallyValid`]: an enclave of the right *shape* ran —
//! not that it was genuine, unmodified hardware running the *expected* code. A
//! malicious host who forged a structurally-correct report would currently pass
//! structural validation. We do not pretend otherwise.

use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

use crate::traits::{Scorer, ScorerError};
use crate::types::{ArtifactRef, Evidence, EvidenceKind, Lift, Measurement, Split};

/// The TEE backend an attestation claims to come from. `None` is the local /
/// non-TEE referee (the M1 stand-in path); the others are the real vendors a
/// production deployment would target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeeType {
    /// No TEE — a local, in-process referee. Honest about being non-confidential.
    None,
    /// Phala / Intel TDX (verified via DCAP/KDS — unimplemented, PRIVACY §12).
    PhalaTdx,
    /// AWS Nitro Enclaves (verified via NSM — unimplemented, PRIVACY §12).
    AwsNitro,
    /// GCP Confidential Computing (vendor quote verification — unimplemented).
    GcpConfidential,
    /// Azure SEV-SNP (vendor quote verification — unimplemented).
    AzureSnp,
}

/// A raw attestation report as collected from an enclave (or stand-in). The bytes
/// are opaque here; only the *shape* is checked today (PRIVACY §12).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestationReport {
    pub tee_type: TeeType,
    /// The vendor quote / evidence blob. Its *signature* is NOT verified yet.
    pub evidence: Vec<u8>,
    /// The enclave measurement (e.g. MRENCLAVE / PCR set). NOT pinned/matched yet.
    pub measurement: Vec<u8>,
    /// A challenge nonce for replay-binding. NOT bound into the quote yet.
    pub nonce: Option<Vec<u8>>,
}

impl AttestationReport {
    /// Keccak-256 of the report's canonical encoding, hex-encoded (no `0x` prefix).
    ///
    /// This is the value committed on-chain (`commitAttestation`,
    /// `Evidence::attestation_hash`). It is a deterministic **commitment** to the
    /// report bytes — it lets a disputer prove *which* report was scored against, but
    /// it does **not** verify the report (that is the §12 seam). The encoding is
    /// length-prefixed per field so two distinct reports can never collide on the hash.
    #[must_use]
    pub fn hash(&self) -> String {
        let mut hasher = Keccak256::new();
        // Domain tag so this hash can't be confused with another keccak commitment.
        hasher.update(b"autoresearch.attestation.v1");
        hasher.update((self.tee_type as u8).to_le_bytes());
        absorb_len_prefixed(&mut hasher, &self.evidence);
        absorb_len_prefixed(&mut hasher, &self.measurement);
        match &self.nonce {
            Some(n) => {
                hasher.update([1u8]);
                absorb_len_prefixed(&mut hasher, n);
            }
            None => hasher.update([0u8]),
        }
        hex::encode(hasher.finalize())
    }
}

/// Absorb a length-prefixed byte slice so variable-length fields cannot collide
/// across the field boundary (the same anti-ambiguity reason the contract uses
/// `abi.encode`, not `abi.encodePacked`).
fn absorb_len_prefixed(hasher: &mut Keccak256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// The outcome of attempting to verify an attestation. Today only the first two are
/// reachable; the third is intentionally unreachable (PRIVACY §12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttestationVerdict {
    /// No verification attempted (a report was supplied but not checked).
    Unverified,
    /// The report passed the **structural** shape check: non-empty evidence +
    /// measurement and the expected TEE type. This is the **maximum** verdict today.
    /// It does NOT prove genuine hardware or the expected code (PRIVACY §12).
    StructurallyValid,
    /// The hardware quote signature was cryptographically verified AND the
    /// measurement matched a pinned value AND the nonce was bound.
    ///
    /// **NOT YET REACHABLE.** No code path returns this. Reaching it requires the
    /// unimplemented §12 work (DCAP/KDS/NSM quote verification + on-chain measurement
    /// pinning + nonce binding). It exists in the type so the *target* state is named
    /// and so callers that (correctly) require `Verified` fail closed today.
    Verified,
    /// The report failed validation (empty fields, or a TEE-type mismatch).
    Failed,
}

/// The result of [`verify_structural`]: the verdict plus the three orthogonal
/// sub-checks, so a caller can see *exactly* what was and was not proven. Today
/// `signature_verified` and `measurement_matched` are always `false` (PRIVACY §12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedAttestation {
    pub verdict: AttestationVerdict,
    /// Whether the hardware quote signature was cryptographically verified. Always
    /// `false` today — the §12 seam (DCAP/KDS/NSM is unimplemented).
    pub signature_verified: bool,
    /// Whether the enclave measurement matched a pinned expected value. Always
    /// `false` today — measurement pinning is unimplemented (§12).
    pub measurement_matched: bool,
    /// Whether the report passed the structural shape check (the only check that is
    /// actually performed today).
    pub structural_ok: bool,
}

impl VerifiedAttestation {
    /// Whether this attestation is at least structurally valid (the bar a tier that
    /// requires attestation must clear today; PRIVACY §12 "gate accordingly").
    #[must_use]
    pub fn is_structurally_valid(&self) -> bool {
        matches!(
            self.verdict,
            AttestationVerdict::StructurallyValid | AttestationVerdict::Verified
        )
    }
}

/// Structurally validate an attestation report against a required TEE type.
///
/// This is **all** the checking that exists today (PRIVACY §12). It verifies the
/// report's *shape*:
///
/// - the `tee_type` matches `required_tee`, and
/// - `evidence` and `measurement` are both non-empty.
///
/// On success it returns [`AttestationVerdict::StructurallyValid`] with
/// `signature_verified = false` and `measurement_matched = false` — it **never**
/// returns [`AttestationVerdict::Verified`], because the cryptographic verification
/// that verdict would assert is not implemented.
///
/// Fail-closed: an empty evidence/measurement field, or a TEE-type mismatch, yields
/// [`AttestationVerdict::Failed`].
///
/// # Closing the seam (PRIVACY §12)
///
/// Reaching [`AttestationVerdict::Verified`] requires: (1) quote-signature
/// verification against each vendor root of trust (DCAP for Phala/TDX, NSM for AWS
/// Nitro, the GCP/Azure equivalents); (2) on-chain measurement pinning per Scorer /
/// harness image with mismatch rejection; (3) fresh-nonce binding into each quote;
/// (4) binding the verified measurement to the committed `attestation_hash`; and (5)
/// client-side verification so a proposer/validator checks the quote independently.
#[must_use]
pub fn verify_structural(report: &AttestationReport, required_tee: TeeType) -> VerifiedAttestation {
    let type_matches = report.tee_type == required_tee;
    let non_empty = !report.evidence.is_empty() && !report.measurement.is_empty();
    let structural_ok = type_matches && non_empty;

    VerifiedAttestation {
        verdict: if structural_ok {
            AttestationVerdict::StructurallyValid
        } else {
            AttestationVerdict::Failed
        },
        // The two cryptographic checks are unimplemented (§12); always false.
        signature_verified: false,
        measurement_matched: false,
        structural_ok,
    }
}

/// A local, in-process referee wrapper that scores on [`Split::HeldOut`] and emits
/// [`Evidence`] carrying an [`AttestationReport`]'s [`AttestationReport::hash`] in
/// `attestation_hash`.
///
/// **This is a local stand-in, not a real enclave.** It documents — and the tests
/// assert — that the attestation it produces is **structural-only**: the report's
/// `tee_type` is [`TeeType::None`] and no genuine TEE executes the scorer. It exists
/// to wire the attestation-hash commitment end-to-end (so the on-chain
/// `commitAttestation` path and `Evidence::attestation_hash` are exercised) WITHOUT
/// claiming confidentiality against a malicious host. The production swap replaces
/// this with a referee whose scorer runs inside a vendor TEE and whose report is the
/// real hardware quote — at which point [`verify_structural`] is replaced by the §12
/// verification and `Verified` becomes reachable.
#[derive(Clone, Debug)]
pub struct LocalReferee<Sc> {
    scorer: Sc,
}

impl<Sc> LocalReferee<Sc>
where
    Sc: Scorer,
{
    /// Wrap a scorer as a local (non-TEE) referee.
    #[must_use]
    pub fn new(scorer: Sc) -> Self {
        Self { scorer }
    }

    /// The attestation report this referee emits. It is honest about being a
    /// non-TEE stand-in: `tee_type` is [`TeeType::None`], and the evidence/measurement
    /// blobs are local-provenance markers, not a hardware quote.
    #[must_use]
    pub fn attestation_report(&self) -> AttestationReport {
        AttestationReport {
            tee_type: TeeType::None,
            // A stable, non-empty local-provenance marker so the report is
            // structurally well-formed. NOT a hardware quote.
            evidence: format!("local-referee:{}", self.scorer.id()).into_bytes(),
            measurement: b"local-stand-in:no-enclave".to_vec(),
            nonce: None,
        }
    }

    /// Score a candidate against the baseline on the held-out split and certify it as
    /// [`Evidence`] with the attestation hash committed in `attestation_hash`.
    ///
    /// The lift is estimated by the caller-supplied `estimate` closure (the protocol
    /// layer injects [`autoresearch_protocol::estimate_lift`]); this keeps the runtime
    /// crate free of the estimator while still producing a complete evidence row.
    ///
    /// # Errors
    /// Propagates any [`ScorerError`] from scoring the candidate or the baseline.
    pub async fn certify<F>(
        &self,
        candidate: &Sc::Artifact,
        baseline: &Sc::Artifact,
        suite_ref: ArtifactRef,
        estimate: F,
    ) -> Result<(Evidence, AttestationReport), ScorerError>
    where
        F: Fn(&Measurement, &Measurement) -> Lift,
    {
        let baseline_m = self.scorer.score(baseline, Split::HeldOut).await?;
        let candidate_m = self.scorer.score(candidate, Split::HeldOut).await?;
        let lift = estimate(&candidate_m, &baseline_m);
        let report = self.attestation_report();
        let evidence = Evidence {
            kind: EvidenceKind::ReplayFull,
            lift,
            measurement: candidate_m,
            confounded: false,
            suite_ref,
            attestation_hash: report.hash(),
        };
        Ok((evidence, report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_report() -> AttestationReport {
        AttestationReport {
            tee_type: TeeType::PhalaTdx,
            evidence: vec![1, 2, 3, 4],
            measurement: vec![9, 8, 7],
            nonce: Some(vec![0xAB]),
        }
    }

    #[test]
    fn hash_is_deterministic() {
        let r = good_report();
        assert_eq!(r.hash(), r.hash());
        assert_eq!(r.hash().len(), 64, "keccak256 is 32 bytes = 64 hex chars");
    }

    #[test]
    fn hash_changes_with_any_field() {
        let base = good_report();
        let h = base.hash();

        let mut diff_tee = base.clone();
        diff_tee.tee_type = TeeType::AwsNitro;
        assert_ne!(diff_tee.hash(), h);

        let mut diff_ev = base.clone();
        diff_ev.evidence = vec![1, 2, 3, 5];
        assert_ne!(diff_ev.hash(), h);

        let mut diff_meas = base.clone();
        diff_meas.measurement = vec![9, 8, 6];
        assert_ne!(diff_meas.hash(), h);

        let mut diff_nonce = base.clone();
        diff_nonce.nonce = None;
        assert_ne!(diff_nonce.hash(), h);
    }

    #[test]
    fn hash_does_not_collide_across_field_boundary() {
        // Length-prefixing must prevent a byte from sliding between evidence and
        // measurement and producing the same hash.
        let a = AttestationReport {
            tee_type: TeeType::None,
            evidence: vec![1, 2],
            measurement: vec![3],
            nonce: None,
        };
        let b = AttestationReport {
            tee_type: TeeType::None,
            evidence: vec![1],
            measurement: vec![2, 3],
            nonce: None,
        };
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn structural_validation_never_returns_verified() {
        // The whole point of §12: a good report is StructurallyValid, NEVER Verified.
        let v = verify_structural(&good_report(), TeeType::PhalaTdx);
        assert_eq!(v.verdict, AttestationVerdict::StructurallyValid);
        assert!(v.structural_ok);
        // The cryptographic checks are unimplemented and must report false.
        assert!(!v.signature_verified);
        assert!(!v.measurement_matched);
        assert_ne!(
            v.verdict,
            AttestationVerdict::Verified,
            "Verified must be unreachable until the §12 seam is closed"
        );
        assert!(v.is_structurally_valid());
    }

    #[test]
    fn structural_validation_fails_closed_on_empty_fields() {
        let empty_ev = AttestationReport {
            tee_type: TeeType::PhalaTdx,
            evidence: vec![],
            measurement: vec![1],
            nonce: None,
        };
        let v = verify_structural(&empty_ev, TeeType::PhalaTdx);
        assert_eq!(v.verdict, AttestationVerdict::Failed);
        assert!(!v.structural_ok);
        assert!(!v.is_structurally_valid());

        let empty_meas = AttestationReport {
            tee_type: TeeType::PhalaTdx,
            evidence: vec![1],
            measurement: vec![],
            nonce: None,
        };
        assert_eq!(
            verify_structural(&empty_meas, TeeType::PhalaTdx).verdict,
            AttestationVerdict::Failed
        );
    }

    #[test]
    fn structural_validation_fails_closed_on_tee_mismatch() {
        // A report claiming AwsNitro cannot satisfy a PhalaTdx requirement.
        let v = verify_structural(&good_report(), TeeType::AwsNitro);
        assert_eq!(v.verdict, AttestationVerdict::Failed);
        assert!(!v.is_structurally_valid());
    }

    #[test]
    fn verdict_never_verified_across_all_tee_types() {
        // Exhaustively confirm no required_tee makes verify_structural return Verified.
        for tee in [
            TeeType::None,
            TeeType::PhalaTdx,
            TeeType::AwsNitro,
            TeeType::GcpConfidential,
            TeeType::AzureSnp,
        ] {
            let report = AttestationReport {
                tee_type: tee,
                evidence: vec![1],
                measurement: vec![1],
                nonce: None,
            };
            let v = verify_structural(&report, tee);
            assert_ne!(v.verdict, AttestationVerdict::Verified);
            assert_eq!(v.verdict, AttestationVerdict::StructurallyValid);
        }
    }
}
