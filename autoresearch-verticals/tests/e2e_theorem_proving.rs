//! End-to-end: the autoresearch market runs a **theorem-proving** competition and
//! pays only for a proof a held-out checker **accepts** and that is strictly
//! **shorter** than the baseline.
//!
//! Four researchers each drive the *same* generic [`GenericEngine`] — they
//! differ only by their starting tactic vector, search seed, budget, and step size,
//! NOT by a different engine. Each searches the tactic-vector encoding to maximise
//! its dev-checker score; the market then re-checks every produced proof on the
//! stricter **held-out** checker, gates, ranks, and pays.
//!
//! - `minimal-proof` lands on the correct-proof target: a valid, minimal proof — the
//!   decisive winner.
//! - `clever-rewrite` lands just off-centre: a valid, near-minimal proof — a real but
//!   smaller win.
//! - `overfit-tactics` starts in the dev-valid-but-held-out-invalid annulus with a
//!   tiny budget: it nudges its *dev* score up but cannot escape into the held-out
//!   region, so the Referee's stricter checker **rejects** it — the proof-domain
//!   generalization gap. Gated out.
//! - `diverged-search` starts far from any valid proof with a tiny budget: it never
//!   reaches a valid region, so the checker rejects it outright. Gated out.
//!
//! Like `e2e_distributed_training` this is fully deterministic (no Lean/Coq/kernel),
//! so it runs in CI rather than being `#[ignore]`d. It proves the *market mechanism
//! around delegated proving*: one generic engine across researchers, a binary
//! accept/reject checker, held-out re-scoring of a delegated proof, and the promotion
//! gate refusing invalid or over-long proofs.

use autoresearch_generic_engine::{GenericArtifact, GenericEngine, GenericSurface};
use autoresearch_protocol::{CompetitionConfig, ResearcherRun, run_oneshot_competitive};
use autoresearch_runtime::reward::{RewardSchedule, total_wei};
use autoresearch_runtime::traits::Scorer;
use autoresearch_runtime::types::{Cadence, Gate, Knobs, ScorerKind, Split, Structure, Visibility};
use autoresearch_verticals::theorem_proving::{
    ProofScorer, TACTIC_DIM, VALID_BONUS, baseline_proof, proof_at,
};

const POOL_WEI: u128 = 1_000_000;
const CHECKS: u32 = 16; // >= Gate::default().min_n (12)

/// The correct-proof tactic vector (mirrors `theorem_proving::TARGET`). A proof at
/// this point is valid on both splits and minimal-size.
const TARGET: [f64; TACTIC_DIM] = [1.0, -1.0, 0.5, 2.0];

fn knobs() -> Knobs {
    Knobs {
        structure: Structure::Competitive,
        cadence: Cadence::OneShot,
        visibility: Visibility::Public,
        scorer_kind: ScorerKind::HeldOutEval,
    }
}

/// Each researcher's distinct **start point** (the proof they begin searching from).
/// The engine, scorer, surface, and gate are identical across researchers — only the
/// start (and the seed/budget/step below) differ.
fn start_for(name: &str) -> GenericArtifact {
    match name {
        // Begins already at the correct proof — search only confirms/tightens it.
        "minimal-proof" => proof_at(TARGET.to_vec(), "start: candidate minimal proof"),
        // Begins a short hop off-centre; search walks it toward the target.
        "clever-rewrite" => proof_at(
            TARGET.iter().map(|t| t + 0.5).collect(),
            "start: rewrite-heavy proof, near-minimal",
        ),
        // Begins inside the dev-valid-but-held-out-invalid annulus (distance 1.3,
        // between HELDOUT_RADIUS=1.0 and DEV_RADIUS=1.5).
        "overfit-tactics" => proof_at(
            vec![TARGET[0] + 1.3, TARGET[1], TARGET[2], TARGET[3]],
            "start: proof tuned to the dev lemma statements only",
        ),
        // Begins far from any valid proof.
        "diverged-search" => proof_at(vec![5.0; TACTIC_DIM], "start: unfocused tactic flailing"),
        _ => baseline_proof(),
    }
}

/// Each researcher's search seed.
fn seed_for(name: &str) -> u64 {
    match name {
        "minimal-proof" => 1,
        "clever-rewrite" => 2,
        "overfit-tactics" => 7,
        "diverged-search" => 4,
        _ => 0,
    }
}

/// Each researcher's `(budget, step)`. The two winners get a real budget and step so
/// the search reaches the held-out region; the two gated researchers get a tiny
/// budget and step so they cannot escape their (annulus / far) start point — the
/// honest reason an under-resourced proof search fails the checker.
fn budget_step_for(name: &str) -> (usize, f64) {
    match name {
        "minimal-proof" => (400, 0.5),
        "clever-rewrite" => (600, 0.3),
        "overfit-tactics" => (6, 0.03),
        "diverged-search" => (12, 0.05),
        _ => (256, 1.0),
    }
}

