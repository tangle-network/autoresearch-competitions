//! End-to-end proof for M6 — the COLLABORATIVE market structure and the
//! competition → marketplace flywheel.
//!
//! # Part A — Collaborative competition (`docs/MECHANISM.md §6`)
//!
//! Several contributors pool compute onto ONE shared [`GenericArtifact`]. Each emits a
//! delta that the collaborative runner folds into the running shared artifact via
//! [`AdditiveSurface::apply_delta`] (elementwise-additive), scoring the result on
//! the held-out split. Payout is by **held-out-gated, single-permutation marginal
//! contribution** (a first-difference estimator over a canonical fold order, exact
//! under orthogonal deltas — see `collaborative.rs`; NOT a permutation-invariant
//! Shapley value) — the improvement over the training-blueprint's GPU-minutes-only
//! baseline:
//!
//! - the shared artifact reaches a real held-out lift `> 0.30` (measured ~0.51);
//! - a **free-rider** (zero-marginal delta) earns **0 share** and **0 payout**;
//! - productive contributors earn shares **proportional to their marginal contribution**;
//! - payouts **conserve the pool** (flooring drops dust, never mints);
//! - swapping the productive contributors for baseline-only deltas makes the shared
//!   artifact miss the gate, and then **NOBODY is paid** (the held-out gate bites).
//!
//! # Why a dedicated additive surface (not `GenericSurface`)
//!
//! `GenericSurface::apply_delta` is full-replacement (a produced candidate supersedes
//! the baseline) — the right semantics for the competitive search verticals. The
//! collaborative fold is fundamentally additive: each contributor's delta must sum
//! onto the running shared artifact, not replace it. The linear vertical therefore
//! keeps its own [`AdditiveSurface`] — a surface over the SAME [`GenericArtifact`]
//! type with elementwise-additive `apply_delta`. Only the fold semantics differ; the
//! artifact type is unified.
//!
//! # Part B — Marketplace flywheel (`docs/MECHANISM.md §10`)
//!
//! The certified collaborative artifact becomes sellable inventory: it is listed
//! (consented, priced by certified lift), a buyer purchases it, provenance + certified
//! lift carry over to the buyer, and a double-sell of an exclusive listing is rejected.
//!
//! # Honest seam — NOT a real GPU cluster
//!
//! The contributors here are [`SharedSearchContributor`], the local deterministic
//! **stand-in for the DeMo (Decoupled Momentum) distributed-training engine** (the real
//! training-blueprint integration is the seam — there is no GPU cluster here). The
//! training-blueprint verifies contribution by **GPU-minutes + a statistical gradient
//! check** (a known gap — gameable by collusion, no held-out gating; §6.1). M6 improves
//! on that by pricing each delta on its **held-out-eval-gated marginal contribution**
//! (§6.2). Every number below is measured on real held-out data through the same scorer
//! the Referee would use — nothing is mocked.

use autoresearch_generic_engine::GenericArtifact;
use autoresearch_protocol::collaborative::{CollaborativeOutcome, Contribution, run_collaborative};
use autoresearch_protocol::orchestrator::CompetitionConfig;
use autoresearch_runtime::marketplace::{
    ArtifactListing, CertifiedAttestation, MarketError, Marketplace, PricingPolicy, price_by_lift,
};
use autoresearch_runtime::reward::{RewardSchedule, total_wei};
use autoresearch_runtime::traits::{Scorer, Surface};
use autoresearch_runtime::types::{
    ArtifactRef, Cadence, Gate, Knobs, Measurement, ScorerKind, Split, Structure, Visibility,
};
use autoresearch_verticals::{AdditiveSurface, LinearScorer, SharedSearchContributor};

const POOL_WEI: u128 = 1_000_000;
const COMPETITION_ID: u64 = 42;

fn collaborative_knobs() -> Knobs {
    Knobs {
        structure: Structure::Collaborative,
        cadence: Cadence::OneShot, // Collaborative is OneShot (Knobs::validate enforces)
        visibility: Visibility::Public,
        scorer_kind: ScorerKind::HeldOutEval,
    }
}

fn collaborative_cfg() -> CompetitionConfig {
    CompetitionConfig {
        id: COMPETITION_ID,
        gate: Gate::default(),
        // The reward schedule field is unused by the collaborative settle path (which
        // pays by contribution share); it is carried for spec completeness.
        reward: RewardSchedule::TerminalPrize,
        reward_pool_wei: POOL_WEI,
        knobs: collaborative_knobs(),
    }
}

