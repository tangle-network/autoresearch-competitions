//! Privacy tiers, the pick-at-most-two-of-three exfiltration rule, feedback
//! gating, and brokered egress for `Private` competitions.
//!
//! This module encodes the controls from `docs/PRIVACY.md`. The single most
//! important thing to keep straight (PRIVACY §1, §3): in the default competitive
//! mode there is almost nothing to protect — researchers submit artifacts and get
//! back only **scores**, so there is no plaintext on their side to exfiltrate. A
//! TEE protects an enclave from the **host operator** (boundary B4), *not* the
//! proposer's data from the **researcher** whose own code runs inside in white-box
//! mode (boundary B3). The hard cases are white-box, and they are an
//! **egress / information-flow** problem, not an attestation problem.
//!
//! # The hard rule (PRIVACY §4)
//!
//! > A researcher's code cannot simultaneously have all three of
//! > `{ arbitrary_code, raw_data_access, free_egress }`. Any two are safe; all
//! > three is exfiltration-by-design.
//!
//! This is arithmetic, not policy — see [`ResearcherCapabilities::exfiltration_safe`].
//! Every [`PrivacyTier`] is one application of the rule: each drops exactly one of
//! the three capabilities ([`PrivacyTier::dropped_capability`]), so every tier's
//! capability set passes [`ResearcherCapabilities::validate`].
//!
//! # What this module does NOT claim
//!
//! - It does **not** make a `Private` competition leak-*proof*. The score itself is
//!   a channel (PRIVACY §8); the leak is **bounded**, not zero. [`SubmissionBudget`]
//!   lets a caller rate-limit submissions to *bound* that leak — it does not
//!   eliminate it.
//! - It does **not** provide cryptographic TEE attestation. That lives in
//!   [`crate::attestation`] and is honestly **structural-only** today (PRIVACY §12).

use serde::{Deserialize, Serialize};

use crate::types::{Gate, Lift, Measurement};

/// Errors from the privacy layer. All are fail-closed: a misconfiguration that
/// would weaken the guarantee is rejected, never silently downgraded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PrivacyError {
    /// The pick-at-most-two-of-three rule was violated: a researcher's code was
    /// configured with arbitrary code AND raw data access AND free egress. That is
    /// an exfiltration program by construction (PRIVACY §4) and is rejected.
    #[error(
        "researcher capabilities {{arbitrary_code, raw_data_access, free_egress}} are all true: \
         this is exfiltration-by-design (PRIVACY §4); a privacy tier must drop one"
    )]
    AllThreeCapabilities,
    /// An egress destination was not on the brokered allowlist (PRIVACY §6). Only
    /// the allowlist is reachable; arbitrary sockets are never permitted.
    #[error("egress to {host} denied: not on the brokered allowlist (PRIVACY §6)")]
    EgressDenied { host: String },
    /// A tier that requires a TEE referee was run without an attestation report.
    #[error("this tier requires a TEE attestation but none was supplied (PRIVACY §7)")]
    AttestationRequired,
    /// An attestation was supplied but did not even pass the structural shape check
    /// (non-empty evidence + measurement, matching TEE type). Fail-closed: a report
    /// that is not structurally valid cannot back a private run (PRIVACY §12).
    #[error("attestation failed structural validation (PRIVACY §12); it cannot back a private run")]
    AttestationInvalid,
}

// ---------------------------------------------------------------------------
// The three capabilities and the hard rule.
// ---------------------------------------------------------------------------

/// What a researcher's code is permitted to do inside a competition. The hard
/// rule (PRIVACY §4) is a constraint on this triple: at most two may be `true`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearcherCapabilities {
    /// The researcher ships a Turing-complete program (vs. configs/strategies into
    /// a fixed harness).
    pub arbitrary_code: bool,
    /// The researcher's code can read the proposer's raw private data in plaintext.
    pub raw_data_access: bool,
    /// The researcher's code can open arbitrary network sockets (vs. brokered,
    /// allowlist-only egress or none).
    pub free_egress: bool,
}

