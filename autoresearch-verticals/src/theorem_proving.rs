//! Theorem-proving / formal-verification vertical: the autoresearch market drives a
//! **proof-search** competition where researchers submit *tactic sequences* and the
//! market pays only for a proof a checker **accepts** and that is **short**.
//!
//! Researchers submit a candidate proof encoded as a numeric *tactic vector* (the
//! [`GenericArtifact::params`] the generic [`GenericEngine`] searches). A
//! deterministic **proof checker** decides VALIDITY — the proof is accepted iff the
//! tactic vector lands in a *valid region* (a deterministic predicate over the
//! params, the stand-in for "the kernel type-checks every step") — and measures
//! proof SIZE (the number of tactic steps, the stand-in for proof length). The
//! market's Referee re-scores the produced proof on a held-out checker, gates it,
//! ranks, and pays. **Delegating the search never delegates the trust** — a
//! researcher's own dev signal is ignored for payment; only the held-out re-score
//! decides, and an *invalid* proof can never clear the gate.
//!
//! # value = HIGHER is better
//!
//! [`ProofScorer`] reports `value = valid ? (VALID_BONUS - size) : INVALID_VALUE`:
//!
//! - A **valid, short** proof scores just below [`VALID_BONUS`] (smaller proofs
//!   score higher), so the generic engine — which maximises the dev value — is
//!   pulled toward valid regions and then toward shorter proofs inside them.
//! - An **invalid** proof scores [`INVALID_VALUE`] (a large negative constant), far
//!   below any baseline, so it produces a negative lift and is *always* gated out.
//!   No amount of size-shrinking can rescue an invalid proof — exactly the binary
//!   "the checker rejects it" semantics formal verification has.
//!
//! # The honest seam — this is NOT a real proof kernel
//!
//! The checker here is a closed-form predicate over the tactic vector, not Lean /
//! Coq / Isabelle running a real kernel. It is the marked stand-in for a live
//! external prover backend. What it *does* prove is the **market mechanism around
//! delegated proving**: a binary accept/reject checker, held-out re-scoring of a
//! delegated proof, the promotion gate refusing invalid or over-long proofs, and a
//! real CI from a cheap repeated check.
//!
//! # Dev vs held-out — why the gate is meaningful
//!
//! The held-out checker is *stricter* than the dev checker: it requires the tactic
//! vector to sit a safety **margin** inside the valid region (the stand-in for a
//! proof that only type-checks against the exact dev lemma statements but breaks on
//! the held-out instantiation). A researcher that over-searches the dev signal can
//! drift to a point that is *barely* dev-valid — accepted by the dev checker, so its
//! dev value looks great — yet lands outside the held-out region and is **rejected**
//! on re-score, hence gated out. A researcher that lands solidly inside the region
//! is valid on both and, once short, clears the gate. This is the proof-domain
//! analogue of the distributed-training generalization gap.

use std::future::Future;

use autoresearch_generic_engine::{ArtifactKind, GenericArtifact};
use autoresearch_runtime::traits::{Scorer, ScorerError};
use autoresearch_runtime::types::{Measurement, Split};

// --- Proof-checker constants ------------------------------------------------
//
// A closed-form model of a tactic-based proof: the params are a point in tactic
// space, validity is "the point lands in the valid region", and size is "how many
// tactic steps the proof spends". The point is not kernel fidelity; it is to give
// the market a real binary-accept search surface with the shape of the proving
// tradeoff — a proof must FIRST be valid, and only then does being SHORT win.

/// Dimension of the tactic vector a proof is encoded as. Each coordinate is one
/// continuous "tactic knob" (which lemma to apply, with what argument).
pub const TACTIC_DIM: usize = 4;

/// The centre of the valid region: the tactic vector of a *correct* proof. A proof
/// is accepted when its vector is close enough to this target — the deterministic
/// stand-in for "every tactic step type-checks". Off-centre but in-region proofs are
/// still valid but spend more steps (are longer).
const TARGET: [f64; TACTIC_DIM] = [1.0, -1.0, 0.5, 2.0];

/// The dev checker's acceptance radius: a proof whose tactic vector is within this
/// (Euclidean) distance of [`TARGET`] is accepted by the *dev* checker.
const DEV_RADIUS: f64 = 1.5;

/// The held-out checker's acceptance radius. It is *stricter* (smaller) than the dev
/// radius: a proof must sit a safety margin inside the region to survive re-scoring.
/// A proof in the annulus `(HELDOUT_RADIUS, DEV_RADIUS]` is dev-valid but held-out
/// **invalid** — the generalization gap the gate exploits.
const HELDOUT_RADIUS: f64 = 1.0;

