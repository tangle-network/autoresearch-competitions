//! End-to-end: the autoresearch market runs an **agent-profile** competition and pays
//! only for certified, GENERALIZING lift.
//!
//! Several researchers improve an agent profile against the SAME generic
//! [`GenericEngine`] — the deterministic stand-in for a recursive-self-improvement
//! loop. They differ ONLY by their start profile, their search budget, and their seed
//! — not by a different engine. Every engine maximizes the researcher-visible **dev**
//! task-suite pass-rate; the market's Referee then re-scores each produced profile on
//! a **held-out** task suite, gates it on a Wilson CI, ranks, and pays.
//!
//! The honest tension this proves: the dev signal rewards a profile that over-tunes to
//! the dev split (the `overfit` knob), so a researcher who over-searches the dev signal
//! drives that knob up, wins the dev number, and FAILS held-out — exactly the overfit
//! the held-out gate exists to catch. A researcher that improves real capability
//! (skills / prompt / tools / memory) from a low-overfit start clears the gate.
//!
//! Fully deterministic (no Node / model / GPU), so it runs in CI rather than being
//! `#[ignore]`d. It proves the market mechanism around a delegated agent eval:
//! held-out re-scoring of a submitted profile, the Wilson-CI gate refusing
//! plausible-but-overfit profiles, ranking, and conserved payouts.

use autoresearch_protocol::{CompetitionConfig, ResearcherRun, run_oneshot_competitive};
use autoresearch_runtime::reward::{RewardSchedule, total_wei};
use autoresearch_runtime::traits::Scorer;
use autoresearch_runtime::types::{Cadence, Gate, Knobs, ScorerKind, Split, Structure, Visibility};
use autoresearch_generic_engine::{GenericArtifact, GenericSurface, GenericEngine};
use autoresearch_verticals::agent_improvement::{
    AgentProfileScorer, baseline_profile, profile_from_knobs,
};

const POOL_WEI: u128 = 1_000_000;
const N_TASKS: u32 = 200; // >= Gate::default().min_n (12), real Wilson power

fn knobs() -> Knobs {
    Knobs {
        structure: Structure::Competitive,
        cadence: Cadence::OneShot,
        visibility: Visibility::Public,
        scorer_kind: ScorerKind::HeldOutEval,
    }
}

/// One researcher hypothesis: a start profile + a search budget. All four drive the
/// SAME `GenericEngine`; they differ only in where they start, how hard they search
/// the dev signal, and their seed.
struct Hypothesis {
    /// Raw (pre-squash) start knobs: skill, prompt, tool, memory, overfit.
    start: [f64; 5],
    /// Search budget (long-horizon improvement steps the engine takes on the dev signal).
    budget: usize,
}

/// The researcher field of play. The names describe the strategy each profile encodes.
fn hypothesis_for(name: &str) -> Hypothesis {
    match name {
        // Improves real capability from a clean, low-overfit start with a modest
        // budget: it raises skills/prompt/tools/memory and never over-tunes the dev
        // split. Generalizes -> clears the held-out gate.
        "balanced-skills" => Hypothesis {
            start: [1.2, 1.0, 0.8, 0.6, -3.0],
            budget: 60,
        },
        // A capability-first researcher starting slightly weaker, modest budget. Real
        // but smaller lift; still generalizes.
        "tool-specialist" => Hypothesis {
            start: [0.8, 0.6, 1.2, 0.4, -3.0],
            budget: 50,
        },
        // Over-searches the dev signal from a start already leaning on the overfit knob.
        // A large budget lets the dev-maximizing search drive overfit up hard: it wins
        // the dev number but collapses on held-out -> gated out.
        "dev-overfitter" => Hypothesis {
            start: [0.2, 0.1, 0.0, 0.0, 1.5],
            budget: 4000,
        },
        // Barely improves: a tiny budget from a weak start. No real held-out lift
        // over the baseline -> gated out.
        "under-trained" => Hypothesis {
            start: [-1.0, -1.0, -1.0, -1.0, -3.0],
            budget: 3,
        },
        _ => Hypothesis {
            start: [0.0, 0.0, 0.0, 0.0, 0.0],
            budget: 10,
        },
    }
}

fn start_artifact(name: &str) -> GenericArtifact {
    let h = hypothesis_for(name);
    profile_from_knobs(
        h.start[0],
        h.start[1],
        h.start[2],
        h.start[3],
        h.start[4],
        format!("agent profile: {name}"),
    )
}