impl ResearcherCapabilities {
    /// The hard rule as arithmetic (PRIVACY §4): a configuration is exfiltration-safe
    /// iff it does **not** hold all three of `{arbitrary_code, raw_data_access,
    /// free_egress}`. Arbitrary code + raw data + an open socket *is* a data-export
    /// program; dropping any one of the three breaks the export path.
    #[must_use]
    pub fn exfiltration_safe(&self) -> bool {
        !(self.arbitrary_code && self.raw_data_access && self.free_egress)
    }

    /// Fail-closed validation of the hard rule.
    ///
    /// # Errors
    /// [`PrivacyError::AllThreeCapabilities`] if all three capabilities are present —
    /// the one configuration the rule forbids.
    pub fn validate(&self) -> Result<(), PrivacyError> {
        if self.exfiltration_safe() {
            Ok(())
        } else {
            Err(PrivacyError::AllThreeCapabilities)
        }
    }
}

// ---------------------------------------------------------------------------
// Privacy tiers (PRIVACY §5).
// ---------------------------------------------------------------------------

/// The privacy tier of a `Private` competition. Each tier is one application of
/// the hard rule: it names the *one* capability dropped (PRIVACY §4 table, §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrivacyTier {
    /// Drop raw data access. The researcher sees only a **score** — Kaggle-style.
    /// Maximum privacy (nothing to exfiltrate); weakest research signal (PRIVACY §5.1).
    BlackBox,
    /// Refinement of [`PrivacyTier::BlackBox`]: still no raw data access, but the
    /// feedback widens from a bare score to PII-stripped **aggregate diagnostics**
    /// and synthetic/redacted exemplars (PRIVACY §5.2). The default when feedback is
    /// needed. Leaks a controlled bit more through the score channel, never raw data.
    RedactedFeedback,
    /// Drop free egress. The researcher's **arbitrary code runs on the raw data** in
    /// a **no-network enclave**; only the gated output leaves (PRIVACY §5.3). Strong
    /// but covert-channel-bounded — not zero-leakage.
    WhiteBoxNoEgress,
    /// Drop arbitrary code. The researcher ships **strategies/configs** into a
    /// **measured, attested harness** that has raw data + brokered egress; the
    /// harness, not the researcher, decides what leaves (PRIVACY §5.4).
    AttestedHarness,
}