/// Score awarded to a valid proof of size 0 (an unreachable ideal). A valid proof
/// scores `VALID_BONUS - size`, so smaller proofs score higher and every valid proof
/// scores far above an invalid one.
pub const VALID_BONUS: f64 = 100.0;

/// Score of any *invalid* (checker-rejected) proof. A large negative constant — far
/// below the worst valid proof and far below the baseline — so an invalid proof
/// always yields a negative lift and is gated out. There is no size that rescues it.
pub const INVALID_VALUE: f64 = -1_000.0;

/// Proof "size" (tactic-step count proxy) charged for being off-centre in the valid
/// region. Distance `d` from [`TARGET`] costs `SIZE_PER_DIST * d` steps; a perfectly
/// centred proof spends only [`SIZE_FLOOR`] steps. This is what makes a *short* valid
/// proof beat a *long* valid one even though both are accepted.
const SIZE_PER_DIST: f64 = 8.0;

/// The minimum step count any proof spends (you cannot prove a non-trivial lemma in
/// zero steps). A centred proof scores `VALID_BONUS - SIZE_FLOOR`.
const SIZE_FLOOR: f64 = 2.0;

/// Std of per-check measurement noise (a non-deterministic-tactic-ordering proxy)
/// that gives a *valid* proof's score a real, non-degenerate CI. Invalid proofs are
/// reported with a tight CI — rejection is unambiguous.
const CHECK_NOISE: f64 = 0.02;

/// z for a two-sided 95% normal interval.
const Z_95: f64 = 1.96;

// --- Deterministic noise ----------------------------------------------------

/// A splitmix64 finalizer mapped to a uniform `f64` in `[-1, 1)`. Deterministic from
/// its input mix word — no `rand`, no clock, no I/O — so every measurement is
/// byte-reproducible, which is what lets the e2e test assert concrete lift. Mirrors
/// the `jitter` in `distributed_training`.
use crate::util::jitter;

// --- Proof checker ----------------------------------------------------------

/// The outcome of running the (stand-in) proof checker on a tactic vector.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CheckResult {
    /// Whether the checker accepted the proof (the binary kernel verdict).
    pub valid: bool,
    /// Proof size (tactic-step proxy). Only meaningful when `valid`.
    pub size: f64,
    /// Distance of the tactic vector from the correct-proof target.
    pub distance: f64,
}

/// Euclidean distance of a tactic vector from the correct-proof [`TARGET`]. Tactic
/// vectors shorter than [`TACTIC_DIM`] are padded with zeros (a missing tactic knob
/// reads as the origin); extra coordinates are ignored.
#[must_use]
pub fn tactic_distance(params: &[f64]) -> f64 {
    let mut sse = 0.0;
    for (i, t) in TARGET.iter().enumerate() {
        let p = params.get(i).copied().unwrap_or(0.0);
        sse += (p - t).powi(2);
    }
    sse.sqrt()
}

/// Run the deterministic proof checker on a tactic vector for one split.
///
/// `Dev` accepts within [`DEV_RADIUS`]; `HeldOut` accepts only within the stricter
/// [`HELDOUT_RADIUS`]. A non-finite coordinate is an ill-formed proof and is rejected
/// (fail-closed). Size is the off-centre tactic-step cost; it is only read when the
/// proof is accepted.
#[must_use]
pub fn check(params: &[f64], split: Split) -> CheckResult {
    if params.iter().any(|p| !p.is_finite()) {
        return CheckResult {
            valid: false,
            size: f64::INFINITY,
            distance: f64::INFINITY,
        };
    }
    let distance = tactic_distance(params);
    let radius = match split {
        Split::Dev => DEV_RADIUS,
        Split::HeldOut => HELDOUT_RADIUS,
    };
    let valid = distance <= radius;
    let size = SIZE_FLOOR + SIZE_PER_DIST * distance;
    CheckResult {
        valid,
        size,
        distance,
    }
}

/// The score a single check yields *before* measurement noise: `VALID_BONUS - size`
/// for an accepted proof, [`INVALID_VALUE`] for a rejected one. Higher is better.
#[must_use]
pub fn proof_value(result: &CheckResult) -> f64 {
    if result.valid {
        VALID_BONUS - result.size
    } else {
        INVALID_VALUE
    }
}

// --- Scorer (the Referee's held-out proof check) ----------------------------

