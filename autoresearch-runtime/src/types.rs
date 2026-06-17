//! Core domain types for the autoresearch-competitions market.
//!
//! These types are the off-chain, strongly-typed domain model. The on-chain
//! ABI layer (in `autoresearch-competitions-lib`) carries serialized / sealed
//! references to most of these; the chain never stores artifacts or data, only
//! commitments, certified scores, attestation hashes, and payouts.
//!
//! Terminology and invariants follow `SPEC.md`, `docs/ARCHITECTURE.md`, and
//! `docs/MECHANISM.md`. Naming here is `(proposed)` until the contracts freeze.

use serde::{Deserialize, Serialize};

/// On-chain identifier for a competition. Allocated by the CompetitionManager
/// contract at `CREATE_COMPETITION` time.
pub type CompetitionId = u64;

/// A 20-byte EVM address rendered as a `0x`-prefixed lowercase hex string.
/// Kept as a string at this layer to avoid pulling `alloy` into the core crate;
/// the ABI layer converts to/from `alloy_primitives::Address`.
pub type Address = String;

/// Content-addressed or sealed reference to an artifact, a scorer, a surface
/// definition, or a data split. The string is opaque to this crate: it may be a
/// CID, an S3 URL, a sealed-secret handle, or a TEE-scoped reference. The chain
/// stores at most the keccak hash of one of these — never the bytes behind it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactRef(pub String);

// ---------------------------------------------------------------------------
// The four orthogonal knobs (SPEC.md §4). Every competition is one point in
// this 4-D space.
// ---------------------------------------------------------------------------

/// Knob 1 — how researchers relate to one another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Structure {
    /// Researchers submit separate candidates that are ranked; pay top-k.
    Competitive,
    /// Researchers pool compute on one shared artifact; pay by contribution share.
    Collaborative,
}

/// Knob 2 — when the competition settles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cadence {
    /// Single deadline, terminal payout.
    OneShot,
    /// King-of-the-hill; the leaderboard keeps moving and rewards the marginal
    /// improvement over the current best, settled per-epoch.
    Continuous,
}

/// Knob 3 — who may see the proposer's inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    /// Open, viral arena.
    Public,
    /// Sealed; the proposer's data/scorer is not disclosed to researchers.
    Private,
}

/// Knob 4 — what kind of referee computes the score.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScorerKind {
    /// A held-out evaluation suite (e.g. an agent-profile evaluator).
    HeldOutEval,
    /// A hidden reference oracle the researcher never sees (e.g. the quantum case).
    PrivateOracle,
    /// Privileged or expensive hardware (e.g. a quantum device, a proprietary sim).
    PrivilegedHardware,
    /// A panel of human judges.
    HumanPanel,
}

/// The full knob setting for a competition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Knobs {
    pub structure: Structure,
    pub cadence: Cadence,
    pub visibility: Visibility,
    pub scorer_kind: ScorerKind,
}

impl Knobs {
    /// Reject knob combinations that are nonsensical (SPEC.md §4 coherence matrix).
    /// This is a *coherence* check, not a policy check.
    pub fn validate(&self) -> Result<(), &'static str> {
        // Collaborative is OneShot in this milestone (M6). A single shared artifact has
        // no "current best to beat by a margin", which is exactly what the Continuous
        // (king-of-the-hill / marginal-over-best) cadence rewards; the per-epoch
        // contribution split that would make Continuous the natural grain for
        // Collaborative is deferred (see SPEC.md §4 coherence matrix and ROADMAP.md M6,
        // both of which mark Collaborative as OneShot-in-M6 with Continuous proposed).
        // Until that path is implemented, Collaborative × Continuous is rejected so the
        // runner never silently runs a mode it does not implement.
        if self.structure == Structure::Collaborative && self.cadence == Cadence::Continuous {
            return Err(
                "Collaborative competitions are OneShot in M6; per-epoch Continuous contribution accounting is deferred (SPEC.md §4)",
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Which data split a scorer is run against.
// ---------------------------------------------------------------------------

/// The data partition a [`crate::Scorer`] runs against. Researchers may receive
/// `Dev` signal (depending on the privacy tier); only the Referee ever runs
/// `HeldOut`, and its tasks are never disclosed to researchers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Split {
    /// Public/dev split researchers may hill-climb against.
    Dev,
    /// Private held-out split that decides payment. Referee-only.
    HeldOut,
}

// ---------------------------------------------------------------------------
// Measurements, lift, and the promotion gate.
// ---------------------------------------------------------------------------

/// A single certified measurement of an artifact on a split, with a confidence
/// interval and the sample size behind it. Mirrors the agent-profile stand-in
/// evidence row (`{value, ci, n, cost}`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    /// Point estimate of the score (units are scorer-defined).
    pub value: f64,
    /// Lower bound of the confidence interval on `value`.
    pub ci_lower: f64,
    /// Upper bound of the confidence interval on `value`.
    pub ci_upper: f64,
    /// Number of episodes behind the estimate.
    pub n: u32,
    /// Total cost of producing the measurement (scorer-defined units, e.g. USD).
    pub cost: f64,
}

/// Certified improvement of a candidate over a baseline, with a CI on the delta.
/// The reward layer consumes the integer micro-unit form; this f64 form is for
/// gates, display, and the evidence ledger.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lift {
    /// `candidate.value - baseline.value`.
    pub delta: f64,
    /// Lower CI bound of the delta. The gate keys off this (not the point estimate).
    pub ci_lower: f64,
    /// Upper CI bound of the delta.
    pub ci_upper: f64,
    /// Episodes behind the delta. The M1 estimator combines candidate and baseline
    /// measurements *unpaired*; paired replay tightens this CI — see the `lift`
    /// module in `autoresearch-protocol`.
    pub n: u32,
}