impl PrivacyTier {
    /// The single capability this tier drops to satisfy the hard rule (PRIVACY §4).
    #[must_use]
    pub fn dropped_capability(&self) -> &'static str {
        match self {
            // Black-box and redacted-feedback both keep "no raw data access".
            PrivacyTier::BlackBox | PrivacyTier::RedactedFeedback => "raw_data_access",
            PrivacyTier::WhiteBoxNoEgress => "free_egress",
            PrivacyTier::AttestedHarness => "arbitrary_code",
        }
    }

    /// The researcher-capability configuration this tier permits. By construction it
    /// drops exactly one of the three capabilities, so the returned config always
    /// passes [`ResearcherCapabilities::validate`] (asserted in the unit tests).
    #[must_use]
    pub fn capabilities(&self) -> ResearcherCapabilities {
        match self {
            // Drop raw data access; keep arbitrary code + free egress (it is theirs,
            // on their box, with no proprietary data to leak).
            PrivacyTier::BlackBox | PrivacyTier::RedactedFeedback => ResearcherCapabilities {
                arbitrary_code: true,
                raw_data_access: false,
                free_egress: true,
            },
            // Drop free egress; keep arbitrary code on plaintext, output-gated.
            PrivacyTier::WhiteBoxNoEgress => ResearcherCapabilities {
                arbitrary_code: true,
                raw_data_access: true,
                free_egress: false,
            },
            // Drop arbitrary code; keep raw data + (brokered) egress in a measured harness.
            PrivacyTier::AttestedHarness => ResearcherCapabilities {
                arbitrary_code: false,
                raw_data_access: true,
                free_egress: true,
            },
        }
    }

    /// Whether this tier's safety story relies on a TEE-backed referee/harness. The
    /// black-box / redacted-feedback default does **not** depend on attestation at
    /// all (PRIVACY §12) — it holds because the data never crosses to the researcher.
    /// The white-box and attested-harness tiers materially rely on it.
    #[must_use]
    pub fn requires_attestation(&self) -> bool {
        matches!(
            self,
            PrivacyTier::WhiteBoxNoEgress | PrivacyTier::AttestedHarness
        )
    }

    /// What kind of feedback a researcher in this tier may receive (PRIVACY §8).
    #[must_use]
    pub fn feedback_level(&self) -> FeedbackLevel {
        match self {
            PrivacyTier::BlackBox => FeedbackLevel::GateVerdictOnly,
            PrivacyTier::RedactedFeedback => FeedbackLevel::RedactedDiagnostics,
            // White-box / attested-harness researchers have (gated) access to the
            // data themselves, so withholding the lift number buys nothing.
            PrivacyTier::WhiteBoxNoEgress | PrivacyTier::AttestedHarness => {
                FeedbackLevel::FullPlaintext
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Feedback gating (PRIVACY §8 — the score channel).
// ---------------------------------------------------------------------------

/// How much information the researcher-facing feedback may carry. Ordered from
/// least to most revealing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeedbackLevel {
    /// A single bit: did the candidate clear the promotion gate? No lift number.
    GateVerdictOnly,
    /// Aggregate, PII-stripped diagnostics and a coarse summary; no exact lift, no
    /// raw exemplars (PRIVACY §5.2).
    RedactedDiagnostics,
    /// The full measured lift and measurement (no withholding).
    FullPlaintext,
}

/// The researcher-facing feedback for one scored candidate, gated to the tier's
/// [`FeedbackLevel`]. This is the value that crosses the score channel (boundary B2)
/// back to the researcher — never the referee's full outcome.
///
/// The information-withholding invariant, tested in this module: for
/// [`PrivacyTier::BlackBox`] the exact lift delta is **absent** from this value — it
/// is not recoverable from a [`ResearcherFeedback::Verdict`], which carries only a
/// boolean. For [`FeedbackLevel::FullPlaintext`] tiers the lift is present in full.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ResearcherFeedback {
    /// Black-box: only whether the gate was cleared. No lift number leaks.
    Verdict { cleared_gate: bool },
    /// Redacted-feedback: a coarse summary string plus the gate verdict. The summary
    /// carries no exact lift and no raw exemplars (PRIVACY §5.2).
    Redacted { summary: String, cleared_gate: bool },
    /// White-box / attested-harness: the full measured lift and measurement.
    Full {
        lift: Lift,
        measurement: Measurement,
    },
}

impl ResearcherFeedback {
    /// Whether the gate was cleared, available at every feedback level (a single bit
    /// is the minimum any researcher who submits at all necessarily learns).
    #[must_use]
    pub fn cleared_gate(&self) -> bool {
        match self {
            ResearcherFeedback::Verdict { cleared_gate }
            | ResearcherFeedback::Redacted { cleared_gate, .. } => *cleared_gate,
            ResearcherFeedback::Full { lift, measurement } => {
                Gate::default().clears(lift, measurement)
            }
        }
    }
}

/// Gate a measured result down to the feedback a researcher in `tier` may see.
///
/// This is the function that actually withholds information per tier (PRIVACY §8):
///
/// - [`PrivacyTier::BlackBox`] → [`ResearcherFeedback::Verdict`] carrying only the
///   gate boolean. The lift delta and the measurement are **dropped here** and never
///   reach the researcher — the only signal is one bit.
/// - [`PrivacyTier::RedactedFeedback`] → [`ResearcherFeedback::Redacted`] with a
///   coarse bucketed summary (aggregate, no exact lift, no raw exemplars).
/// - [`PrivacyTier::WhiteBoxNoEgress`] / [`PrivacyTier::AttestedHarness`] →
///   [`ResearcherFeedback::Full`] (these researchers already touch the data).
///
/// The score channel residual (PRIVACY §8) is real even at `GateVerdictOnly`: a bit
/// per submission still leaks. [`SubmissionBudget`] bounds the *number* of those
/// bits; it does not make the leak zero.
#[must_use]
pub fn redact(
    tier: PrivacyTier,
    lift: &Lift,
    measurement: &Measurement,
    gate: &Gate,
) -> ResearcherFeedback {
    let cleared = gate.clears(lift, measurement);
    match tier.feedback_level() {
        FeedbackLevel::GateVerdictOnly => ResearcherFeedback::Verdict {
            cleared_gate: cleared,
        },
        FeedbackLevel::RedactedDiagnostics => ResearcherFeedback::Redacted {
            // Coarse, bucketed summary: enough to steer, not enough to read the lift.
            // Deliberately NOT `lift.delta` — only a qualitative band and the n.
            summary: format!(
                "cleared={cleared}; improvement_band={}; episodes>={}",
                improvement_band(lift.delta),
                bucket_n(measurement.n),
            ),
            cleared_gate: cleared,
        },
        FeedbackLevel::FullPlaintext => ResearcherFeedback::Full {
            lift: *lift,
            measurement: *measurement,
        },
    }
}

/// Coarse, non-invertible band for a lift delta — the redacted channel reports the
/// *band*, never the number, so the exact lift cannot be read back (PRIVACY §5.2).
fn improvement_band(delta: f64) -> &'static str {
    if !delta.is_finite() || delta <= 0.0 {
        "none"
    } else if delta < 0.05 {
        "small"
    } else if delta < 0.20 {
        "moderate"
    } else {
        "large"
    }
}