/// Re-checks a candidate proof on a data split by running the checker `checks` times
/// (a repeated-verification proxy that yields a real CI) and reporting the mean
/// score. `value = VALID_BONUS - size` for an accepted proof and [`INVALID_VALUE`]
/// for a rejected one, so higher is better and the orchestrator computes a positive
/// lift only for a *valid, shorter* proof.
///
/// On [`Split::HeldOut`] the stricter held-out checker is used; on [`Split::Dev`] the
/// looser dev checker is used — so a proof that is only *barely* dev-valid looks good
/// on the dev signal a researcher sees and still gets rejected by the Referee's
/// held-out check. The proof's content-addressed identity (its tactic vector) seeds
/// the deterministic check noise, so the same proof always re-scores the same.
#[derive(Clone, Copy, Debug)]
pub struct ProofScorer {
    checks: u32,
}

impl ProofScorer {
    /// `checks` should be at least the gate's `min_n` (12) for the result to be
    /// admissible. It is the number of repeated checker runs behind the CI.
    #[must_use]
    pub fn new(checks: u32) -> Self {
        Self { checks }
    }

    /// A deterministic seed for the check noise, derived from the tactic vector's
    /// bits so the same proof always produces the same measurement (content-addressed
    /// check, no clock, no `rand`).
    fn proof_seed(params: &[f64]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for p in params {
            for byte in p.to_bits().to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
            }
        }
        hash
    }

    /// Synchronous scoring core, exposed so sync callers can re-check a proof without
    /// driving the always-ready [`Scorer::score`] future. Returns the mean score over
    /// `checks` repeated verifications with a normal 95% CI.
    #[must_use]
    pub fn measure(&self, artifact: &GenericArtifact, split: Split) -> Measurement {
        let result = check(&artifact.params, split);
        let base_value = proof_value(&result);
        // A rejected proof's noise std is zero: rejection is unambiguous, so its CI is
        // a point. A valid proof gets a small per-check perturbation (tactic-ordering
        // proxy) that gives the score a real, non-degenerate interval.
        let noise = if result.valid { CHECK_NOISE } else { 0.0 };

        let split_word: u64 = match split {
            Split::Dev => 0x0000_0000_0000_D0D0,
            Split::HeldOut => 0x0000_0000_0000_4E1D,
        };
        let seed = Self::proof_seed(&artifact.params);

        let n = self.checks.max(1);
        let mut samples = Vec::with_capacity(n as usize);
        for run in 0..n {
            let mix = seed.wrapping_mul(0x100_0000_01B3)
                ^ split_word
                ^ (u64::from(run).wrapping_mul(0x9E37_79B1));
            samples.push(base_value + noise * jitter(mix));
        }

        let nf = f64::from(n);
        let mean = samples.iter().sum::<f64>() / nf;
        let var = if n > 1 {
            samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (nf - 1.0)
        } else {
            0.0
        };
        let se = (var / nf).sqrt();
        let half = Z_95 * se;
        Measurement {
            value: mean,
            ci_lower: mean - half,
            ci_upper: mean + half,
            n,
            // Cheapest verify: a binary checker run per repeat (one cost unit each).
            cost: nf,
        }
    }
}

impl Scorer for ProofScorer {
    type Artifact = GenericArtifact;

    fn id(&self) -> &str {
        "theorem-proving-heldout"
    }