#[tokio::test]
async fn market_improves_agent_on_heldout() {
    let surface = GenericSurface;
    let scorer = AgentProfileScorer::new(N_TASKS);

    // Baseline: the zero-knob starting agent, scored on held-out. A candidate must beat
    // THIS held-out pass-rate (by a gate-clearing CI margin) to certify lift and be paid.
    let baseline: GenericArtifact = baseline_profile();
    let base_m = scorer
        .score(&baseline, Split::HeldOut)
        .await
        .expect("baseline scores");
    println!(
        "\n=== agent self-improvement market (agent-profile stand-in) — {N_TASKS} tasks ===\n\
         baseline (zero-knob agent): held-out pass_rate = {:.4} \
         (Wilson CI [{:.4}, {:.4}], n={})",
        base_m.value, base_m.ci_lower, base_m.ci_upper, base_m.n,
    );

    let names = [
        "balanced-skills",
        "tool-specialist",
        "dev-overfitter",
        "under-trained",
    ];
    let researchers: Vec<ResearcherRun> = names
        .iter()
        .enumerate()
        .map(|(i, n)| ResearcherRun {
            researcher: (*n).to_string(),
            seed: (i as u64) + 1,
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

    // The SAME generic engine for every researcher. Researchers differ only by start
    // profile, search budget, and seed — never by a different engine. The engine
    // searches `params` to MAXIMISE the dev scorer (the researcher-visible signal).
    let outcome =
        run_oneshot_competitive(&cfg, &surface, &scorer, &baseline, &researchers, |run| {
            let h = hypothesis_for(&run.researcher);
            GenericEngine::new(
                run.researcher.clone(),
                start_artifact(&run.researcher),
                scorer, // dev scorer; the Referee re-scores produced artifacts on held-out
                run.seed,
            )
            .with_budget(h.budget)
            .with_step(0.5)
        })
        .await
        .expect("competition runs");

    // Report dev-vs-heldout for every researcher so the overfit gap is visible.
    println!("\nproduced-profile dev vs held-out pass-rates (the overfit gap):");
    for name in names {
        let h = hypothesis_for(name);
        let seed = (names.iter().position(|n| *n == name).unwrap() as u64) + 1;
        let engine = GenericEngine::new(name, start_artifact(name), scorer, seed)
            .with_budget(h.budget)
            .with_step(0.5);
        use autoresearch_runtime::traits::Engine;
        let ctx = engine_ctx();
        let produced = engine.produce(&ctx).await.unwrap();
        let dev = scorer.measure(&produced, Split::Dev).value;
        let held = scorer.measure(&produced, Split::HeldOut).value;
        println!(
            "  {name:<16} dev={dev:.4}  held-out={held:.4}  gap={:+.4}",
            dev - held
        );
    }

    println!("\nleaderboard (gate-clearing only, best held-out lift first):");
    for (rank, (name, lift)) in outcome.ranked.iter().enumerate() {
        let pay = outcome
            .payouts
            .iter()
            .find(|p| &p.researcher == name)
            .map_or(0, |p| p.wei);
        println!(
            "  #{}  {name:<16} lift={:.4} (ci_lower={:.4})  pay={pay}",
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
    println!("gated out (no certified held-out lift): {gated_out:?}");
    println!(
        "pool {POOL_WEI} -> paid {} across {} winners\n",
        total_wei(&outcome.payouts),
        outcome.winners
    );

    // --- assertions: the market certified GENERALIZING lift and refused the overfit --
    assert!(
        outcome.winners >= 1,
        "expected >=1 gate-clearing (generalizing) researcher"
    );

    // The overfitter over-searched the dev signal and must NOT be paid: it fails the
    // held-out gate despite (or because of) its inflated dev number.
    assert!(
        gated_out.contains(&"dev-overfitter"),
        "dev-overfitter over-tuned the dev split and must be gated out on held-out"
    );
    // The barely-trained researcher has no real lift and must be gated out too.
    assert!(
        gated_out.contains(&"under-trained"),
        "under-trained has no certified lift and must be gated out"
    );

    // A generalizing researcher must win and clear the gate CI floor.
    let (top_name, top_lift) = &outcome.ranked[0];
    assert!(
        *top_name == "balanced-skills" || *top_name == "tool-specialist",
        "a generalizing capability profile must win, got {top_name}"
    );
    assert!(
        top_lift.ci_lower >= cfg.gate.min_lift_ci_lower,
        "winner clears the gate CI floor ({} >= {})",
        top_lift.ci_lower,
        cfg.gate.min_lift_ci_lower
    );
    assert!(
        top_lift.delta > 0.0,
        "winner must show a positive held-out pass-rate lift, got {}",
        top_lift.delta
    );

    // Payouts conserve the pool.
    assert!(
        total_wei(&outcome.payouts) <= POOL_WEI,
        "payouts must conserve the reward pool"
    );
}

/// A minimal engine context for the dev-vs-heldout reporting drive above (the
/// orchestrator builds its own internally for the scored run).
fn engine_ctx() -> autoresearch_runtime::traits::EngineContext {
    use autoresearch_runtime::types::ArtifactRef;
    autoresearch_runtime::traits::EngineContext {
        competition: 1,
        baseline_ref: ArtifactRef("base".into()),
        dev_split_ref: None,
        budget_wei: 0,
        egress_policy: None,
    }
}
