//! Real-world validation: the autoresearch market improving a char-level **nanoGPT**.
//!
//! Five researchers submit real hyper-/architecture-parameter hypotheses; the Referee
//! (`NanoGptScorer`) trains each — and the baseline — for a fixed 300-iter budget over
//! 12 seeds and measures the held-out val loss with a CI; the market gates, ranks, and
//! pays. A winning config that reaches a lower val loss at the same compute is a genuine
//! improvement, so the certified lift is the real reduction in val loss.
//!
//! This runs Karpathy's nanoGPT on real data, so it is `#[ignore]`d (needs Python +
//! torch + the prepared `shakespeare_char` data, ~8 min on CPU). Run it with:
//!
//! ```text
//! cargo test -p autoresearch-verticals --test e2e_nanogpt -- --ignored --nocapture
//! ```

use autoresearch_protocol::{CompetitionConfig, ResearcherRun, run_oneshot_competitive};
use autoresearch_runtime::reward::{RewardSchedule, total_wei};
use autoresearch_runtime::traits::Scorer;
use autoresearch_runtime::types::{Cadence, Gate, Knobs, ScorerKind, Split, Structure, Visibility};
use autoresearch_verticals::{FixedConfigEngine, NanoGptConfig, NanoGptScorer, NanoGptSurface};

const POOL_WEI: u128 = 1_000_000;
const SEEDS: u32 = 12; // == Gate::default().min_n
const BUDGET_ITERS: u32 = 300;

fn knobs() -> Knobs {
    Knobs {
        structure: Structure::Competitive,
        cadence: Cadence::OneShot,
        visibility: Visibility::Public,
        scorer_kind: ScorerKind::HeldOutEval,
    }
}

/// The five researcher hypotheses (real configs). `scale-tune` is the strong winner
/// (~2.25 val loss); `lr-tune` is a moderate improvement; the rest are worse than the
/// baseline (~2.41) and must be gated out — the market should pay only genuine lift.
fn researcher_config(name: &str) -> NanoGptConfig {
    let base = NanoGptConfig::baseline();
    match name {
        // scale up + good lr + longer warmup: converges well even at 300 iters.
        "scale-tune" => NanoGptConfig {
            learning_rate: 3e-3,
            n_layer: 5,
            n_embd: 192,
            warmup_iters: 50,
            ..base
        },
        // just tune the learning rate up — a smaller, real gain.
        "lr-tune" => NanoGptConfig {
            learning_rate: 3e-3,
            ..base
        },
        // wide but over-high lr: a plausible-but-worse hypothesis.
        "wide-highlr" => NanoGptConfig {
            learning_rate: 4e-3,
            n_embd: 192,
            ..base
        },
        // lr far too low: trains too slowly, clearly worse.
        "too-low-lr" => NanoGptConfig {
            learning_rate: 1e-4,
            ..base
        },
        // lr overshoots: worse than the baseline.
        "overshoot" => NanoGptConfig {
            learning_rate: 5e-3,
            ..base
        },
        _ => base,
    }
}

#[tokio::test]
#[ignore = "real nanoGPT training; needs python+torch+data, ~8 min CPU"]
async fn market_improves_nanogpt_val_loss_on_real_training() {
    let surface = NanoGptSurface;
    let scorer = NanoGptScorer::new(SEEDS, BUDGET_ITERS);
    let baseline = NanoGptConfig::baseline();

    let names = [
        "scale-tune",
        "lr-tune",
        "wide-highlr",
        "too-low-lr",
        "overshoot",
    ];
    let researchers: Vec<ResearcherRun> = names
        .iter()
        .enumerate()
        .map(|(i, n)| ResearcherRun {
            researcher: (*n).to_string(),
            seed: i as u64,
        })
        .collect();

    // Anchor: the baseline's held-out val loss (same seeds the orchestrator uses, so
    // absolute candidate losses recover exactly as baseline_loss - lift).
    let base_m = scorer
        .score(&baseline, Split::HeldOut)
        .await
        .expect("baseline scores");
    let baseline_loss = -base_m.value;
    println!(
        "\n=== nanoGPT auto-research market — {SEEDS} seeds, {BUDGET_ITERS}-iter budget ===\n\
         baseline (lr=1e-3, 4L/128d): val_loss = {baseline_loss:.4}  (CI ±{:.4}, n={})",
        (base_m.ci_upper - base_m.ci_lower) / 2.0,
        base_m.n,
    );

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
            FixedConfigEngine::new(run.researcher.clone(), researcher_config(&run.researcher))
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
            "  #{}  {name:<12} val_loss={cand_loss:.4}  lift={:.4} (ci_lower={:.4})  pay={pay}",
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

    // --- assertions: the market found a real improvement and refused the noise ---
    assert!(
        outcome.winners >= 1,
        "expected >=1 gate-clearing researcher"
    );
    let (top_name, top_lift) = &outcome.ranked[0];
    assert_eq!(top_name, "scale-tune", "scale-tune is the decisive winner");
    assert!(
        top_lift.delta > 0.10,
        "winner must beat baseline by a real margin, got {:.4}",
        top_lift.delta
    );
    assert!(
        top_lift.ci_lower >= cfg.gate.min_lift_ci_lower,
        "winner clears the gate CI floor"
    );
    assert!(
        baseline_loss - top_lift.delta < baseline_loss,
        "winner's val loss is below baseline"
    );
    // The clearly-bad hypotheses must NOT be paid.
    assert!(
        gated_out.contains(&"too-low-lr"),
        "too-low-lr must be gated out"
    );
    assert!(
        total_wei(&outcome.payouts) <= POOL_WEI,
        "payouts conserve the pool"
    );
}
