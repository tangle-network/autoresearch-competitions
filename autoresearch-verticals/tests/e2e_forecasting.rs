//! End-to-end: the autoresearch market runs a **forecasting** competition and pays
//! only for a certified model that generalizes out of sample.
//!
//! Every researcher drives the **same universal** [`SupervisorEngine`] — the one
//! seeded local search shared across all verticals — differing only by start point,
//! search budget, step, and seed. The engine searches the forecaster's AR-coefficient
//! encoding ([`GenericArtifact::params`]) to drive the *dev* (in-sample) error down;
//! the market re-scores the produced model on a **held-out** window of the same
//! process (with an out-of-sample complexity penalty), gates, ranks, and pays.
//!
//! Deterministic (a synthetic series, no live data) so it runs in CI. It proves the
//! *market mechanism around forecasting*: one universal engine, held-out re-scoring of
//! a delegated model, and the gate refusing an over-fit that only looked good in-sample.

use autoresearch_protocol::{CompetitionConfig, ResearcherRun, run_oneshot_competitive};
use autoresearch_runtime::reward::{RewardSchedule, total_wei};
use autoresearch_runtime::traits::Scorer;
use autoresearch_runtime::types::{Cadence, Gate, Knobs, ScorerKind, Split, Structure, Visibility};
use autoresearch_supervisor::{ArtifactKind, GenericArtifact, GenericSurface, SupervisorEngine};
use autoresearch_verticals::forecasting::{ForecastScorer, baseline, start};

const POOL_WEI: u128 = 1_000_000;
const EVAL_SHARDS: u32 = 16; // >= Gate::default().min_n (12)

fn knobs() -> Knobs {
    Knobs {
        structure: Structure::Competitive,
        cadence: Cadence::OneShot,
        visibility: Visibility::Public,
        scorer_kind: ScorerKind::HeldOutEval,
    }
}

struct Hypothesis {
    start: GenericArtifact,
    budget: usize,
    step: f64,
}

fn forecaster(coeffs: Vec<f64>, label: &str) -> GenericArtifact {
    GenericArtifact::new(ArtifactKind::Forecast, coeffs, label.to_string())
}

fn hypothesis_for(name: &str) -> Hypothesis {
    match name {
        // Patient search from the zero-coefficient baseline: recovers the true AR
        // structure and generalizes out of sample. The decisive winner.
        "recover-coeffs" => Hypothesis {
            start: start(),
            budget: 4000,
            step: 0.4,
        },
        // Warm-started near the true coefficients with a small refinement budget: a
        // real, gate-clearing forecaster.
        "warm-refine" => Hypothesis {
            start: forecaster(vec![0.6, 0.2, 0.1], "warm-started forecaster"),
            budget: 600,
            step: 0.2,
        },
        // Too little budget to recover the full signal: only a partial fit, whose lift
        // is real but below this competition's high bar — refused.
        "under-search" => Hypothesis {
            start: start(),
            budget: 5,
            step: 0.2,
        },
        // Inflated coefficients (chasing in-sample noise) that pay the out-of-sample
        // complexity penalty: an over-parameterized model whose generalizing lift falls
        // below the high bar — refused.
        "over-fit" => Hypothesis {
            start: forecaster(vec![1.6, -1.3, 1.0], "inflated (over-fit) forecaster"),
            budget: 4,
            step: 0.05,
        },
        _ => Hypothesis {
            start: start(),
            budget: 256,
            step: 1.0,
        },
    }
}

#[tokio::test]
async fn market_improves_forecasting_on_heldout() {
    let scorer = ForecastScorer::new(EVAL_SHARDS);

    let baseline = baseline();
    let base_m = scorer
        .score(&baseline, Split::HeldOut)
        .await
        .expect("baseline scores");
    println!(
        "\n=== forecasting market — {EVAL_SHARDS} eval shards ===\n\
         baseline (zero-coefficient forecaster): value(-rmse) = {:.4} (CI ±{:.4}, n={})",
        base_m.value,
        (base_m.ci_upper - base_m.ci_lower) / 2.0,
        base_m.n,
    );

    let names = ["recover-coeffs", "warm-refine", "under-search", "over-fit"];
    let researchers: Vec<ResearcherRun> = names
        .iter()
        .enumerate()
        .map(|(i, n)| ResearcherRun {
            researcher: (*n).to_string(),
            seed: i as u64 + 1,
        })
        .collect();

    // This proposer demands a forecaster that recovers most of the autoregressive
    // signal: a high lift bar. A partial fit (under-searched, or an over-parameterized
    // model that only captures the trend) produces a real but insufficient lift and is
    // refused — only a model that generalizes near the true coefficients is paid.
    let cfg = CompetitionConfig {
        id: 1,
        gate: Gate {
            min_lift_ci_lower: 0.45,
            cost_per_task_ceiling: None,
            min_n: 12,
        },
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
            SupervisorEngine::new(run.researcher.clone(), h.start, scorer.clone(), run.seed)
                .with_budget(h.budget)
                .with_step(h.step)
        })
        .await
        .expect("competition runs");

    println!("\nleaderboard (gate-clearing only, best lift first):");
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
    println!("gated out (no certified, generalizing forecast): {gated_out:?}");
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
        top_lift.delta > 0.05,
        "winner must beat baseline by a real out-of-sample margin, got {:.4}",
        top_lift.delta
    );
    assert!(
        top_lift.ci_lower >= cfg.gate.min_lift_ci_lower,
        "winner clears the gate CI floor"
    );
    for bad in ["under-search", "over-fit"] {
        assert!(gated_out.contains(&bad), "{bad} must be gated out");
    }
    assert!(
        total_wei(&outcome.payouts) <= POOL_WEI,
        "payouts conserve the pool"
    );
}
