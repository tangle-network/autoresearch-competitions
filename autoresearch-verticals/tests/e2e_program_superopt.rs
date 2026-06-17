//! End-to-end: the autoresearch market runs a **program-superoptimization**
//! competition and pays only for a certified, generalizing, *correct* speedup.
//!
//! Every researcher drives the **same universal** [`SupervisorEngine`] — the one
//! seeded local search shared across all verticals. They differ only by **seed,
//! search budget, step size, and start point**, never by a different engine. The
//! engine searches the recipe encoding (`[unroll, vectorize, cache_block]` in
//! [`GenericArtifact::params`]) to maximize the *dev* speedup [`ProgramScorer`]
//! reports; the market then re-scores each produced recipe on a **held-out** benchmark
//! instance, gates it on a real certified speedup, ranks, and pays.
//!
//! The well-budgeted searches find the optimum and clear the gate; the tiny-budget
//! search never gets off the baseline; and the search that starts deep in the modeled
//! *incorrectness* region with too little budget to escape stays a fast-but-wrong
//! program and is refused. This is fully deterministic (no compiler, no CPU timing),
//! so it runs in CI rather than being `#[ignore]`d. It proves the *market mechanism
//! around superoptimization*: one universal engine, held-out re-scoring of a delegated
//! recipe, a correctness constraint refusing fast-but-wrong programs, and the gate
//! refusing speedups that do not generalize.

use autoresearch_protocol::{CompetitionConfig, ResearcherRun, run_oneshot_competitive};
use autoresearch_runtime::reward::{RewardSchedule, total_wei};
use autoresearch_runtime::traits::Scorer;
use autoresearch_runtime::types::{Cadence, Gate, Knobs, ScorerKind, Split, Structure, Visibility};
use autoresearch_supervisor::{ArtifactKind, GenericArtifact, GenericSurface, SupervisorEngine};
use autoresearch_verticals::program_superopt::{ProgramScorer, RECIPE_DIM, baseline_artifact};

const POOL_WEI: u128 = 1_000_000;
const EVAL_INSTANCES: u32 = 16; // >= Gate::default().min_n (12)

fn knobs() -> Knobs {
    Knobs {
        structure: Structure::Competitive,
        cadence: Cadence::OneShot,
        visibility: Visibility::Public,
        scorer_kind: ScorerKind::HeldOutEval,
    }
}

/// A researcher hypothesis: the start point and search budget/step that parameterize
/// the *same* universal [`SupervisorEngine`].
struct Hypothesis {
    start: GenericArtifact,
    budget: usize,
    step: f64,
}

/// Map a researcher name to how it parameterizes the universal engine. Note: NONE of
/// these returns a different engine — they only change start/budget/step/seed.
fn hypothesis_for(name: &str) -> Hypothesis {
    match name {
        // Patient, well-budgeted search from the un-optimized baseline: climbs to the
        // hidden optimum and stays correct. The decisive winner.
        "deep-search" => Hypothesis {
            start: baseline_artifact(),
            budget: 4000,
            step: 0.6,
        },
        // A second genuine improvement: warm-started near the optimum with a smaller
        // refinement budget. A real, gate-clearing speedup, just less search.
        "warm-refine" => Hypothesis {
            start: GenericArtifact::new(
                ArtifactKind::Program,
                vec![3.0, 7.0, 1.5],
                "warm-started kernel",
            ),
            budget: 800,
            step: 0.4,
        },
        // Far too little search budget to move off the baseline: no real speedup.
        "tiny-budget" => Hypothesis {
            start: baseline_artifact(),
            budget: 6,
            step: 0.2,
        },
        // Starts DEEP in the modeled incorrectness region (unroll*vectorize far above
        // the data-dependence bound) with a tiny step and tiny budget: it cannot climb
        // back to a correct recipe within budget, so it stays a fast-but-WRONG program
        // and is refused outright.
        "reckless-stuck" => Hypothesis {
            start: GenericArtifact::new(
                ArtifactKind::Program,
                vec![40.0, 40.0, 2.0],
                "over-aggressive (incorrect) kernel",
            ),
            budget: 5,
            step: 0.05,
        },
        _ => Hypothesis {
            start: baseline_artifact(),
            budget: 256,
            step: 1.0,
        },
    }
}

