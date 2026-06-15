//! Dispute resolution: the m-of-n Validator committee that re-scores a challenged
//! candidate and decides whether the original certification stands.
//!
//! A `CHALLENGE` is the only path that activates Validators (MECHANISM.md §7). We
//! **attest once and re-score only on dispute** — never re-score every artifact
//! on-chain (SPEC §11). On a challenge an m-of-n committee independently re-runs
//! the Scorer on the held-out split and signs the result; the off-chain mechanism
//! here is real and tested. The on-chain k-of-n EIP-712 signature verification of
//! those signed verdicts is a documented seam (see `CompetitionManager.sol`'s
//! `resolveDispute`, which mirrors the trading blueprint's `TradeValidator` m-of-n
//! EIP-712 pattern, default 2-of-3, score threshold >= 50).
//!
//! Two pieces:
//! - [`committee_verdict`] — the **pure, deterministic** m-of-n aggregation. Given
//!   the original `clears` decision and each validator's independent verdict, it
//!   returns [`DisputeOutcome`]. This is the tolerant-of-a-faulty-minority core,
//!   testable against mixed honest/Byzantine inputs without any I/O.
//! - [`collect_verdicts`] — the **real re-score path**: it re-runs the Scorer per
//!   validator and forms each validator's verdict from the freshly measured lift.
//!   In M1 the Scorer is deterministic, so honest validators agree by construction;
//!   that is exactly what makes a single Byzantine validator a *minority* the
//!   committee tolerates.

use autoresearch_runtime::traits::{Scorer, Surface};
use autoresearch_runtime::types::{Gate, Lift, Split};

use crate::lift::estimate_lift;
use crate::orchestrator::ProtocolError;

/// One Validator's independent re-score verdict on a challenged candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatorVerdict {
    /// The validator's identity (an address at the on-chain layer).
    pub validator: String,
    /// Whether this validator found the candidate clears the gate.
    pub clears: bool,
    /// The lift this validator measured re-running the Scorer.
    pub lift: Lift,
}

/// The committee's decision relative to the original certification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisputeOutcome {
    /// At least `m` validators agree with the original `clears` decision: the
    /// certification stands. The challenger was wrong.
    Upheld,
    /// At least `m` validators agree on the *opposite* of the original decision:
    /// the certification was wrong and is overturned. The challenger was right.
    Overturned,
    /// Neither side reached the `m`-quorum: no decision. Conservative default — no
    /// stake moves on an inconclusive dispute (MECHANISM.md §7, fail-closed).
    Inconclusive,
}

/// Aggregate validator verdicts into a [`DisputeOutcome`] under m-of-n quorum.
///
/// Each validator independently re-decides whether the candidate clears the gate.
/// We compare each to `original_clears`:
/// - a validator that *agrees* with the original supports **Upheld**;
/// - a validator that *disagrees* (the opposite boolean) supports **Overturned**.
///
/// The outcome is:
/// - [`DisputeOutcome::Upheld`] iff `>= m` validators agree with `original_clears`;
/// - [`DisputeOutcome::Overturned`] iff `>= m` validators agree on the opposite;
/// - [`DisputeOutcome::Inconclusive`] otherwise (no side reached quorum).
///
/// This is **pure and deterministic**: identical inputs always yield identical
/// outputs, with no I/O, clock, or RNG. With a default 2-of-3 committee
/// (`m = 2`), a single Byzantine validator can never force either outcome on its
/// own — it is outvoted by the two honest validators who, re-running the same
/// deterministic Scorer, agree. That is the m-of-n fault tolerance.
///
/// # Strict-majority quorum guard (fail closed)
///
/// The fault tolerance above holds **only** when the quorum is a strict majority of
/// the committee, i.e. `m > n/2`. If `m <= n/2`, two *disjoint* quorums of size `m`
/// can co-exist, so a faulty MINORITY could reach quorum on the wrong side and flip
/// a correct verdict (the maximally destructive [`DisputeOutcome::Overturned`], which
/// slashes the honest researcher). We therefore reject a sub-majority quorum outright:
/// a committee config with `2*m <= n` is invalid and resolves to
/// [`DisputeOutcome::Inconclusive`] (no stake moves), never to a partisan outcome a
/// minority could have forced. With `m > n/2` the two quorums cannot both be reached,
/// so a sub-majority of faulty validators can never decide.
///
/// `m == 0` is treated as `m == 1`: a quorum of zero is meaningless and would let
/// an empty committee "decide", so we fail closed to requiring at least one vote.
#[must_use]
pub fn committee_verdict(
    original_clears: bool,
    verdicts: &[ValidatorVerdict],
    m: usize,
) -> DisputeOutcome {
    let quorum = m.max(1);
    let n = verdicts.len();

    // Fail closed on a sub-majority quorum. Unless the quorum is a strict majority of
    // the committee (`quorum > n/2`, i.e. `2*quorum > n`), two disjoint quorums could
    // co-exist and a faulty minority could force a (wrong) outcome. An invalid config
    // resolves to Inconclusive — the conservative default that moves no stake.
    if 2 * quorum <= n {
        return DisputeOutcome::Inconclusive;
    }

    let agree = verdicts
        .iter()
        .filter(|v| v.clears == original_clears)
        .count();
    let disagree = n - agree;

    // With the strict-majority guard above, at most one side can reach quorum, so the
    // (true, true) state is unreachable. We still pattern-match all four states for
    // totality and route the (impossible) tie to Inconclusive — the conservative
    // default — rather than to a partisan outcome.
    let upheld = agree >= quorum;
    let overturned = disagree >= quorum;
    match (upheld, overturned) {
        (true, true) => DisputeOutcome::Inconclusive, // unreachable under m > n/2
        (false, true) => DisputeOutcome::Overturned,
        (true, false) => DisputeOutcome::Upheld,
        (false, false) => DisputeOutcome::Inconclusive,
    }
}

