//! The collaborative runner — the *other half* of the four-knob model.
//!
//! Where the competitive runners ([`crate::orchestrator`], [`crate::private`]) rank
//! **separate** candidate artifacts and pay the top-k, the collaborative runner pools
//! many contributors onto **ONE shared artifact** and pays each by their
//! **contribution share** (`Σ share_bps ≤ 10,000`). This is the
//! `Structure::Collaborative` path — the "train a 70B no single node can hold" escape
//! hatch (`docs/MECHANISM.md §6`): contributors each produce a *delta* to the shared
//! artifact, the deltas are folded in via [`Surface::apply_delta`], and the resulting
//! shared artifact is scored on the held-out split.
//!
//! # The improvement over the training-blueprint baseline (MECHANISM §6)
//!
//! The training-blueprint's DeMo engine measures contribution as **GPU-minutes** and
//! verifies it **statistically only** (TOPLOC hash + gradient-norm outliers); there is
//! **no held-out gating and no auto-slash**. That is gameable: a collusion ring can
//! report plausible GPU-minutes for low-value or redundant work and split the pool,
//! and burning compute is paid even when the shared artifact's held-out score does not
//! move — which violates the project thesis (**pay for outcome, not effort**) at
//! exactly the mode where effort is the unit (§6.1).
//!
//! This runner implements the two host-independent layers of the §6.2 fix:
//!
//! 1. **Held-out-eval-gated payout (§6.2 layer 1).** The *pool* is tied to certified
//!    held-out improvement of the final shared artifact: if the final artifact does
//!    not clear the competition [`Gate`], **nobody is paid** — effort is rewarded only
//!    when it produced a certified outcome. This alone removes the "burn compute for
//!    pay even if the model didn't improve" failure.
//!
//! 2. **Single-permutation marginal-contribution credit (§6.2 layer 2).** Instead of
//!    raw GPU-minutes, each contributor is weighted by the **marginal effect of their
//!    delta on the held-out score** — the held-out lift the shared artifact *gained*
//!    when that contributor's delta was folded in. A contributor whose delta does not
//!    move held-out (a free-rider / dead-gradient) gets **near-zero** credit even if
//!    they "burned" equal compute. [`attribute_shares`] turns these marginals into
//!    basis-point shares (clamped `≥ 0`, summing to `≤ 10,000`).
//!
//!    **This is a single-permutation marginal (first-difference) estimator, NOT a true
//!    Shapley value.** Each contributor's marginal is the held-out lift gained when
//!    their delta is folded in *at their position in the fold order* — a sequential
//!    leave-one-out over ONE permutation. A true Shapley value averages this marginal
//!    over ALL permutations precisely to remove order-dependence; this runner evaluates
//!    exactly one. The two coincide (credit == true contribution) **only when
//!    contributors' deltas are mutually orthogonal / non-overlapping** — each moving a
//!    disjoint part of the held-out score. When deltas overlap (as real
//!    model-merging / training deltas do, via diminishing returns on a saturating
//!    surface), credit becomes order-dependent: a delta folded into an early slot, when
//!    the running score still has headroom, books a larger marginal than the *same*
//!    delta folded into a late slot where earlier deltas already consumed the headroom.
//!    Two fully substitutable contributors can therefore see a large payout swing
//!    decided solely by fold order.
//!
//!    To deny a caller the ability to *choose* the over-crediting order, this runner
//!    does **not** fold contributors in caller-supplied order: it folds them in a
//!    canonical order derived from a content hash of each contributor's `delta_ref`
//!    (see [`run_collaborative`]), so the fold order is a deterministic function of the
//!    deltas themselves, not of list position. This makes credit reproducible and
//!    un-gameable-by-reordering; it does **not** make overlapping-delta credit
//!    order-*independent* (only a full/sampled Shapley average would). The §6.2 doc
//!    names a "checkpoint-difference estimator (proposed)"; this is its
//!    single-permutation form with a canonical permutation.
//!
//! The Validator spot-check (§6.2 layer 3) and stake-weighted SLA (§6.2 layer 4) are
//! the slashing layers; they reuse the existing dispute/stake spine and are not part
//! of this runner.
//!
//! # Honest seam (no faked GPU cluster)
//!
//! The real distributed-training integration — the **training-blueprint / DeMo
//! (Decoupled Momentum) engine** running across a GPU cluster — is a **SEAM**, exactly
//! like the sandbox-runtime and Improvement-Plane seams elsewhere. There is no real
//! GPU cluster here. The contributor that produces each delta is supplied by the
//! caller (`make_contributor`); the verticals ship a **local deterministic stand-in**
//! ([`autoresearch_verticals::SharedSearchContributor`]) that improves the shared
//! [`ConfigArtifact`](autoresearch_verticals::ConfigArtifact) toward the ground truth
//! by a seeded local step. The *mechanism* (held-out gating + single-permutation
//! marginal credit + conserving settlement) is real and fully tested; the training
//! engine behind a delta is the marked stand-in. We do not claim a real cluster.