/// The promotion gate a candidate must clear to be eligible for payout.
/// Defaults follow the agent-profile stand-in (`minLiftCiLower = 0.02`, `n >= 12`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    /// The lower CI bound of the lift must be at least this (default 0.02 = 2pp).
    pub min_lift_ci_lower: f64,
    /// Optional per-task cost ceiling; `None` means uncapped.
    pub cost_per_task_ceiling: Option<f64>,
    /// Minimum episode count for a result to be admissible (default 12).
    pub min_n: u32,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            min_lift_ci_lower: 0.02,
            cost_per_task_ceiling: None,
            min_n: 12,
        }
    }
}

impl Gate {
    /// True iff the lift (and its supporting measurement) clears the gate.
    ///
    /// Fail-closed: any missing power, non-finite (`NaN`/`inf`) input, or insufficient
    /// lower bound fails. The finiteness guards are first and the bound checks are
    /// written as positive `>=` assertions (which are `false` for `NaN`), so a
    /// candidate whose lift is `NaN` — e.g. propagated from a buggy or adversarial
    /// scorer — can never clear the gate and be paid.
    // The negated comparisons below (`!(x >= y)`, `!(x <= y)`) are deliberate NaN
    // fail-closed guards and are NOT equivalent to `x < y` / `x > y`: `!(x >= y)` is
    // `true` when `x` is `NaN`, whereas `x < y` is `false`. Rewriting them as clippy
    // suggests would let a `NaN` lift slip through the gate and be paid, so the lint is
    // suppressed here intentionally.
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    pub fn clears(&self, lift: &Lift, measurement: &Measurement) -> bool {
        // Reject non-finite lift inputs outright: `NaN`/`inf` must never pay.
        if !lift.delta.is_finite() || !lift.ci_lower.is_finite() {
            return false;
        }
        // Sufficient statistical power.
        if lift.n < self.min_n {
            return false;
        }
        // Positive assertion: `>=` is `false` for `NaN`, so this is fail-closed.
        if !(lift.ci_lower >= self.min_lift_ci_lower) {
            return false;
        }
        if let Some(ceiling) = self.cost_per_task_ceiling {
            // A non-finite cost ceiling check must also fail closed.
            if !measurement.cost.is_finite() {
                return false;
            }
            let per_task = if measurement.n == 0 {
                f64::INFINITY
            } else {
                measurement.cost / f64::from(measurement.n)
            };
            // `per_task <= ceiling` is `false` for `NaN`/`inf` per_task — fail-closed.
            if !(per_task <= ceiling) {
                return false;
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Evidence ledger.
// ---------------------------------------------------------------------------

/// Provenance class of a piece of evidence. Observational evidence is badged but
/// never promotes (it is confounded); see `docs/MECHANISM.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceKind {
    /// Full re-execution of the task with the candidate artifact (Tier A).
    ReplayFull,
    /// Tool-mocked deterministic replay against logged tool calls (Tier B).
    ReplayMocked,
    /// Observational only (Tier C) — confounded, never promotes.
    Observational,
    /// Scored by a private reference oracle.
    Oracle,
    /// Scored by privileged hardware.
    Hardware,
    /// Scored by a human panel.
    Human,
}

impl EvidenceKind {
    /// Whether evidence of this kind is ever eligible to promote / pay out.
    pub fn can_promote(&self) -> bool {
        !matches!(self, EvidenceKind::Observational)
    }
}

/// One certified row in a candidate's evidence ledger.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub kind: EvidenceKind,
    pub lift: Lift,
    pub measurement: Measurement,
    /// True if the evidence is confounded (observational); such rows never pay.
    pub confounded: bool,
    /// Reference to the suite / split the evidence was produced against.
    pub suite_ref: ArtifactRef,
    /// keccak hash of the TEE attestation under which scoring ran, hex-encoded.
    /// Committed on-chain by `REPORT_SCORE`; empty for non-TEE referees.
    pub attestation_hash: String,
}

// ---------------------------------------------------------------------------
// Candidates.
// ---------------------------------------------------------------------------

/// A researcher's submission. Through the commit-reveal flow a candidate exists
/// first as only a `commitment` (a hash), then gains its `artifact_ref` at
/// reveal, then its `evidence` once the Referee certifies it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// Stable id (the reveal commitment, hex-encoded) — also the dedupe key.
    pub id: String,
    pub competition: CompetitionId,
    pub researcher: Address,
    /// keccak256(abi.encode(artifact_ref, salt)), committed before the deadline.
    pub commitment: String,
    /// Set at reveal; `None` while still committed.
    pub artifact_ref: Option<ArtifactRef>,
    /// Set after the Referee scores it; `None` until certified.
    pub evidence: Option<Evidence>,
}

impl Candidate {
    /// A candidate is payable iff it has been revealed, certified with
    /// promotable (non-confounded) evidence, and that evidence clears the gate.
    pub fn is_payable(&self, gate: &Gate) -> bool {
        match (&self.artifact_ref, &self.evidence) {
            (Some(_), Some(ev)) => {
                ev.kind.can_promote() && !ev.confounded && gate.clears(&ev.lift, &ev.measurement)
            }
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Competition specification.
// ---------------------------------------------------------------------------

/// The full off-chain specification a proposer authors. A sealed / hashed form
/// of this is referenced on-chain at `CREATE_COMPETITION`; the bytes stay off-chain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompetitionSpec {
    pub knobs: Knobs,
    pub gate: Gate,
    /// Sealed reference to the held-out scorer (Referee-resolved).
    pub scorer_ref: ArtifactRef,
    /// Reference to the surface definition (what may change).
    pub surface_ref: ArtifactRef,
    /// Reference to the baseline artifact lift is measured against.
    pub baseline_ref: ArtifactRef,
    /// Total reward pool in wei.
    pub reward_pool_wei: u128,
    /// ERC-20 reward asset (or the zero address for native).
    pub reward_asset: Address,
    /// Unix deadline (OneShot) or epoch boundary cursor (Continuous).
    pub deadline: u64,
}

impl CompetitionSpec {
    /// Structural validity check independent of any external state.
    pub fn validate(&self) -> Result<(), String> {
        self.knobs.validate().map_err(str::to_string)?;
        if self.reward_pool_wei == 0 {
            return Err("reward pool must be non-zero".into());
        }
        if self.gate.min_n == 0 {
            return Err("gate min_n must be positive".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_measurement() -> Measurement {
        Measurement {
            value: 0.85,
            ci_lower: 0.80,
            ci_upper: 0.90,
            n: 80,
            cost: 80.0,
        }
    }

    #[test]
    fn gate_clears_a_clean_well_powered_win() {
        let gate = Gate::default();
        let lift = Lift {
            delta: 0.35,
            ci_lower: 0.30,
            ci_upper: 0.40,
            n: 80,
        };
        assert!(gate.clears(&lift, &clean_measurement()));
    }

    #[test]
    fn gate_is_fail_closed_against_nan_ci_lower() {
        // A NaN lower bound must NOT clear, even with ample power. `NaN < min` is
        // `false` in Rust, so the old `<` form would have wrongly passed this through.
        let gate = Gate::default();
        let lift = Lift {
            delta: 0.35,
            ci_lower: f64::NAN,
            ci_upper: 0.40,
            n: 80, // well above min_n
        };
        assert!(!gate.clears(&lift, &clean_measurement()));
    }

    #[test]
    fn gate_is_fail_closed_against_nan_delta() {
        let gate = Gate::default();
        let lift = Lift {
            delta: f64::NAN,
            ci_lower: 0.30,
            ci_upper: 0.40,
            n: 80,
        };
        assert!(!gate.clears(&lift, &clean_measurement()));
    }

    #[test]
    fn gate_is_fail_closed_against_infinite_ci_lower() {
        let gate = Gate::default();
        let lift = Lift {
            delta: 0.35,
            ci_lower: f64::INFINITY,
            ci_upper: f64::INFINITY,
            n: 80,
        };
        assert!(!gate.clears(&lift, &clean_measurement()));
    }

    #[test]
    fn gate_is_fail_closed_against_nan_cost_when_ceiling_set() {
        let gate = Gate {
            min_lift_ci_lower: 0.02,
            cost_per_task_ceiling: Some(10.0),
            min_n: 12,
        };
        let lift = Lift {
            delta: 0.35,
            ci_lower: 0.30,
            ci_upper: 0.40,
            n: 80,
        };
        let measurement = Measurement {
            value: 0.85,
            ci_lower: 0.80,
            ci_upper: 0.90,
            n: 80,
            cost: f64::NAN,
        };
        assert!(!gate.clears(&lift, &measurement));
    }

    #[test]
    fn gate_rejects_insufficient_power() {
        let gate = Gate::default();
        let lift = Lift {
            delta: 0.35,
            ci_lower: 0.30,
            ci_upper: 0.40,
            n: 1, // below min_n (12)
        };
        assert!(!gate.clears(&lift, &clean_measurement()));
    }

    #[test]
    fn gate_rejects_insufficient_lower_bound() {
        let gate = Gate::default();
        let lift = Lift {
            delta: 0.35,
            ci_lower: 0.01, // below min_lift_ci_lower (0.02)
            ci_upper: 0.40,
            n: 80,
        };
        assert!(!gate.clears(&lift, &clean_measurement()));
    }
}