/// Coarse power bucket — reports a floor on the episode count, not the exact `n`.
fn bucket_n(n: u32) -> u32 {
    // Round down to the nearest power-of-ten-ish floor so the exact count is hidden.
    match n {
        0..=11 => 0,
        12..=49 => 12,
        50..=199 => 50,
        _ => 200,
    }
}

// ---------------------------------------------------------------------------
// Submission budget — bounds (does not eliminate) the score-channel leak (§8).
// ---------------------------------------------------------------------------

/// A per-researcher cap on scoring submissions for a competition (PRIVACY §8).
///
/// Each scored submission leaks at most one feedback unit about the held-out set;
/// capping the count caps the *total* leakage. This **bounds** the score-channel
/// residual — it does not make it zero. Over-querying past the cap is the hook a
/// real deployment slashes on (MECHANISM "Submission rate-limits").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionBudget {
    pub max_submissions: u32,
    pub used: u32,
}

impl SubmissionBudget {
    #[must_use]
    pub fn new(max_submissions: u32) -> Self {
        Self {
            max_submissions,
            used: 0,
        }
    }

    /// Whether another submission is within budget.
    #[must_use]
    pub fn has_remaining(&self) -> bool {
        self.used < self.max_submissions
    }

    /// Consume one submission. Returns `true` if it was within budget and the count
    /// was incremented; `false` (no-op) if the budget is already exhausted. The
    /// caller enforces the rate-limit; this type only accounts for it.
    pub fn try_consume(&mut self) -> bool {
        if self.has_remaining() {
            self.used += 1;
            true
        } else {
            false
        }
    }

    /// Submissions still available before the leak bound is reached.
    #[must_use]
    pub fn remaining(&self) -> u32 {
        self.max_submissions.saturating_sub(self.used)
    }
}

// ---------------------------------------------------------------------------
// Brokered egress (PRIVACY §6).
// ---------------------------------------------------------------------------

/// A brokered-egress policy: only an explicit allowlist of destinations is
/// reachable, through a referee-controlled proxy — never arbitrary sockets
/// (PRIVACY §6). In a full deployment the proxy is the Tangle router running in its
/// own TEE so the broker itself is attested; this type encodes the *allowlist
/// decision*, the part that is host-independent and testable here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressPolicy {
    /// Exact-match hostnames the enclave may reach.
    pub allowlist: Vec<String>,
    /// When `true` (the safe default), any host not on the allowlist is denied.
    pub default_deny: bool,
}

impl EgressPolicy {
    /// A fail-closed policy: deny everything except `allowlist`.
    #[must_use]
    pub fn allowlisted(allowlist: Vec<String>) -> Self {
        Self {
            allowlist,
            default_deny: true,
        }
    }