use autoresearch_runtime::reward::{BPS_DENOM, Payout, settle_snapshot_topk};
use autoresearch_runtime::traits::{Engine, EngineContext, Scorer, Surface};
use autoresearch_runtime::types::{ArtifactRef, Lift, Split, Structure};

use crate::lift::estimate_lift;
use crate::orchestrator::{CompetitionConfig, ProtocolError};

/// One contributor to the shared artifact. `delta_ref` is the (sealed/content)
/// reference to the delta the contributor produced; the concrete delta artifact is
/// produced by the caller's `make_contributor` engine and folded into the running
/// shared artifact via [`Surface::apply_delta`]. For the local stand-in this is a
/// `ConfigArtifact` delta; for the production DeMo seam it is a model-update delta.
#[derive(Clone, Debug)]
pub struct Contribution {
    /// The contributor (a GPU pool, in the production framing).
    pub contributor: String,
    /// Seed handed to the contributor's engine — distinct seeds, distinct deltas, so
    /// contributors add different real marginal value (no tie).
    pub seed: u64,
    /// Sealed/content reference to this contributor's delta. Recorded for provenance;
    /// the concrete delta is what `apply_delta` folds in.
    pub delta_ref: ArtifactRef,
}

/// The result of a collaborative run.
#[derive(Clone, Debug)]
pub struct CollaborativeOutcome {
    /// The certified held-out lift of the FINAL shared artifact over the baseline.
    /// This is the real measured improvement; if it does not clear the gate, no one
    /// is paid (held-out-gated — §6.2 layer 1).
    pub final_artifact_lift: Lift,
    /// Per-contributor basis-point shares of the pool, summing to `≤ 10,000`. A
    /// contributor with zero/negative marginal lift gets `0` (free-rider → zero).
    pub shares: Vec<(String, u32)>,
    /// Settled payouts (pool × share_bps, floored, conserving the pool exactly as the
    /// competitive runners do). Empty if the final artifact did not clear the gate.
    pub payouts: Vec<Payout>,
    /// Count of contributors whose delta was ACCEPTED (improved the running held-out
    /// score and was folded into the shared artifact). Regressions are rejected.
    pub accepted: usize,
}

/// Turn per-contributor marginal held-out lift into basis-point pool shares.
///
/// This is the single-permutation marginal-credit scheme of `docs/MECHANISM.md §6.2`
/// (layer 2): weight each contributor by their marginal effect on the held-out score,
/// not by raw GPU-minutes. Each marginal is **clamped to `≥ 0`** (a non-positive
/// marginal — a free-rider / dead-gradient / regression — earns nothing), then shares
/// are allocated proportionally to the clamped marginals and summed in basis points.
///
/// Properties (tested):
/// - a contributor with a zero/negative marginal gets **exactly 0 bps** (free-rider
///   → zero, the key improvement over GPU-minutes);
/// - shares sum to **`≤ 10,000`** bps (integer flooring drops dust, never mints);
/// - shares are proportional to the positive marginals (more held-out lift → more
///   share), so the credit is monotone in useful contribution.
///
/// **Order-dependence (important — not a Shapley value).** The marginals this function
/// is fed are *single-permutation* first-differences computed by [`run_collaborative`]
/// as the running held-out lift is folded forward. They equal each contributor's true
/// (order-independent) contribution **only when the deltas are mutually orthogonal /
/// non-overlapping**. When deltas overlap — the realistic case for model-merging /
/// training deltas, which saturate — the marginal a contributor books depends on the
/// fold order, so the resulting shares are order-dependent. [`run_collaborative`]
/// folds in a canonical (delta-content-hashed) order so the order is not caller-chosen,
/// but it does NOT make overlapping-delta shares order-*independent*; only averaging
/// over many permutations (a sampled Shapley value) would. Treat these shares as a
/// reproducible single-permutation estimate, not a permutation-invariant Shapley value.
///
/// Non-finite marginals (`NaN`/`inf`, e.g. from an adversarial scorer) are treated as
/// zero — fail-closed. If the total positive marginal is zero (everyone a free-rider),
/// every share is 0.
#[must_use]
pub fn attribute_shares(marginals: &[(String, f64)]) -> Vec<(String, u32)> {
    // Clamp each marginal to >= 0 and drop non-finite ones (fail-closed).
    let clamped: Vec<f64> = marginals
        .iter()
        .map(|(_, m)| if m.is_finite() && *m > 0.0 { *m } else { 0.0 })
        .collect();
    // `total` is a sum of finite, non-negative clamped marginals, so it is itself
    // finite and `>= 0`: `total <= 0.0` is exactly "no useful contribution" with no
    // NaN ambiguity (the non-finite inputs were already mapped to 0 above).
    let total: f64 = clamped.iter().sum();
    if total <= 0.0 {
        // No useful contribution at all → no shares.
        return marginals.iter().map(|(c, _)| (c.clone(), 0u32)).collect();
    }
    // Allocate basis points proportionally, flooring each so the sum never exceeds
    // BPS_DENOM (dust is dropped, never minted — same discipline as settle_snapshot_topk).
    marginals
        .iter()
        .zip(&clamped)
        .map(|((contributor, _), &m)| {
            let bps = (m / total * f64::from(BPS_DENOM)).floor();
            // `bps` is finite and in [0, BPS_DENOM]; cast is safe.
            let bps = bps.clamp(0.0, f64::from(BPS_DENOM)) as u32;
            (contributor.clone(), bps)
        })
        .collect()
}