/// Re-run the Scorer once per validator and form each validator's independent
/// verdict from the freshly measured lift over the baseline.
///
/// This is the **real** re-score path, not a mock: each validator scores the
/// `candidate` and the `baseline` on [`Split::HeldOut`] through the supplied
/// [`Scorer`], estimates the lift ([`estimate_lift`]), and decides `clears` via the
/// competition [`Gate`]. In M1 the Scorer is deterministic, so every honest
/// validator computes the *same* lift and the *same* `clears` — which is precisely
/// why injecting one Byzantine verdict into [`committee_verdict`] leaves it a
/// tolerated minority.
///
/// The `validators` slice supplies the committee identities; the production wiring
/// would hand each validator its own TEE-isolated Scorer instance. Here they share
/// one deterministic instance, which is the honest M1 model: independent runs of a
/// deterministic scorer are bit-identical.
///
/// # Errors
/// Propagates [`ProtocolError::Scorer`] if any re-score fails; in M1 a scoring
/// failure is a bug, not a flaky peer, so it aborts the collection.
pub async fn collect_verdicts<S, Sc>(
    scorer: &Sc,
    surface: &S,
    candidate: &S::Artifact,
    baseline: &S::Artifact,
    gate: &Gate,
    validators: &[String],
) -> Result<Vec<ValidatorVerdict>, ProtocolError>
where
    S: Surface,
    Sc: Scorer<Artifact = S::Artifact>,
{
    // Surface-validate the disputed artifact before any (expensive) re-score: an
    // artifact that fails the surface contract can never clear, fail closed.
    surface.validate(candidate)?;

    // The baseline bar, measured once on the held-out split (shared across the
    // committee — the deterministic scorer gives every validator the same value).
    let baseline_measurement = scorer.score(baseline, Split::HeldOut).await?;

    let mut verdicts = Vec::with_capacity(validators.len());
    for validator in validators {
        let measurement = scorer.score(candidate, Split::HeldOut).await?;
        let lift = estimate_lift(&measurement, &baseline_measurement);
        let clears = gate.clears(&lift, &measurement);
        verdicts.push(ValidatorVerdict {
            validator: validator.clone(),
            clears,
            lift,
        });
    }
    Ok(verdicts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lift(delta: f64) -> Lift {
        Lift {
            delta,
            ci_lower: delta - 0.05,
            ci_upper: delta + 0.05,
            n: 80,
        }
    }

    fn verdict(name: &str, clears: bool) -> ValidatorVerdict {
        ValidatorVerdict {
            validator: name.into(),
            clears,
            lift: lift(if clears { 0.30 } else { 0.0 }),
        }
    }

    /// Honest majority agreeing with a correct original certification → Upheld.
    /// Default 2-of-3 committee, all three honest validators re-score and agree.
    #[test]
    fn upheld_on_honest_unanimous_agreement() {
        let original_clears = true;
        let verdicts = vec![
            verdict("v1", true),
            verdict("v2", true),
            verdict("v3", true),
        ];
        assert_eq!(
            committee_verdict(original_clears, &verdicts, 2),
            DisputeOutcome::Upheld
        );
    }

    /// A single Byzantine validator in a 2-of-3 committee cannot overturn a correct
    /// certification: the two honest validators still form the 2-quorum for Upheld,
    /// and the lone faulty vote falls short of its own 2-quorum for Overturned.
    #[test]
    fn byzantine_minority_cannot_overturn_correct_certification() {
        let original_clears = true;
        let verdicts = vec![
            verdict("honest-1", true),
            verdict("honest-2", true),
            verdict("byzantine", false), // lies that it does not clear
        ];
        assert_eq!(
            committee_verdict(original_clears, &verdicts, 2),
            DisputeOutcome::Upheld,
            "one faulty validator must not flip a correct certification"
        );
    }

    /// A fraudulent certification (original said `clears=true` for a candidate that
    /// truly does not) is overturned when the honest majority disagrees — even with
    /// one Byzantine validator propping up the fraud.
    #[test]
    fn overturns_fraudulent_certification_despite_byzantine_support() {
        // The original certification fraudulently claims the candidate clears.
        let original_clears = true;
        let verdicts = vec![
            verdict("honest-1", false), // truly does not clear
            verdict("honest-2", false),
            verdict("byzantine", true), // colludes with the fraud
        ];
        assert_eq!(
            committee_verdict(original_clears, &verdicts, 2),
            DisputeOutcome::Overturned,
            "honest majority must overturn a fraudulent score"
        );
    }

    /// No-quorum: a 3-validator committee split such that neither side reaches the
    /// 2-quorum yields Inconclusive (here `m = 3` with a 2/1 split).
    #[test]
    fn inconclusive_when_no_side_reaches_quorum() {
        let original_clears = true;
        let verdicts = vec![
            verdict("v1", true),
            verdict("v2", true),
            verdict("v3", false),
        ];
        // Need 3 agreeing to uphold or 3 disagreeing to overturn; neither holds.
        assert_eq!(
            committee_verdict(original_clears, &verdicts, 3),
            DisputeOutcome::Inconclusive
        );
    }

    /// An empty committee can never decide: no quorum is reachable.
    #[test]
    fn empty_committee_is_inconclusive() {
        assert_eq!(
            committee_verdict(true, &[], 2),
            DisputeOutcome::Inconclusive
        );
    }

    /// `m == 0` is clamped to 1 so an empty committee still cannot decide, but a
    /// single vote suffices once present (degenerate 1-of-n).
    #[test]
    fn zero_quorum_is_clamped_to_one() {
        assert_eq!(
            committee_verdict(true, &[verdict("v1", true)], 0),
            DisputeOutcome::Upheld
        );
        assert_eq!(
            committee_verdict(true, &[], 0),
            DisputeOutcome::Inconclusive
        );
    }

    /// Sub-majority quorum (m <= n/2) must NOT let a faulty minority overturn a
    /// correct certification. n=4, m=2: a 2/2 split between honest-correct and
    /// Byzantine would, without the guard, reach the 2-quorum on BOTH sides and the
    /// old tie-break steered to the attacker-favoring Overturned. With the strict-
    /// majority guard (2*m <= n is invalid), the config fails closed to Inconclusive
    /// — no stake moves, the honest researcher is not slashed.
    #[test]
    fn submajority_quorum_n4_m2_cannot_be_overturned_by_minority() {
        let original_clears = true; // truth = clears
        let verdicts = vec![
            verdict("honest-1", true),
            verdict("honest-2", true),
            verdict("byzantine-1", false),
            verdict("byzantine-2", false),
        ];
        assert_eq!(
            committee_verdict(original_clears, &verdicts, 2),
            DisputeOutcome::Inconclusive,
            "m=2,n=4 is a sub-majority quorum: a Byzantine minority must not overturn"
        );
    }

    /// n=5, m=2: 3 honest-correct vs only 2 Byzantine. The honest majority is correct,
    /// yet m=2 is a sub-majority (2*2 <= 5), so two disjoint 2-quorums exist and the
    /// Byzantine pair could reach the Overturned quorum. The guard rejects the invalid
    /// config (Inconclusive) instead of overturning a correct certification.
    #[test]
    fn submajority_quorum_n5_m2_cannot_be_overturned_by_minority() {
        let original_clears = true;
        let verdicts = vec![
            verdict("honest-1", true),
            verdict("honest-2", true),
            verdict("honest-3", true),
            verdict("byzantine-1", false),
            verdict("byzantine-2", false),
        ];
        assert_eq!(
            committee_verdict(original_clears, &verdicts, 2),
            DisputeOutcome::Inconclusive,
            "m=2,n=5 is a sub-majority quorum: a 2-validator minority must not overturn"
        );
    }

    /// The smallest valid strict-majority committee still works: n=5, m=3 with the
    /// honest majority agreeing upholds; a 2-validator Byzantine minority cannot reach
    /// the 3-quorum to overturn.
    #[test]
    fn strict_majority_n5_m3_upholds_correct_certification_over_minority() {
        let original_clears = true;
        let verdicts = vec![
            verdict("honest-1", true),
            verdict("honest-2", true),
            verdict("honest-3", true),
            verdict("byzantine-1", false),
            verdict("byzantine-2", false),
        ];
        assert_eq!(
            committee_verdict(original_clears, &verdicts, 3),
            DisputeOutcome::Upheld,
            "m=3,n=5 is a strict majority: honest 3 uphold, Byzantine 2 fall short"
        );
    }
}