    /// A no-egress policy (white-box no-egress, PRIVACY §5.3): nothing is reachable.
    #[must_use]
    pub fn no_egress() -> Self {
        Self {
            allowlist: Vec::new(),
            default_deny: true,
        }
    }

    /// Whether `host` is reachable under this policy. With `default_deny` (the safe
    /// configuration) only the allowlist is reachable; the open-by-default mode is
    /// provided only for non-private competitions and is never used by a tier.
    #[must_use]
    pub fn allows(&self, host: &str) -> bool {
        if self.allowlist.iter().any(|h| h == host) {
            return true;
        }
        !self.default_deny
    }

    /// Fail-closed checked egress: `Ok(())` if allowed, else
    /// [`PrivacyError::EgressDenied`].
    ///
    /// # Errors
    /// [`PrivacyError::EgressDenied`] if `host` is not reachable under this policy.
    pub fn check(&self, host: &str) -> Result<(), PrivacyError> {
        if self.allows(host) {
            Ok(())
        } else {
            Err(PrivacyError::EgressDenied {
                host: host.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_TIERS: [PrivacyTier; 4] = [
        PrivacyTier::BlackBox,
        PrivacyTier::RedactedFeedback,
        PrivacyTier::WhiteBoxNoEgress,
        PrivacyTier::AttestedHarness,
    ];

    fn good_lift() -> Lift {
        Lift {
            delta: 0.35,
            ci_lower: 0.30,
            ci_upper: 0.40,
            n: 80,
        }
    }

    fn good_measurement() -> Measurement {
        Measurement {
            value: 0.85,
            ci_lower: 0.80,
            ci_upper: 0.90,
            n: 80,
            cost: 80.0,
        }
    }

    // --- the hard rule (PRIVACY §4) ---------------------------------------

    #[test]
    fn all_three_capabilities_is_rejected() {
        let all_three = ResearcherCapabilities {
            arbitrary_code: true,
            raw_data_access: true,
            free_egress: true,
        };
        assert!(!all_three.exfiltration_safe());
        assert_eq!(
            all_three.validate(),
            Err(PrivacyError::AllThreeCapabilities)
        );
    }

    #[test]
    fn every_pair_of_two_is_safe() {
        // All three "drop exactly one" configurations are safe; the only unsafe one
        // is all-three (tested above).
        let pairs = [
            (false, true, true),  // drop arbitrary_code
            (true, false, true),  // drop raw_data_access
            (true, true, false),  // drop free_egress
            (true, false, false), // dropping more is also safe
            (false, false, false),
        ];
        for (ac, rd, fe) in pairs {
            let caps = ResearcherCapabilities {
                arbitrary_code: ac,
                raw_data_access: rd,
                free_egress: fe,
            };
            assert!(caps.exfiltration_safe(), "{caps:?} should be safe");
            assert!(caps.validate().is_ok());
        }
    }

    #[test]
    fn every_tier_capability_config_passes_validate() {
        // The core invariant: each tier drops exactly one capability, so its config
        // can never be the forbidden all-three.
        for tier in ALL_TIERS {
            let caps = tier.capabilities();
            assert!(
                caps.validate().is_ok(),
                "{tier:?} capabilities must satisfy the hard rule: {caps:?}"
            );
            // And the dropped capability is actually false in the returned config.
            let dropped = tier.dropped_capability();
            let is_false = match dropped {
                "arbitrary_code" => !caps.arbitrary_code,
                "raw_data_access" => !caps.raw_data_access,
                "free_egress" => !caps.free_egress,
                other => panic!("unexpected dropped capability {other}"),
            };
            assert!(is_false, "{tier:?} claims to drop {dropped} but it is set");
        }
    }

    // --- feedback gating / information withholding (PRIVACY §8) ------------

    #[test]
    fn blackbox_feedback_withholds_the_lift() {
        let lift = good_lift();
        let fb = redact(
            PrivacyTier::BlackBox,
            &lift,
            &good_measurement(),
            &Gate::default(),
        );
        // The feedback is a bare verdict — it carries only a boolean.
        match &fb {
            ResearcherFeedback::Verdict { cleared_gate } => assert!(*cleared_gate),
            other => panic!("black-box must yield a bare verdict, got {other:?}"),
        }
        // The exact lift delta is NOT recoverable from the feedback. Serializing the
        // entire feedback and searching for the delta proves the number never crosses
        // the channel.
        let serialized = serde_json::to_string(&fb).unwrap();
        assert!(
            !serialized.contains("0.35"),
            "black-box feedback must not leak the lift delta: {serialized}"
        );
        assert!(
            !serialized.contains("delta"),
            "black-box feedback must not even carry a lift field: {serialized}"
        );
    }

    #[test]
    fn redacted_feedback_gives_a_band_not_the_number() {
        let lift = good_lift();
        let fb = redact(
            PrivacyTier::RedactedFeedback,
            &lift,
            &good_measurement(),
            &Gate::default(),
        );
        match &fb {
            ResearcherFeedback::Redacted {
                summary,
                cleared_gate,
            } => {
                assert!(*cleared_gate);
                // A qualitative band, never the exact delta.
                assert!(summary.contains("improvement_band=large"));
                assert!(
                    !summary.contains("0.35"),
                    "redacted summary must not carry the exact lift: {summary}"
                );
            }
            other => panic!("redacted tier must yield Redacted, got {other:?}"),
        }
    }

    #[test]
    fn whitebox_and_attested_feedback_reveal_the_full_lift() {
        for tier in [PrivacyTier::WhiteBoxNoEgress, PrivacyTier::AttestedHarness] {
            let lift = good_lift();
            let fb = redact(tier, &lift, &good_measurement(), &Gate::default());
            match fb {
                ResearcherFeedback::Full {
                    lift: got,
                    measurement: _,
                } => {
                    assert_eq!(got.delta, lift.delta, "{tier:?} must reveal the full lift");
                }
                other => panic!("{tier:?} must yield Full feedback, got {other:?}"),
            }
        }
    }

    #[test]
    fn cleared_gate_is_consistent_across_levels() {
        // A failing candidate reports cleared=false at every level.
        let weak = Lift {
            delta: 0.001,
            ci_lower: -0.01,
            ci_upper: 0.02,
            n: 80,
        };
        for tier in ALL_TIERS {
            let fb = redact(tier, &weak, &good_measurement(), &Gate::default());
            assert!(!fb.cleared_gate(), "{tier:?} weak lift must not clear");
        }
    }

    // --- submission budget (PRIVACY §8 residual) --------------------------

    #[test]
    fn submission_budget_bounds_queries() {
        let mut budget = SubmissionBudget::new(2);
        assert_eq!(budget.remaining(), 2);
        assert!(budget.try_consume());
        assert!(budget.try_consume());
        assert_eq!(budget.remaining(), 0);
        // The third is refused — the leak is bounded to two feedback units.
        assert!(!budget.try_consume());
        assert!(!budget.has_remaining());
        assert_eq!(budget.used, 2);
    }

    // --- brokered egress (PRIVACY §6) -------------------------------------

    #[test]
    fn egress_allowlist_permits_only_listed_hosts() {
        let policy = EgressPolicy::allowlisted(vec!["model.endpoint".into()]);
        assert!(policy.allows("model.endpoint"));
        assert!(!policy.allows("evil.example"));
        assert_eq!(
            policy.check("evil.example"),
            Err(PrivacyError::EgressDenied {
                host: "evil.example".into()
            })
        );
        assert!(policy.check("model.endpoint").is_ok());
    }

    #[test]
    fn no_egress_reaches_nothing() {
        let policy = EgressPolicy::no_egress();
        assert!(!policy.allows("model.endpoint"));
        assert!(!policy.allows("anything"));
    }

    #[test]
    fn requires_attestation_only_for_whitebox_tiers() {
        assert!(!PrivacyTier::BlackBox.requires_attestation());
        assert!(!PrivacyTier::RedactedFeedback.requires_attestation());
        assert!(PrivacyTier::WhiteBoxNoEgress.requires_attestation());
        assert!(PrivacyTier::AttestedHarness.requires_attestation());
    }
}
