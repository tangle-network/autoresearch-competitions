//! End-to-end: the autoresearch market runs a **combinatorial-solver** competition
//! and pays only for a certified, generalizing improvement in solution quality.
//!
//! Every researcher drives the **same universal** [`SupervisorEngine`] — the one
//! seeded local search shared across all verticals — differing only by start point,
//! search budget, step, and seed. The engine searches the solver's heuristic-weight
//! encoding ([`GenericArtifact::params`]) to raise the *dev* objective
//! [`SolverScorer`] reports; the market re-scores the produced weights on **held-out**
//! instances (a small distribution shift), gates, ranks, and pays.
//!
//! Deterministic (no real solver) so it runs in CI. It proves the *market mechanism
//! around solver design*: one universal engine, held-out re-scoring of delegated
//! heuristic weights, and the gate refusing weights that do not actually improve
//! solution quality out of sample.

use autoresearch_protocol::{CompetitionConfig, ResearcherRun, run_oneshot_competitive};
use autoresearch_runtime::reward::{RewardSchedule, total_wei};
use autoresearch_runtime::traits::Scorer;
use autoresearch_runtime::types::{Cadence, Gate, Knobs, ScorerKind, Split, Structure, Visibility};
use autoresearch_supervisor::{GenericArtifact, GenericSurface, SupervisorEngine};
use autoresearch_verticals::combinatorial_solver::{
    SolverScorer, baseline_artifact, dev_optimum, solver_artifact,
};

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

fn hypothesis_for(name: &str) -> Hypothesis {
    // A clearly-bad weight vector of the right dimension (uniformly extreme), far from
    // the good heuristic — a stuck researcher cannot escape it on a tiny budget.
    let bad_weights = vec![8.0; dev_optimum().len()];
    match name {
        // Patient search from the un-tuned (zero-weight) heuristic: climbs to the good
        // heuristic and generalizes. The decisive winner.
        "tune-heuristic" => Hypothesis {
            start: baseline_artifact(),
            budget: 4000,
            step: 0.6,
        },
        // Warm-started at the dev optimum with a small refinement budget: a real,
        // gate-clearing improvement, just less search.
        "warm-start" => Hypothesis {
            start: solver_artifact(dev_optimum(), "warm-started heuristic"),
            budget: 600,
            step: 0.3,
        },
        // Far too little budget to move off the un-tuned baseline: no real improvement.
        "tiny-budget" => Hypothesis {
            start: baseline_artifact(),
            budget: 5,
            step: 0.2,
        },
        // Starts at extreme, poor weights with a tiny step and budget: cannot climb back
        // to a good heuristic within budget, so it stays a poor solver and is refused.
        "stuck-bad" => Hypothesis {
            start: solver_artifact(bad_weights, "extreme (poor) weights"),
            budget: 4,
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
async fn market_improves_combinatorial_solver_on_heldout() {
    let scorer = SolverScorer::new(EVAL_INSTANCES);

    let baseline = baseline_artifact();
    let base_m = scorer
        .score(&baseline, Split::HeldOut)
        .await
        .expect("baseline scores");
    println!(
        "\n=== combinatorial-solver market — {EVAL_INSTANCES} held-out instances ===\n\
         baseline (un-tuned heuristic): objective = {:.4} (CI ±{:.4}, n={})",
        base_m.value,
        (base_m.ci_upper - base_m.ci_lower) / 2.0,
        base_m.n,
    );

    let names = ["tune-heuristic", "warm-start", "tiny-budget", "stuck-bad"];
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
        gate: Gate::default(),
        reward: RewardSchedule::SnapshotTopK {
            weights_bps: vec![6_000, 4_000],
        },
        reward_pool_wei: POOL_WEI,
        knobs: knobs(),
    };

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

    println!("\nleaderboard (gate-clearing only, best objective-lift first):");
    for (rank, (name, lift)) in outcome.ranked.iter().enumerate() {
        let pay = outcome
            .payouts
            .iter()
            .find(|p| &p.researcher == name)
            .map_or(0, |p| p.wei);
        println!(
            "  #{}  {name:<16} lift={:.4} (ci_lower={:.4}, n={})  pay={pay}",
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
    println!("gated out (no certified, generalizing improvement): {gated_out:?}");
    println!(
        "pool {POOL_WEI} -> paid {} across {} winners\n",
        total_wei(&outcome.payouts),
        outcome.winners
    );

    assert!(
        outcome.winners >= 1,
        "expected >=1 gate-clearing researcher"
    );
    assert!(
        outcome.winners >= 2,
        "both well-searched researchers should clear the gate"
    );
    let (_top_name, top_lift) = &outcome.ranked[0];
    assert!(
        top_lift.delta > 0.10,
        "winner must beat baseline by a real objective margin, got {:.4}",
        top_lift.delta
    );
    assert!(
        top_lift.ci_lower >= cfg.gate.min_lift_ci_lower,
        "winner clears the gate CI floor"
    );
    for bad in ["tiny-budget", "stuck-bad"] {
        assert!(gated_out.contains(&bad), "{bad} must be gated out");
    }
    assert!(
        total_wei(&outcome.payouts) <= POOL_WEI,
        "payouts conserve the pool"
    );
}