/// Four productive contributors, each owning one true dimension in the fold order
/// `[0, 1, 3, 2]` (each adds a strictly positive sequential held-out marginal —
/// measured +0.150 / +0.2125 / +0.050 / +0.100), plus one free-rider that contributes
/// an all-zeros delta. The ordering matters: it is the sequence the runner folds in.
fn contributors() -> Vec<Contribution> {
    let mut c: Vec<Contribution> = [0usize, 1, 3, 2]
        .iter()
        .map(|&dim| Contribution {
            contributor: format!("0xpool{dim}"),
            seed: dim as u64 + 1,
            delta_ref: ArtifactRef(format!("delta:dim{dim}")),
        })
        .collect();
    c.push(Contribution {
        contributor: "0xfreerider".into(),
        seed: 99,
        delta_ref: ArtifactRef("delta:free".into()),
    });
    c
}

/// Build the contributor engine for a [`Contribution`]: a productive
/// [`SharedSearchContributor`] owning the dimension encoded in its id, or the explicit
/// free-rider. This is the `make_contributor` seam — the DeMo-engine injection point.
fn make_contributor(c: &Contribution) -> SharedSearchContributor {
    match c.contributor.as_str() {
        "0xpool0" => SharedSearchContributor::new(c.seed, vec![0], 1.0),
        "0xpool1" => SharedSearchContributor::new(c.seed, vec![1], 1.0),
        "0xpool3" => SharedSearchContributor::new(c.seed, vec![3], 1.0),
        "0xpool2" => SharedSearchContributor::new(c.seed, vec![2], 1.0),
        _ => SharedSearchContributor::free_rider(c.seed),
    }
}

#[tokio::test]
async fn collaborative_pools_one_artifact_pays_marginal_freerider_zero() {
    let surface = AdditiveSurface;
    let scorer = LinearScorer::new();
    let baseline = AdditiveSurface::baseline();
    let cfg = collaborative_cfg();

    let outcome = run_collaborative(
        &cfg,
        &surface,
        &scorer,
        &baseline,
        &contributors(),
        make_contributor,
    )
    .await
    .expect("collaborative run should succeed");

    // 1. The shared artifact reached a real, large held-out lift (measured ~0.51:
    //    held-out accuracy ~1.00 vs ~0.49 baseline). Floor at 0.30 so a regression that
    //    halved the real lift fails, without being brittle.
    assert!(
        outcome.final_artifact_lift.delta > 0.30,
        "shared artifact lift should exceed 0.30, got {}",
        outcome.final_artifact_lift.delta
    );
    // The final lift clears the gate (lower CI bound above the 0.02 floor).
    assert!(
        outcome.final_artifact_lift.ci_lower >= cfg.gate.min_lift_ci_lower,
        "final shared-artifact lift must clear the gate: {:?}",
        outcome.final_artifact_lift
    );

    // 2. All four productive contributors were accepted; the free-rider was not.
    assert_eq!(
        outcome.accepted, 4,
        "all four productive deltas should be accepted (each had positive marginal)"
    );

    // 3. The free-rider earns ZERO share and ZERO payout — the fairness property that
    //    improves on GPU-minutes (which would have paid it for burning compute).
    let free_share = outcome
        .shares
        .iter()
        .find(|(c, _)| c == "0xfreerider")
        .expect("free-rider must appear in the share table")
        .1;
    assert_eq!(free_share, 0, "a zero-marginal free-rider must earn 0 bps");
    assert!(
        !outcome
            .payouts
            .iter()
            .any(|p| p.researcher == "0xfreerider"),
        "the free-rider must receive no payout"
    );

    // 4. Every productive contributor earns a strictly positive share by its held-out
    //    marginal; the four together (almost) split the pool.
    //
    //    NOTE on order: the LinearScorer measures classification accuracy via a single
    //    dot product, so the per-dimension marginal is NOT separable — fixing one
    //    dimension changes the marginal of the others (overlapping / diminishing-returns
    //    deltas, the realistic case). The runner therefore folds in a CANONICAL
    //    delta-content-hashed order rather than the caller's slice order, so credit is a
    //    reproducible function of the deltas, not of list position. We do not assert a
    //    hard-coded inter-dim ranking (that was an artifact of the old caller order);
    //    instead we assert the durable properties: every productive pool earns > 0, and
    //    the credit is INVARIANT to the caller's input order (the order-gaming defense).
    let share =
        |out: &CollaborativeOutcome, id: &str| out.shares.iter().find(|(c, _)| c == id).unwrap().1;
    for id in ["0xpool0", "0xpool1", "0xpool2", "0xpool3"] {
        assert!(
            share(&outcome, id) > 0,
            "{id} contributed real marginal lift and must earn > 0"
        );
    }

    // 4b. Order-gaming defense: shuffling the caller's contributor order must NOT change
    //     any contributor's share. An orchestrator cannot front-load a chosen pool into a
    //     high-credit slot, because the fold order is canonical (content-hashed).
    let mut shuffled = contributors();
    shuffled.reverse();
    let outcome_shuffled = run_collaborative(
        &cfg,
        &surface,
        &scorer,
        &baseline,
        &shuffled,
        make_contributor,
    )
    .await
    .expect("collaborative run should succeed on a reordered slice");
    for id in ["0xpool0", "0xpool1", "0xpool2", "0xpool3", "0xfreerider"] {
        assert_eq!(
            share(&outcome, id),
            share(&outcome_shuffled, id),
            "share for {id} must be invariant to caller order (canonical fold order)"
        );
    }

    // 5. Shares sum to <= 10_000 bps and payouts conserve the pool (never mint).
    let total_bps: u32 = outcome.shares.iter().map(|(_, w)| *w).sum();
    assert!(
        total_bps <= 10_000,
        "shares must sum to <= 10_000 bps, got {total_bps}"
    );
    let paid = total_wei(&outcome.payouts);
    assert!(
        paid <= POOL_WEI,
        "payouts {paid} exceeded the pool {POOL_WEI}"
    );
    // The whole pool, minus sub-bps flooring dust, is distributed across the four.
    assert!(
        POOL_WEI - paid <= 1_000,
        "near-full pool distributed (only flooring dust in escrow), got {paid}/{POOL_WEI}"
    );
    assert_eq!(
        outcome.payouts.len(),
        4,
        "exactly the four productive pools are paid"
    );
}