#[tokio::test]
async fn market_improves_program_superopt_on_heldout() {
    assert_eq!(RECIPE_DIM, 3, "recipe is [unroll, vectorize, cache_block]");
    let scorer = ProgramScorer::new(EVAL_INSTANCES);

    // Baseline: the un-optimized kernel. A candidate must beat this on held-out to
    // certify a speedup.
    let baseline = baseline_artifact();
    let base_m = scorer
        .score(&baseline, Split::HeldOut)
        .await
        .expect("baseline scores");
    println!(
        "\n=== program-superopt market — {EVAL_INSTANCES} benchmark instances ===\n\
         baseline (un-optimized kernel): speedup = {:.4} (CI ±{:.4}, n={})",
        base_m.value,
        (base_m.ci_upper - base_m.ci_lower) / 2.0,
        base_m.n,
    );

    let names = [
        "deep-search",
        "warm-refine",
        "tiny-budget",
        "reckless-stuck",
    ];
    let researchers: Vec<ResearcherRun> = names
        .iter()
        .enumerate()
        .map(|(i, n)| ResearcherRun {
            researcher: (*n).to_string(),
            seed: i as u64 + 1,
        })
        .collect();

    let cfg = CompetitionConfig {
        id: 1,
        gate: Gate::default(), // min_lift_ci_lower=0.02, min_n=12
        reward: RewardSchedule::SnapshotTopK {
            weights_bps: vec![6_000, 4_000],
        },
        reward_pool_wei: POOL_WEI,
        knobs: knobs(),
    };

    // EVERY researcher drives the SAME universal SupervisorEngine; they differ only by
    // start / budget / step / seed (the dev scorer is the researcher-visible signal it
    // hill-climbs on Split::Dev). The Referee re-scores the produced recipe on held-out.
    let surface = GenericSurface;
    let outcome =
        run_oneshot_competitive(&cfg, &surface, &scorer, &baseline, &researchers, |run| {
            let h = hypothesis_for(&run.researcher);
            SupervisorEngine::new(run.researcher.clone(), h.start, scorer, run.seed)
                .with_budget(h.budget)
                .with_step(h.step)
        })
        .await
        .expect("competition runs");

    println!("\nleaderboard (gate-clearing only, best speedup-lift first):");
    for (rank, (name, lift)) in outcome.ranked.iter().enumerate() {
        let pay = outcome
            .payouts
            .iter()
            .find(|p| &p.researcher == name)
            .map_or(0, |p| p.wei);
        println!(
            "  #{}  {name:<14} lift={:.4} (ci_lower={:.4}, n={})  pay={pay}",
            rank + 1,
            lift.delta,
            lift.ci_lower,
            lift.n,
        );
    }
    let gated_out: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| !outcome.ranked.iter().any(|(r, _)| r == n))
        .collect();
    println!("gated out (no certified, correct, generalizing speedup): {gated_out:?}");
    println!(
        "pool {POOL_WEI} -> paid {} across {} winners\n",
        total_wei(&outcome.payouts),
        outcome.winners
    );

    // --- assertions: the market found genuine speedups and refused the rest ----------
    assert!(
        outcome.winners >= 1,
        "expected >=1 gate-clearing researcher"
    );
    assert!(
        outcome.winners >= 2,
        "both well-searched researchers should clear the gate"
    );

    let (top_name, top_lift) = &outcome.ranked[0];
    assert_eq!(
        top_name, "deep-search",
        "the deep, well-budgeted search is the decisive winner"
    );
    assert!(
        top_lift.delta > 0.3,
        "winner must beat baseline by a real speedup margin, got {:.4}",
        top_lift.delta
    );
    assert!(
        top_lift.ci_lower >= cfg.gate.min_lift_ci_lower,
        "winner clears the gate CI floor"
    );

    // The no-speedup and fast-but-incorrect researchers must NOT be paid.
    for bad in ["tiny-budget", "reckless-stuck"] {
        assert!(gated_out.contains(&bad), "{bad} must be gated out");
    }

    // Sanity: the produced winner is a CORRECT, well-optimized program — re-derive its
    // held-out speedup directly to confirm the gate certified a real, correct recipe.
    let winner_engine = {
        let h = hypothesis_for("deep-search");
        SupervisorEngine::new("deep-search", h.start, scorer, 1)
            .with_budget(h.budget)
            .with_step(h.step)
    };
    let ctx = autoresearch_runtime::traits::EngineContext {
        competition: cfg.id,
        baseline_ref: autoresearch_runtime::types::ArtifactRef("base".into()),
        dev_split_ref: None,
        budget_wei: 0,
        egress_policy: None,
    };
    use autoresearch_runtime::traits::Engine;
    let produced = winner_engine.produce(&ctx).await.expect("winner produces");
    use autoresearch_verticals::program_superopt::OptRecipe;
    assert!(
        OptRecipe::from_params(&produced.params).is_correct(),
        "the paid winner must be a correct program, not fast-but-wrong"
    );

    assert!(
        total_wei(&outcome.payouts) <= POOL_WEI,
        "payouts conserve the pool"
    );
}
