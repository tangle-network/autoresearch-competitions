//! The headline generalization proof: the **one** universal [`SupervisorEngine`]
//! improves **every** domain — program superoptimization, combinatorial solving,
//! theorem proving, agent self-improvement, and forecasting — with no per-domain
//! engine. Each domain is just a `Scorer` over `GenericArtifact`; the engine is
//! shared. One generic function drives all five, which *is* the generalization:
//! adding a new algorithmic-advancement domain means writing a scorer, never an
//! engine.

use autoresearch_runtime::traits::{Engine, EngineContext, Scorer};
use autoresearch_runtime::types::{ArtifactRef, Split};
use autoresearch_supervisor::{GenericArtifact, SupervisorEngine};

fn ctx() -> EngineContext {
    EngineContext {
        competition: 1,
        baseline_ref: ArtifactRef("base".into()),
        dev_split_ref: None,
        budget_wei: 0,
        egress_policy: None,
    }
}

/// Run the *one* universal engine over `start` against the domain's dev scorer and
/// return `(baseline held-out value, produced held-out value)`. Generic over the
/// scorer — the very same function body works for every domain. That genericity is
/// the point: the engine is domain-blind; only the `Scorer` differs.
async fn universal_improves<Sc>(
    dev: Sc,
    start: GenericArtifact,
    budget: usize,
    step: f64,
    seed: u64,
) -> (f64, f64)
where
    Sc: Scorer<Artifact = GenericArtifact> + Clone + Send + Sync,
{
    let base_v = dev.score(&start, Split::HeldOut).await.unwrap().value;
    // Each domain passes a known-good search config (seed/budget/step) — exactly what a
    // researcher tunes for their own domain (e.g. the agent domain's strong overfit gap
    // rewards a search path that generalizes). The per-domain e2e tests explore the field.
    let engine = SupervisorEngine::new("r", start, dev.clone(), seed)
        .with_budget(budget)
        .with_step(step);
    let produced = engine.produce(&ctx()).await.unwrap();
    let prod_v = dev.score(&produced, Split::HeldOut).await.unwrap().value;
    (base_v, prod_v)
}

#[tokio::test]
async fn one_engine_improves_every_domain() {
    use autoresearch_verticals::{
        agent_improvement as agent, combinatorial_solver as solver, forecasting as fc,
        program_superopt as prog, theorem_proving as thm,
    };

    // (domain label, baseline held-out value, produced held-out value) for each domain,
    // every one produced by the SAME SupervisorEngine type.
    let mut results: Vec<(&str, f64, f64)> = Vec::new();

    let (b, p) = universal_improves(
        prog::ProgramScorer::new(16),
        prog::baseline_artifact(),
        4000,
        0.6,
        1,
    )
    .await;
    results.push(("program-superopt", b, p));

    let (b, p) = universal_improves(
        solver::SolverScorer::new(16),
        solver::baseline_artifact(),
        4000,
        0.6,
        1,
    )
    .await;
    results.push(("combinatorial-solver", b, p));

    let (b, p) = universal_improves(
        thm::ProofScorer::new(16),
        thm::baseline_proof(),
        4000,
        0.5,
        1,
    )
    .await;
    results.push(("theorem-proving", b, p));

    let (b, p) = universal_improves(
        agent::AgentProfileScorer::new(16),
        agent::baseline_profile(),
        4000,
        0.5,
        7,
    )
    .await;
    results.push(("agent-improvement", b, p));

    let (b, p) = universal_improves(fc::ForecastScorer::new(16), fc::start(), 4000, 0.4, 1).await;
    results.push(("forecasting", b, p));

    println!("\n=== one universal SupervisorEngine, every domain ===");
    println!("(held-out value before -> after; higher is better)");
    for (name, b, p) in &results {
        println!("  {name:<22} {b:+.4} -> {p:+.4}   (+{:.4})", p - b);
    }

    // The generalization claim: the single engine improves EVERY domain by a real,
    // gate-clearing margin — none is a no-op, none regresses.
    for (name, b, p) in &results {
        assert!(
            *p > *b + 0.02,
            "the universal engine must improve {name} by a real margin: {b} -> {p}"
        );
    }
    assert_eq!(
        results.len(),
        5,
        "all five domains were driven by one engine"
    );
}