/// Run a `Collaborative × OneShot` competition: pool contributors onto one shared
/// artifact, attribute by held-out-gated marginal contribution, and settle conserving
/// payouts.
///
/// Process (the §6 mechanism):
/// 1. Score the `baseline` once on [`Split::HeldOut`] — the bar the shared artifact is
///    measured against and the starting running score.
/// 2. **Canonicalize the fold order.** Contributors are folded in **deterministic
///    content order** — sorted by a hash of each contributor's `delta_ref` (ties broken
///    by `delta_ref` then contributor id) — **not** in caller-supplied slice order.
///    This is the order-gaming defense: the single-permutation marginal below is
///    order-dependent when deltas overlap (see [`attribute_shares`]), so the order is
///    made a function of the deltas themselves, denying an orchestrator the ability to
///    front-load a chosen contributor into a high-credit slot. The order is stable and
///    reproducible across instances; it does NOT remove overlapping-delta
///    order-dependence (only a sampled Shapley average would).
/// 3. For each contributor in that canonical order: build their engine, let it
///    `produce` a delta, validate the delta on the surface, fold it into the running
///    shared artifact via [`Surface::apply_delta`], and score the new shared artifact
///    on held-out.
///    - The contributor's **marginal contribution** is the *increase* in held-out lift
///      its delta caused (`new_lift.delta - prev_lift.delta`) — a single-permutation
///      first-difference, exact only under orthogonal deltas.
///    - A delta that does **not** improve the running held-out score is **rejected**
///      (the shared artifact is rolled back to before the delta) and its marginal is
///      `0` — a regression never helps and never earns.
/// 4. Estimate the FINAL shared artifact's lift over the baseline.
/// 5. **Held-out gate (load-bearing):** if the final shared artifact does not clear
///    the competition [`Gate`], return zero shares and **no payouts** — nobody is paid.
/// 6. Otherwise [`attribute_shares`] over the per-contributor marginals and settle
///    `pool × share_bps` (conserving, via [`settle_snapshot_topk`]'s flooring).
///
/// `make_contributor` builds a fresh engine per [`Contribution`] (the DeMo-seam
/// injection point). The engine emits a **delta** artifact (not a full replacement);
/// `apply_delta` is the merge step.
///
/// The returned `shares` / `payouts` preserve the **caller's** contributor order, not
/// the internal canonical fold order, so callers see a stable mapping back to the input
/// slice regardless of how the deltas hash.
///
/// # Errors
/// - [`ProtocolError::IncoherentKnobs`] if the knobs are not a coherent
///   `Collaborative × OneShot` (e.g. `Collaborative × Continuous`, which
///   [`Knobs::validate`] rejects), or if the structure is not `Collaborative`.
/// - [`ProtocolError::InvalidReward`] if the reward schedule cannot conserve the pool.
/// - [`ProtocolError::Surface`] / [`ProtocolError::Scorer`] / [`ProtocolError::Engine`]
///   from the underlying produce/validate/score path.
pub async fn run_collaborative<S, Sc, Eng, Mk>(
    cfg: &CompetitionConfig,
    surface: &S,
    scorer: &Sc,
    baseline: &S::Artifact,
    contributors: &[Contribution],
    make_contributor: Mk,
) -> Result<CollaborativeOutcome, ProtocolError>
where
    S: Surface,
    // The shared artifact is folded across contributors and a rejected delta is rolled
    // back, so the running shared artifact must be cloneable. Every surface in this
    // project has a `Clone` artifact (it is a small config/weight vector).
    S::Artifact: Clone,
    Sc: Scorer<Artifact = S::Artifact>,
    Eng: Engine<Artifact = S::Artifact>,
    Mk: Fn(&Contribution) -> Eng,
{
    cfg.knobs
        .validate()
        .map_err(ProtocolError::IncoherentKnobs)?;
    // This runner is the Collaborative path; reject a Competitive config routed here
    // rather than silently treating separate submissions as one shared artifact.
    if cfg.knobs.structure != Structure::Collaborative {
        return Err(ProtocolError::IncoherentKnobs(
            "run_collaborative requires Structure::Collaborative",
        ));
    }
    // Reject a non-conserving reward schedule up front (the pool, when it exists,
    // must never overpay) — same guard the competitive runners apply.
    cfg.reward.validate()?;

    // 1. The baseline bar and the starting shared artifact.
    let baseline_ref = surface.to_ref(baseline)?;
    let baseline_measurement = scorer.score(baseline, Split::HeldOut).await?;

    // The running shared artifact starts at the baseline. Holding it owned lets us
    // accept a delta (fold it in for good) or implicitly roll back a rejected one (the
    // candidate fold is simply discarded, leaving `shared` untouched).
    let mut shared = baseline.clone();
    // The running held-out lift of the shared artifact over the baseline (starts at 0).
    let mut prev_lift_delta = 0.0_f64;
    let mut last_lift = estimate_lift(&baseline_measurement, &baseline_measurement);

    // Per-contributor marginal, indexed by the contributor's position in the CALLER's
    // slice so the returned shares preserve caller order even though we fold in a
    // canonical (content-hashed) order below.
    let mut marginal_by_caller_idx: Vec<f64> = vec![0.0; contributors.len()];
    let mut accepted = 0usize;

    // 2. Canonical fold order — the order-gaming defense. We do NOT trust the caller's
    //    slice order (an orchestrator could front-load a chosen contributor into a
    //    high-credit slot when deltas overlap). Instead fold in an order derived from a
    //    content hash of each contributor's `delta_ref`, with deterministic tie-breaks,
    //    so the order is a stable function of the deltas, not of list position.
    let mut fold_order: Vec<usize> = (0..contributors.len()).collect();
    fold_order.sort_by(|&a, &b| {
        canonical_fold_key(&contributors[a]).cmp(&canonical_fold_key(&contributors[b]))
    });

    // 3. Fold each contributor's delta in (canonical order), accept only held-out
    //    improvements.
    for &idx in &fold_order {
        let contribution = &contributors[idx];
        let engine = make_contributor(contribution);
        let ctx = EngineContext {
            competition: cfg.id,
            baseline_ref: baseline_ref.clone(),
            // Collaborative contributors hill-climb the shared artifact on dev signal.
            dev_split_ref: Some(ArtifactRef(format!("dev-split:{}", cfg.id))),
            budget_wei: cfg.reward_pool_wei,
            egress_policy: None,
        };

        let delta = engine
            .produce(&ctx)
            .await
            .map_err(|source| ProtocolError::Engine {
                researcher: contribution.contributor.clone(),
                source,
            })?;
        surface.validate(&delta)?;

        // Fold the delta into the running shared artifact (the merge step).
        let candidate_shared = surface.apply_delta(&shared, &delta)?;
        let candidate_measurement = scorer.score(&candidate_shared, Split::HeldOut).await?;
        let candidate_lift = estimate_lift(&candidate_measurement, &baseline_measurement);

        // Marginal = how much this delta moved the shared artifact's held-out lift.
        let marginal = candidate_lift.delta - prev_lift_delta;

        if marginal > 0.0 {
            // Accept: the delta improved held-out. Fold it in for good.
            shared = candidate_shared;
            prev_lift_delta = candidate_lift.delta;
            last_lift = candidate_lift;
            accepted += 1;
            marginal_by_caller_idx[idx] = marginal;
        } else {
            // Reject: a non-improving / regressing delta is rolled back (shared is
            // unchanged) and earns nothing. A free-rider's zero/negative delta lands
            // here → zero marginal → zero share. (`marginal_by_caller_idx[idx]` stays 0.)
        }
    }

    // Rebuild the marginal table in CALLER order so the returned shares map back to the
    // caller's input slice, independent of the canonical fold order used above.
    let marginals: Vec<(String, f64)> = contributors
        .iter()
        .zip(&marginal_by_caller_idx)
        .map(|(c, &m)| (c.contributor.clone(), m))
        .collect();

    // 4-5. The final shared-artifact lift, and the load-bearing held-out gate.
    let final_lift = last_lift;
    let final_shared_measurement = scorer.score(&shared, Split::HeldOut).await?;
    let cleared = cfg.gate.clears(&final_lift, &final_shared_measurement);

    if !cleared {
        // Held-out gate bites: the shared artifact did not clear the gate, so the pool
        // does not exist — NOBODY is paid (§6.2 layer 1). Shares are still reported
        // (all credited proportionally) for transparency, but payouts are empty.
        let shares = attribute_shares(&marginals);
        return Ok(CollaborativeOutcome {
            final_artifact_lift: final_lift,
            shares,
            payouts: Vec::new(),
            accepted,
        });
    }

    // 6. Attribute and settle. Shares are single-permutation marginal credit (canonical
    //    fold order); settlement floors each share against the pool so the total can
    //    never exceed the escrow.
    let shares = attribute_shares(&marginals);
    let payouts = settle_shares(cfg.reward_pool_wei, &shares);

    Ok(CollaborativeOutcome {
        final_artifact_lift: final_lift,
        shares,
        payouts,
        accepted,
    })
}