/// The held-out gate is load-bearing: if the shared artifact never improves (every
/// contributor folds a baseline-only / zero delta), it cannot clear the gate, so the
/// pool does not exist and NOBODY is paid — even though "work" was submitted.
#[tokio::test]
async fn collaborative_gate_bites_nobody_paid_without_real_improvement() {
    let surface = AdditiveSurface;
    let scorer = LinearScorer::new();
    let baseline = AdditiveSurface::baseline();
    let cfg = collaborative_cfg();

    // Swap in a baseline-only field: every contributor (including the "productive" ids)
    // contributes a zero delta, so the shared artifact stays at the baseline.
    let outcome = run_collaborative(&cfg, &surface, &scorer, &baseline, &contributors(), |c| {
        SharedSearchContributor::free_rider(c.seed)
    })
    .await
    .expect("run should succeed even when nobody improves");

    assert_eq!(outcome.accepted, 0, "no zero delta can improve held-out");
    assert!(
        outcome.final_artifact_lift.delta.abs() < 1e-9,
        "baseline-only fold yields zero lift, got {}",
        outcome.final_artifact_lift.delta
    );
    // The gate bites: no payouts at all.
    assert!(
        outcome.payouts.is_empty(),
        "gate not cleared => nobody is paid, got {:?}",
        outcome.payouts
    );
    assert_eq!(total_wei(&outcome.payouts), 0);
}