    fn score(
        &self,
        artifact: &Self::Artifact,
        split: Split,
    ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send {
        let m = self.measure(artifact, split);
        std::future::ready(Ok(m))
    }
}

// --- Helpers for constructing proofs ----------------------------------------

/// The baseline proof the market measures lift against: a *long but valid* proof —
/// the naive `simp`-everything script that the checker accepts (it is inside the
/// held-out region) but that spends many tactic steps. A real improvement must stay
/// valid AND get strictly shorter to certify lift.
///
/// It sits at distance `0.9` from the target along the first tactic axis, inside the
/// held-out radius (`1.0`), so it is valid on both splits but far from minimal size.
#[must_use]
pub fn baseline_proof() -> GenericArtifact {
    let mut params = TARGET.to_vec();
    params[0] += 0.9; // off-centre but still inside HELDOUT_RADIUS
    GenericArtifact::new(
        ArtifactKind::Proof,
        params,
        "baseline: naive long-but-valid tactic script",
    )
}

/// A proof artifact at an explicit tactic vector, for tests and researcher start
/// points. `content` carries a human-readable description for provenance.
#[must_use]
pub fn proof_at(params: Vec<f64>, content: impl Into<String>) -> GenericArtifact {
    GenericArtifact::new(ArtifactKind::Proof, params, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centred_proof_is_valid_and_short_on_both_splits() {
        let centred = TARGET.to_vec();
        let dev = check(&centred, Split::Dev);
        let held = check(&centred, Split::HeldOut);
        assert!(
            dev.valid && held.valid,
            "the correct proof must be accepted"
        );
        // A centred proof spends only the size floor.
        assert!((dev.size - SIZE_FLOOR).abs() < 1e-9);
    }

    #[test]
    fn far_proof_is_invalid_on_both_splits() {
        let far = [10.0, 10.0, 10.0, 10.0];
        assert!(
            !check(&far, Split::Dev).valid,
            "a far proof is rejected on dev"
        );
        assert!(
            !check(&far, Split::HeldOut).valid,
            "a far proof is rejected on held-out"
        );
    }

    #[test]
    fn annulus_proof_is_dev_valid_but_heldout_invalid() {
        // A tactic vector in (HELDOUT_RADIUS, DEV_RADIUS] is the generalization gap:
        // accepted by the dev checker, rejected by the stricter held-out checker.
        let mut p = TARGET.to_vec();
        p[0] += 1.25; // distance 1.25 in (1.0, 1.5]
        let d = tactic_distance(&p);
        assert!(
            d > HELDOUT_RADIUS && d <= DEV_RADIUS,
            "must sit in the annulus"
        );
        assert!(
            check(&p, Split::Dev).valid,
            "dev checker accepts the annulus"
        );
        assert!(
            !check(&p, Split::HeldOut).valid,
            "held-out checker rejects the annulus — the gate's teeth"
        );
    }

    #[test]
    fn invalid_proof_scores_far_below_any_valid_proof() {
        let invalid = proof_value(&check(&[9.0, 9.0, 9.0, 9.0], Split::HeldOut));
        let worst_valid = proof_value(&CheckResult {
            valid: true,
            size: VALID_BONUS, // an absurdly long but valid proof
            distance: 0.0,
        });
        assert!(
            invalid < worst_valid,
            "an invalid proof must score below even the longest valid proof: {invalid} < {worst_valid}"
        );
    }

    #[test]
    fn shorter_valid_proof_scores_higher() {
        let scorer = ProofScorer::new(16);
        let centred = proof_at(TARGET.to_vec(), "short");
        let off = {
            let mut p = TARGET.to_vec();
            p[0] += 0.8;
            proof_at(p, "longer")
        };
        let short_v = scorer.measure(&centred, Split::HeldOut).value;
        let long_v = scorer.measure(&off, Split::HeldOut).value;
        assert!(
            short_v > long_v,
            "a shorter valid proof must score higher: {short_v} > {long_v}"
        );
    }

    #[test]
    fn valid_proof_has_a_nondegenerate_ci_invalid_is_a_point() {
        let scorer = ProofScorer::new(16);
        let valid = scorer.measure(&proof_at(TARGET.to_vec(), "v"), Split::HeldOut);
        let invalid = scorer.measure(&proof_at(vec![9.0; TACTIC_DIM], "i"), Split::HeldOut);
        assert!(
            valid.ci_upper > valid.ci_lower,
            "a valid proof's repeated check must yield a real interval"
        );
        assert!(
            (invalid.ci_upper - invalid.ci_lower).abs() < 1e-9,
            "rejection is unambiguous — its CI is a point"
        );
    }

    #[test]
    fn measurement_is_deterministic() {
        let scorer = ProofScorer::new(20);
        let p = proof_at(vec![0.7, -0.6, 0.3, 1.8], "p");
        let a = scorer.measure(&p, Split::HeldOut);
        let b = scorer.measure(&p, Split::HeldOut);
        assert_eq!(a, b, "the same proof must re-check to the same measurement");
    }

    #[test]
    fn baseline_is_valid_but_not_minimal() {
        let scorer = ProofScorer::new(16);
        let base = baseline_proof();
        let held = check(&base.params, Split::HeldOut);
        assert!(held.valid, "the baseline proof must itself be accepted");
        // A centred (shorter) proof must beat the baseline so there is lift to win.
        let centred_v = scorer
            .measure(&proof_at(TARGET.to_vec(), "c"), Split::HeldOut)
            .value;
        let base_v = scorer.measure(&base, Split::HeldOut).value;
        assert!(
            centred_v > base_v,
            "a shorter valid proof must beat the long baseline: {centred_v} > {base_v}"
        );
    }

    #[test]
    fn nonfinite_params_are_rejected() {
        assert!(!check(&[f64::NAN, 0.0, 0.0, 0.0], Split::Dev).valid);
        assert!(!check(&[f64::INFINITY, 0.0, 0.0, 0.0], Split::HeldOut).valid);
    }
}
