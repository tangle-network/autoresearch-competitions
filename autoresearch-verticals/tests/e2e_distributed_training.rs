//! End-to-end: the autoresearch market runs a **distributed-training** competition
//! and pays only for certified, generalizing improvement.
//!
//! Five researchers submit real DiLoCo/DeMo-style training recipes; each is trained
//! by a [`LocalSimCluster`] (the deterministic stand-in for a `prime`/Psyche
//! service instance), and the market re-scores every produced artifact on a
//! held-out split, gates, ranks, and pays. The strong recipe (more islands at the
//! optimal sync interval) wins; the failure-mode recipes (island drift from a
//! too-large sync interval, over-compressed gradients, a mis-set learning rate)
//! are gated out.
//!
//! Unlike `e2e_nanogpt` this is fully deterministic (no Python/torch/GPU), so it
//! runs in CI rather than being `#[ignore]`d. It proves the *market mechanism
//! around delegated training*: cluster-agnostic dispatch, held-out re-scoring of a
//! delegated artifact, and the promotion gate refusing plausible-but-worse recipes.

use autoresearch_protocol::{CompetitionConfig, ResearcherRun, run_oneshot_competitive};
use autoresearch_runtime::reward::{RewardSchedule, total_wei};
use autoresearch_runtime::traits::Scorer;
use autoresearch_runtime::types::{Cadence, Gate, Knobs, ScorerKind, Split, Structure, Visibility};
use autoresearch_verticals::distributed_training::{
    DistributedTrainingEngine, DistributedTrainingScorer, DistributedTrainingSurface,
    LocalSimCluster, TrainingRecipe,
};

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

/// The five researcher hypotheses (real recipes). `scale-islands` is the strong
/// winner; `comm-efficient` is a moderate, genuine improvement; the other three
/// are plausible-but-worse and must be gated out.
fn recipe_for(name: &str) -> TrainingRecipe {
    let base = TrainingRecipe::baseline();
    match name {
        // 8 data-parallel islands at the optimal sync interval, mild compression.
        "scale-islands" => TrainingRecipe {
            islands: 8,
            inner_steps: 32,
            keep_fraction: 0.2,
            ..base
        },
        // 4 islands, optimal sync interval, DeMo compression at its sweet spot.
        "comm-efficient" => TrainingRecipe {
            islands: 4,
            inner_steps: 32,
            keep_fraction: 0.1,
            ..base
        },
        // Sync far too rarely: replicas drift apart -> worse generalization.
        "too-large-H" => TrainingRecipe {
            islands: 4,
            inner_steps: 4000,
            keep_fraction: 0.1,
            ..base
        },
        // Over-compress the gradient: loses signal, generalizes worse.
        "over-compress" => TrainingRecipe {
            islands: 4,
            inner_steps: 32,
            keep_fraction: 0.0005,
            ..base
        },
        // Inner learning rate 10x too low: trains poorly.
        "bad-lr" => TrainingRecipe {
            islands: 4,
            inner_steps: 32,
            keep_fraction: 0.1,
            inner_lr: 3e-4,
            ..base
        },
        _ => base,
    }
}

#[tokio::test]
async fn market_improves_distributed_training_on_heldout() {
    let surface = DistributedTrainingSurface;
    let scorer = DistributedTrainingScorer::new(EVAL_SHARDS);

    // Baseline: the reference recipe, trained once. A candidate must beat this on
    // held-out to certify lift.
    let baseline = LocalSimCluster.train_sync(&TrainingRecipe::baseline(), 0);
    let base_m = scorer
        .score(&baseline, Split::HeldOut)
        .await
        .expect("baseline scores");
    let baseline_loss = -base_m.value;
    println!(
        "\n=== distributed-training market — {EVAL_SHARDS} eval shards ===\n\
         baseline (1 island, H=1, no compression): val_loss = {baseline_loss:.4} \
         (CI ±{:.4}, n={})",
        (base_m.ci_upper - base_m.ci_lower) / 2.0,
        base_m.n,
    );

    let names = [
        "scale-islands",
        "comm-efficient",
        "too-large-H",
        "over-compress",
        "bad-lr",
    ];
    let researchers: Vec<ResearcherRun> = names
        .iter()
        .enumerate()
        .map(|(i, n)| ResearcherRun {
            researcher: (*n).to_string(),
            seed: i as u64,
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

    let outcome =
        run_oneshot_competitive(&cfg, &surface, &scorer, &baseline, &researchers, |run| {
            DistributedTrainingEngine::new(
                run.researcher.clone(),
                recipe_for(&run.researcher),
                run.seed,
                LocalSimCluster,
            )
        })
        .await
        .expect("competition runs");

    println!("\nleaderboard (gate-clearing only, best lift first):");
    for (rank, (name, lift)) in outcome.ranked.iter().enumerate() {
        let cand_loss = baseline_loss - lift.delta;
        let pay = outcome
            .payouts
            .iter()
            .find(|p| &p.researcher == name)
            .map_or(0, |p| p.wei);
        println!(
            "  #{}  {name:<14} val_loss={cand_loss:.4}  lift={:.4} (ci_lower={:.4})  pay={pay}",
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
    println!("gated out (no real lift): {gated_out:?}");
    println!(
        "pool {POOL_WEI} -> paid {} across {} winners\n",
        total_wei(&outcome.payouts),
        outcome.winners
    );

    // --- assertions: the market found genuine improvement and refused the noise --
    assert!(
        outcome.winners >= 1,
        "expected >=1 gate-clearing researcher"
    );
    let (top_name, top_lift) = &outcome.ranked[0];
    assert_eq!(
        top_name, "scale-islands",
        "scale-islands is the decisive winner"
    );
    assert!(
        top_lift.delta > 0.10,
        "winner must beat baseline by a real margin, got {:.4}",
        top_lift.delta
    );
    assert!(
        top_lift.ci_lower >= cfg.gate.min_lift_ci_lower,
        "winner clears the gate CI floor"
    );
    // The plausible-but-worse recipes must NOT be paid.
    for bad in ["too-large-H", "over-compress", "bad-lr"] {
        assert!(gated_out.contains(&bad), "{bad} must be gated out");
    }
    assert!(
        total_wei(&outcome.payouts) <= POOL_WEI,
        "payouts conserve the pool"
    );
}