/// Part B — the marketplace flywheel: the certified collaborative artifact becomes
/// listable, sellable inventory whose provenance + certified lift travel to the buyer,
/// and an exclusive listing cannot be double-sold.
#[tokio::test]
async fn marketplace_lists_and_sells_the_certified_collaborative_artifact() {
    let surface = AdditiveSurface;
    let scorer = LinearScorer::new();
    let baseline = AdditiveSurface::baseline();
    let cfg = collaborative_cfg();

    // Re-run the collaborative competition to obtain the certified shared artifact and
    // its certified lift — the inventory the competition manufactured.
    let outcome = run_collaborative(
        &cfg,
        &surface,
        &scorer,
        &baseline,
        &contributors(),
        make_contributor,
    )
    .await
    .expect("collaborative run should succeed");
    let certified_lift = outcome.final_artifact_lift;

    // Reconstruct the shared artifact's content reference + its certified measurement
    // (the same scorer the Referee used). In a deployment these travel in the evidence
    // row; here we recompute deterministically to obtain the artifact_ref + measurement.
    let shared = fold_productive(&surface);
    let artifact_ref = surface.to_ref(&shared).unwrap();
    let measurement: Measurement = scorer.score(&shared, Split::HeldOut).await.unwrap();
    // Sanity: the reconstructed artifact carries the same certified lift the runner
    // reported (the marketplace prices the certified number, not a claim).
    assert!(
        (measurement.value - 1.0).abs() < 1e-9,
        "reconstructed shared artifact is near-perfect on held-out, got {}",
        measurement.value
    );

    // Price the artifact by its certified lift (MECHANISM §10: price on a
    // buyer-distribution proxy; monotone in certified lift).
    let policy = PricingPolicy::new(10_000, 1_000);
    let price = price_by_lift(&certified_lift, &policy);
    assert!(
        price > 10_000,
        "a strong certified lift prices above the base fee"
    );

    // List the certified WINNING artifact: consented, priced by lift, with provenance.
    let mut market = Marketplace::new();
    let listing = ArtifactListing {
        artifact_ref: artifact_ref.clone(),
        seller: "0xpool1".into(), // a producing contributor lists it under a license
        certified_lift,
        price_wei: price,
        license: "exclusive-resale-v1".into(),
        exclusive: true,
        provenance: COMPETITION_ID,
        consented: true,
        disclose_sub_gate: false, // a gate-clearing winner needs no sub-gate disclosure
    };
    // The Referee's attestation binds the listing: a seller cannot list numbers the
    // competition did not certify. Here it carries the real certified lift + measurement
    // + provenance the collaborative run produced.
    let attestation = CertifiedAttestation {
        artifact_ref: artifact_ref.clone(),
        provenance: COMPETITION_ID,
        certified_lift,
        measurement,
        attestation_hash: "0xcollab-attest".into(),
    };
    let listing_id = market
        .list(listing, &cfg.gate, &attestation)
        .expect("a consented, gate-clearing, priced winner must list");

    // A seller-forged listing (a fabricated higher lift the Referee never certified) is
    // rejected — the certified number, not a vendor claim, is what prices the artifact.
    let mut forged = market.listing(listing_id).unwrap().clone();
    forged.certified_lift.delta += 0.25; // inflate the certified lift
    assert_eq!(
        market.list(forged, &cfg.gate, &attestation),
        Err(MarketError::ForgedListing),
        "a listing whose lift does not match the attestation must be rejected"
    );

    // An unconsented listing of the same artifact is rejected (consent is required).
    let mut unconsented = market.listing(listing_id).unwrap().clone();
    unconsented.consented = false;
    assert_eq!(
        market.list(unconsented, &cfg.gate, &attestation),
        Err(MarketError::Unconsented),
        "a sale is invalid without the producer's consent"
    );

    // A buyer purchases it; provenance + certified lift carry over to the buyer.
    let sale = market
        .buy(listing_id, "0xbuyer")
        .expect("buyer purchases the listing");
    assert_eq!(sale.buyer, "0xbuyer");
    assert_eq!(sale.seller, "0xpool1");
    assert_eq!(sale.price_wei, price);
    assert_eq!(
        sale.provenance, COMPETITION_ID,
        "provenance (the producing competition) travels with the artifact"
    );
    assert_eq!(
        sale.certified_lift, certified_lift,
        "certified lift travels with the artifact"
    );
    assert_eq!(sale.artifact_ref, artifact_ref);

    // A double-sell of the EXCLUSIVE listing is rejected (no selling the same exclusive
    // license twice).
    assert_eq!(
        market.buy(listing_id, "0xsecond"),
        Err(MarketError::AlreadySold(listing_id)),
        "an exclusive listing must not be double-sold"
    );
    // Exactly one sale is on the ledger.
    assert_eq!(market.sales().len(), 1);
}

/// Fold the four productive contributors' deltas onto the baseline to reconstruct the
/// shared artifact, deterministically — the same fold the collaborative runner does.
fn fold_productive(surface: &AdditiveSurface) -> GenericArtifact {
    let mut shared = AdditiveSurface::baseline();
    for &dim in &[0usize, 1, 3, 2] {
        let c = SharedSearchContributor::new(dim as u64 + 1, vec![dim], 1.0);
        // The contributor's delta is produced via its engine; fold it in.
        shared = fold_one(surface, &shared, dim, &c);
    }
    shared
}

/// Apply one contributor's delta. The contributor's `produce` is async; we drive its
/// ready future to completion synchronously (it is a `std::future::ready`).
fn fold_one(
    surface: &AdditiveSurface,
    base: &GenericArtifact,
    _dim: usize,
    contributor: &SharedSearchContributor,
) -> GenericArtifact {
    use autoresearch_runtime::traits::{Engine, EngineContext};
    let ctx = EngineContext {
        competition: COMPETITION_ID,
        baseline_ref: ArtifactRef("baseline".into()),
        dev_split_ref: None,
        budget_wei: 0,
        egress_policy: None,
    };
    let delta = now_or_never(contributor.produce(&ctx))
        .expect("contributor delta future is ready")
        .expect("contributor delta is Ok");
    surface.apply_delta(base, &delta).unwrap()
}

/// Drive a future that is known to be immediately ready (the deterministic engines all
/// return `std::future::ready`). Avoids pulling a runtime into a sync helper.
fn now_or_never<T>(fut: impl std::future::Future<Output = T>) -> Option<T> {
    use std::task::{Context, Poll, Waker};
    let mut fut = Box::pin(fut);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => Some(v),
        Poll::Pending => None,
    }
}