#[tokio::test]
async fn market_certifies_valid_short_proofs_and_gates_invalid_ones() {
    let surface = GenericSurface;
    let scorer = ProofScorer::new(CHECKS);

    // Baseline: a long-but-valid proof. A candidate must stay valid on held-out AND
    // get strictly shorter to certify lift.
    let baseline = baseline_proof();
    let base_m = scorer
        .score(&baseline, Split::HeldOut)
        .await
        .expect("baseline scores");
    // value = VALID_BONUS - size, so the baseline's proof size is VALID_BONUS - value.
    let baseline_size = VALID_BONUS - base_m.value;
    println!(
        "\n=== theorem-proving market — {CHECKS} checks/proof ===\n\
         baseline (long-but-valid proof): size = {baseline_size:.3} steps, \
         value = {:.3} (CI ±{:.4}, n={})",
        base_m.value,
        (base_m.ci_upper - base_m.ci_lower) / 2.0,
        base_m.n,
    );

    let names = [
        "minimal-proof",
        "clever-rewrite",
        "overfit-tactics",
        "diverged-search",
    ];
    let researchers: Vec<ResearcherRun> = names
        .iter()
        .map(|n| ResearcherRun {
            researcher: (*n).to_string(),
            seed: seed_for(n),
        })
        .collect();

    let cfg = CompetitionConfig {
        id: 1,
        gate: Gate::default(), // min_lift_ci_lower=0.02, min_n=12
        reward: RewardSchedule::SnapshotTopK {
            weights_bps: vec![5_000, 3_000, 2_000],
        },
        reward_pool_wei: POOL_WEI,
        knobs: knobs(),
    };

    // Every researcher drives the SAME generic GenericEngine; they differ only
    // by start point, seed, budget, and step.
    let scorer_for_engine = scorer;
    let outcome = run_oneshot_competitive(
        &cfg,
        &surface,
        &scorer,
        &baseline,
        &researchers,
        |run: &ResearcherRun| {
            let name = run.researcher.as_str();
            let (budget, step) = budget_step_for(name);
            GenericEngine::new(
                run.researcher.clone(),
                start_for(name),
                scorer_for_engine,
                run.seed,
            )
            .with_budget(budget)
            .with_step(step)
        },
    )
    .await
    .expect("competition runs");

    println!("\nleaderboard (gate-clearing only, best lift first):");
    for (rank, (name, lift)) in outcome.ranked.iter().enumerate() {
        let cand_size = baseline_size - lift.delta;
        let pay = outcome
            .payouts
            .iter()
            .find(|p| &p.researcher == name)
            .map_or(0, |p| p.wei);
        println!(
            "  #{}  {name:<16} proof_size={cand_size:.3}  lift={:.4} (ci_lower={:.4})  pay={pay}",
            rank + 1,
            lift.delta,
            lift.ci_lower,
        );
    }
    let gated_out: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| !outcome.ranked.iter().any(|(r, _)| r == n))
        .collect();
    println!("gated out (invalid or no real lift): {gated_out:?}");
    println!(
        "pool {POOL_WEI} -> paid {} across {} winners\n",
        total_wei(&outcome.payouts),
        outcome.winners,
    );

    // --- assertions: the market certified valid, short proofs and refused the rest --
    assert!(
        outcome.winners >= 1,
        "expected >=1 gate-clearing researcher"
    );
    let (top_name, top_lift) = &outcome.ranked[0];
    assert_eq!(
        top_name, "minimal-proof",
        "the minimal valid proof is the decisive winner"
    );
    assert!(
        top_lift.delta > 1.0,
        "winner must be meaningfully shorter than the baseline, got {:.4}",
        top_lift.delta,
    );
    assert!(
        top_lift.ci_lower >= cfg.gate.min_lift_ci_lower,
        "winner clears the gate CI floor"
    );

    // At least one valid+short proof clears, and a second genuine improvement clears.
    assert!(
        outcome.ranked.iter().any(|(r, _)| r == "clever-rewrite"),
        "the near-minimal valid proof must also clear"
    );

    // The invalid / over-long proofs must NOT be paid.
    for bad in ["overfit-tactics", "diverged-search"] {
        assert!(
            gated_out.contains(&bad),
            "{bad} must be gated out (held-out checker rejects it)"
        );
    }

    // Sanity: every gated-out researcher really is held-out-INVALID (negative value),
    // proving the gate fired on rejection, not on a near-miss valid proof.
    for bad in &gated_out {
        let start = start_for(bad);
        let (budget, step) = budget_step_for(bad);
        let produced = GenericEngine::new(*bad, start, scorer, seed_for(bad))
            .with_budget(budget)
            .with_step(step)
            .pipe_produce()
            .await;
        let held = scorer.measure(&produced, Split::HeldOut).value;
        assert!(
            held < 0.0,
            "{bad}'s produced proof must be held-out INVALID (value {held} < 0)"
        );
    }

    assert!(
        total_wei(&outcome.payouts) <= POOL_WEI,
        "payouts conserve the pool"
    );
}

/// Small extension trait so the assertion block can re-produce a researcher's proof
/// through the same engine without an `EngineContext` boilerplate at each call site.
trait PipeProduce {
    async fn pipe_produce(self) -> GenericArtifact;
}

impl<Sc> PipeProduce for GenericEngine<Sc>
where
    Sc: Scorer<Artifact = GenericArtifact> + Clone + Send + Sync,
{
    async fn pipe_produce(self) -> GenericArtifact {
        use autoresearch_runtime::traits::{Engine, EngineContext};
        use autoresearch_runtime::types::ArtifactRef;
        let ctx = EngineContext {
            competition: 1,
            baseline_ref: ArtifactRef("base".into()),
            dev_split_ref: None,
            budget_wei: 0,
            egress_policy: None,
        };
        self.produce(&ctx).await.expect("engine produces a proof")
    }
}