/// Deterministic, content-derived sort key for a contributor's fold position.
///
/// The primary key is a stable 64-bit FNV-1a hash of the contributor's `delta_ref`
/// content — chosen over [`std::hash::DefaultHasher`] because FNV-1a is fixed across
/// Rust versions and machines, so the canonical fold order is reproducible across
/// instances (the cross-instance determinism the runner claims). Ties on the hash are
/// broken by the `delta_ref` string and then the contributor id, so the order is a
/// total order that never depends on the caller's slice position. This is what denies a
/// caller the ability to pick the fold order that over-credits a chosen contributor.
fn canonical_fold_key(c: &Contribution) -> (u64, &str, &str) {
    (
        fnv1a_64(c.delta_ref.0.as_bytes()),
        &c.delta_ref.0,
        &c.contributor,
    )
}

/// Stable FNV-1a 64-bit hash. Fixed constants → identical output on every machine and
/// Rust version, so the canonical fold order it induces is reproducible everywhere.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Settle per-contributor basis-point shares into conserving payouts.
///
/// Reuses [`settle_snapshot_topk`]'s exact flooring + running-remainder clamp so the
/// total paid can never exceed `pool_wei` (dust is dropped, never minted) — the same
/// conservation primitive the competitive top-k settlement uses. Zero-share
/// contributors (free-riders) are dropped (no zero-wei payouts).
fn settle_shares(pool_wei: u128, shares: &[(String, u32)]) -> Vec<Payout> {
    let ids: Vec<String> = shares.iter().map(|(c, _)| c.clone()).collect();
    let weights: Vec<u32> = shares.iter().map(|(_, w)| *w).collect();
    settle_snapshot_topk(pool_wei, &ids, &weights)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autoresearch_runtime::reward::{BPS_DENOM, RewardSchedule, total_wei};
    use autoresearch_runtime::types::{Cadence, Gate, Knobs, ScorerKind, Visibility};

    fn collab_knobs() -> Knobs {
        Knobs {
            structure: Structure::Collaborative,
            cadence: Cadence::OneShot,
            visibility: Visibility::Public,
            scorer_kind: ScorerKind::HeldOutEval,
        }
    }

    // --- attribute_shares: the fairness core ------------------------------

    #[test]
    fn free_rider_with_zero_marginal_gets_zero_share() {
        let marginals = vec![
            ("0xa".to_string(), 0.040),
            ("0xb".to_string(), 0.038),
            ("0xfreerider".to_string(), 0.0),
        ];
        let shares = attribute_shares(&marginals);
        let free = shares.iter().find(|(c, _)| c == "0xfreerider").unwrap();
        assert_eq!(free.1, 0, "a zero-marginal free-rider must earn 0 bps");
        // The productive two split (almost) the whole pool proportionally.
        let a = shares.iter().find(|(c, _)| c == "0xa").unwrap().1;
        let b = shares.iter().find(|(c, _)| c == "0xb").unwrap().1;
        assert!(a > b, "more marginal lift => more share");
    }

    #[test]
    fn negative_marginal_is_clamped_to_zero() {
        let marginals = vec![("0xa".to_string(), 0.10), ("0xregress".to_string(), -0.05)];
        let shares = attribute_shares(&marginals);
        assert_eq!(
            shares.iter().find(|(c, _)| c == "0xregress").unwrap().1,
            0,
            "a negative (regressing) marginal earns 0"
        );
        // The sole productive contributor takes (almost) the whole pool.
        assert!(shares.iter().find(|(c, _)| c == "0xa").unwrap().1 >= 9_999);
    }

    #[test]
    fn shares_sum_to_at_most_bps_denom() {
        // A pathological many-way split must still never exceed 10_000 bps.
        let marginals: Vec<(String, f64)> = (0..7)
            .map(|i| (format!("0x{i}"), 0.01 * f64::from(i + 1)))
            .collect();
        let shares = attribute_shares(&marginals);
        let sum: u32 = shares.iter().map(|(_, w)| *w).sum();
        assert!(
            sum <= BPS_DENOM,
            "shares sum {sum} exceeded {BPS_DENOM} bps"
        );
    }

    #[test]
    fn shares_are_proportional_to_marginals() {
        // The MECHANISM §6.2 worked example: 0.040 / 0.038 / 0.002.
        let marginals = vec![
            ("A".to_string(), 0.040),
            ("B".to_string(), 0.038),
            ("C".to_string(), 0.002),
        ];
        let shares = attribute_shares(&marginals);
        let a = shares.iter().find(|(c, _)| c == "A").unwrap().1;
        let b = shares.iter().find(|(c, _)| c == "B").unwrap().1;
        let c = shares.iter().find(|(c, _)| c == "C").unwrap().1;
        // total = 0.080; A=5000, B=4750, C=250 (flooring may shave a bp).
        assert!((4990..=5000).contains(&a), "A ~= 5000 bps, got {a}");
        assert!((4740..=4750).contains(&b), "B ~= 4750 bps, got {b}");
        assert!((240..=250).contains(&c), "C ~= 250 bps, got {c}");
        // C burned compute comparable to A/B's coalition but earns ~250, not ~3333.
        assert!(
            c < a / 10,
            "dead-gradient C earns far less than productive A"
        );
    }

    #[test]
    fn all_free_riders_get_zero() {
        let marginals = vec![("0xa".to_string(), 0.0), ("0xb".to_string(), -1.0)];
        let shares = attribute_shares(&marginals);
        assert!(shares.iter().all(|(_, w)| *w == 0));
    }

    #[test]
    fn settle_shares_conserves_the_pool() {
        let pool = 1_000_000u128;
        let shares = vec![
            ("A".to_string(), 5_000u32),
            ("B".to_string(), 3_000),
            ("C".to_string(), 2_000),
        ];
        let payouts = settle_shares(pool, &shares);
        assert!(
            total_wei(&payouts) <= pool,
            "settlement must conserve the pool"
        );
        assert_eq!(total_wei(&payouts), pool, "full shares distribute the pool");
        // Zero-share contributors produce no payout entry.
        let with_zero = vec![("A".to_string(), 10_000u32), ("Z".to_string(), 0u32)];
        let payouts = settle_shares(pool, &with_zero);
        assert_eq!(payouts.len(), 1, "a zero-share free-rider gets no payout");
        assert_eq!(payouts[0].researcher, "A");
    }

    // --- collaborative_knobs coherence ------------------------------------

    #[test]
    fn collaborative_continuous_is_incoherent() {
        let knobs = Knobs {
            structure: Structure::Collaborative,
            cadence: Cadence::Continuous,
            visibility: Visibility::Public,
            scorer_kind: ScorerKind::HeldOutEval,
        };
        assert!(
            knobs.validate().is_err(),
            "Collaborative+Continuous is rejected"
        );
        // The coherent collaborative mode is OneShot.
        assert!(collab_knobs().validate().is_ok());
    }

    // A trivial deterministic surface/scorer/engine to unit-test the runner end-to-end
    // without the full vertical. A "shared artifact" is a single score in [0, 1]; a
    // "delta" is an additive increment to it. Reused in spirit from the private runner's
    // ScalarSurface but additive so apply_delta is the real merge.
    #[derive(Clone)]
    struct ScalarArtifact(f64);

    struct ScalarSurface;
    impl Surface for ScalarSurface {
        type Artifact = ScalarArtifact;
        fn id(&self) -> &str {
            "scalar"
        }
        fn validate(
            &self,
            a: &Self::Artifact,
        ) -> Result<(), autoresearch_runtime::traits::SurfaceError> {
            if a.0.is_finite() {
                Ok(())
            } else {
                Err(autoresearch_runtime::traits::SurfaceError::Invalid(
                    "non-finite".into(),
                ))
            }
        }
        fn apply_delta(
            &self,
            base: &Self::Artifact,
            delta: &Self::Artifact,
        ) -> Result<Self::Artifact, autoresearch_runtime::traits::SurfaceError> {
            // Additive merge — the real apply_delta the runner folds deltas through.
            Ok(ScalarArtifact((base.0 + delta.0).clamp(0.0, 1.0)))
        }
        fn to_ref(
            &self,
            a: &Self::Artifact,
        ) -> Result<ArtifactRef, autoresearch_runtime::traits::SurfaceError> {
            Ok(ArtifactRef(format!("scalar:{}", a.0)))
        }
    }

    struct ScalarScorer;
    impl Scorer for ScalarScorer {
        type Artifact = ScalarArtifact;
        fn id(&self) -> &str {
            "scalar-scorer"
        }
        fn score(
            &self,
            a: &Self::Artifact,
            _split: Split,
        ) -> impl std::future::Future<
            Output = Result<
                autoresearch_runtime::types::Measurement,
                autoresearch_runtime::traits::ScorerError,
            >,
        > + Send {
            let m = autoresearch_runtime::types::Measurement {
                value: a.0,
                ci_lower: (a.0 - 0.02).max(0.0),
                ci_upper: (a.0 + 0.02).min(1.0),
                n: 80,
                cost: 80.0,
            };
            std::future::ready(Ok(m))
        }
    }

    /// An engine that emits a fixed additive delta (the contributor's "update").
    struct DeltaEngine(f64);
    impl Engine for DeltaEngine {
        type Artifact = ScalarArtifact;
        fn id(&self) -> &str {
            "delta"
        }
        fn produce(
            &self,
            _ctx: &EngineContext,
        ) -> impl std::future::Future<
            Output = Result<Self::Artifact, autoresearch_runtime::traits::EngineError>,
        > + Send {
            std::future::ready(Ok(ScalarArtifact(self.0)))
        }
    }

    fn cfg(pool: u128) -> CompetitionConfig {
        CompetitionConfig {
            id: 1,
            gate: Gate::default(),
            reward: RewardSchedule::TerminalPrize, // unused by the collab settle path
            reward_pool_wei: pool,
            knobs: collab_knobs(),
        }
    }

    fn contribs() -> Vec<Contribution> {
        vec![
            Contribution {
                contributor: "0xbig".into(),
                seed: 1,
                delta_ref: ArtifactRef("delta:big".into()),
            },
            Contribution {
                contributor: "0xsmall".into(),
                seed: 2,
                delta_ref: ArtifactRef("delta:small".into()),
            },
            Contribution {
                contributor: "0xfreerider".into(),
                seed: 3,
                delta_ref: ArtifactRef("delta:free".into()),
            },
        ]
    }

    #[tokio::test]
    async fn productive_contributors_paid_by_marginal_freerider_zero() {
        let pool = 1_000_000u128;
        // Baseline 0.5; big adds +0.30, small adds +0.10, free-rider adds +0.0.
        let outcome = run_collaborative(
            &cfg(pool),
            &ScalarSurface,
            &ScalarScorer,
            &ScalarArtifact(0.5),
            &contribs(),
            |c| match c.contributor.as_str() {
                "0xbig" => DeltaEngine(0.30),
                "0xsmall" => DeltaEngine(0.10),
                _ => DeltaEngine(0.0),
            },
        )
        .await
        .unwrap();

        // Final shared artifact 0.5 + 0.30 + 0.10 = 0.90; lift over baseline ~0.40.
        assert!(
            outcome.final_artifact_lift.delta > 0.30,
            "shared artifact reached a real held-out lift, got {}",
            outcome.final_artifact_lift.delta
        );
        // Only the two productive deltas were accepted; the free-rider's was rejected.
        assert_eq!(outcome.accepted, 2);

        // Free-rider earns 0 bps and no payout.
        let free = outcome
            .shares
            .iter()
            .find(|(c, _)| c == "0xfreerider")
            .unwrap();
        assert_eq!(free.1, 0, "free-rider gets zero share");
        assert!(
            !outcome
                .payouts
                .iter()
                .any(|p| p.researcher == "0xfreerider"),
            "free-rider gets no payout"
        );

        // Big contributor (0.30 marginal) earns ~3x the small one (0.10 marginal).
        let big = outcome.shares.iter().find(|(c, _)| c == "0xbig").unwrap().1;
        let small = outcome
            .shares
            .iter()
            .find(|(c, _)| c == "0xsmall")
            .unwrap()
            .1;
        assert!(big > small, "bigger marginal => bigger share");
        // 0.30 / (0.30+0.10) = 7500 bps; 0.10/0.40 = 2500 bps (floor may shave a bp of
        // floating-point dust off the smaller share — never minted, always conserved).
        assert!((7_499..=7_500).contains(&big), "big ~= 7500 bps, got {big}");
        assert!(
            (2_499..=2_500).contains(&small),
            "small ~= 2500 bps, got {small}"
        );

        // Conservation: never mint more than the pool. Flooring leaves at most a
        // sub-bps residual per contributor in escrow (here ~100 wei = 1 bps of a 1M
        // pool) — dust is dropped, never minted.
        let paid = total_wei(&outcome.payouts);
        assert!(paid <= pool, "payouts must never exceed the pool");
        assert!(
            pool - paid <= 1_000,
            "only flooring dust may stay in escrow, got {paid}/{pool}"
        );
    }

    #[tokio::test]
    async fn credit_is_invariant_to_caller_order() {
        // The order-gaming defense: because the runner folds in a canonical
        // (delta-content-hashed) order rather than caller slice order, shuffling the
        // caller's input must yield IDENTICAL per-contributor shares. Here the three
        // ScalarArtifact deltas are additive and the surface does not saturate within
        // range (0.5 + 0.30 + 0.10 = 0.90 < 1.0), so the deltas are effectively
        // orthogonal and credit is exact — the canonical order makes it reproducible
        // regardless of how the caller orders the slice.
        let pool = 1_000_000u128;
        let mk = |c: &Contribution| match c.contributor.as_str() {
            "0xbig" => DeltaEngine(0.30),
            "0xsmall" => DeltaEngine(0.10),
            _ => DeltaEngine(0.0),
        };

        let forward = contribs();
        let mut reversed = contribs();
        reversed.reverse();

        let out_fwd = run_collaborative(
            &cfg(pool),
            &ScalarSurface,
            &ScalarScorer,
            &ScalarArtifact(0.5),
            &forward,
            mk,
        )
        .await
        .unwrap();
        let out_rev = run_collaborative(
            &cfg(pool),
            &ScalarSurface,
            &ScalarScorer,
            &ScalarArtifact(0.5),
            &reversed,
            mk,
        )
        .await
        .unwrap();

        // Compare shares keyed by contributor id (the slices are in different orders).
        let share_of = |out: &CollaborativeOutcome, id: &str| {
            out.shares.iter().find(|(c, _)| c == id).unwrap().1
        };
        for id in ["0xbig", "0xsmall", "0xfreerider"] {
            assert_eq!(
                share_of(&out_fwd, id),
                share_of(&out_rev, id),
                "share for {id} must not depend on caller order"
            );
        }
        // And the returned shares are keyed in CALLER order (so a caller can zip them
        // back to its input slice): forward[0] is 0xbig, reversed[0] is 0xfreerider.
        assert_eq!(out_fwd.shares[0].0, "0xbig");
        assert_eq!(out_rev.shares[0].0, "0xfreerider");
    }

    #[test]
    fn canonical_fold_key_is_independent_of_caller_position() {
        // The fold key depends only on delta_ref content + contributor id, never on
        // where the contribution sits in the caller's slice.
        let a = Contribution {
            contributor: "0xa".into(),
            seed: 1,
            delta_ref: ArtifactRef("delta:a".into()),
        };
        let b = Contribution {
            contributor: "0xb".into(),
            seed: 2,
            delta_ref: ArtifactRef("delta:b".into()),
        };
        // Same content → same key regardless of which list it came from.
        let a2 = a.clone();
        assert_eq!(canonical_fold_key(&a), canonical_fold_key(&a2));
        // Distinct deltas → distinct keys → a total fold order exists.
        assert_ne!(canonical_fold_key(&a), canonical_fold_key(&b));
    }

    #[test]
    fn fnv1a_64_is_stable_and_distinguishing() {
        // Pin the FNV-1a output so a refactor that changes the hash (and thus the
        // canonical fold order across instances) is caught.
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(fnv1a_64(b"delta:a"), fnv1a_64(b"delta:b"));
    }

    #[tokio::test]
    async fn no_payout_when_final_artifact_misses_the_gate() {
        let pool = 1_000_000u128;
        // Every contributor adds essentially nothing: the shared artifact never clears
        // the gate, so NOBODY is paid (held-out gate is load-bearing).
        let outcome = run_collaborative(
            &cfg(pool),
            &ScalarSurface,
            &ScalarScorer,
            &ScalarArtifact(0.5),
            &contribs(),
            |_| DeltaEngine(0.0),
        )
        .await
        .unwrap();
        assert_eq!(outcome.accepted, 0, "no delta improved held-out");
        assert!(
            outcome.payouts.is_empty(),
            "gate not cleared => no payouts at all"
        );
        assert_eq!(total_wei(&outcome.payouts), 0);
    }

    #[tokio::test]
    async fn competitive_config_is_rejected_by_the_collaborative_runner() {
        let mut c = cfg(1_000_000);
        c.knobs.structure = Structure::Competitive;
        let err = run_collaborative(
            &c,
            &ScalarSurface,
            &ScalarScorer,
            &ScalarArtifact(0.5),
            &contribs(),
            |_| DeltaEngine(0.30),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ProtocolError::IncoherentKnobs(_)),
            "a Competitive config must be rejected, got {err:?}"
        );
    }
}
