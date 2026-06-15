//! Scenario A proof — the **Private Oracle (quantum) case**.
//!
//! This is the M5 end-to-end proof of the "open network beat the withheld benchmark"
//! pattern, the EigenCloud withheld-quantum-benchmark shape: a competition whose
//! referee is a HIDDEN-reference oracle, where researchers run a black-box optimizer
//! under a query budget and improve over the baseline using ONLY bounded scalar
//! queries (solve-hard / verify-easy). The researchers never see the hidden reference,
//! and cannot recover it in closed form from the scores they receive: the score channel
//! is deterministically perturbed per-artifact so it is non-invertible (the `D + 1`-query
//! closed-form solve that breaks a bare closeness is defeated — proven at the unit level
//! in `scorers::tests::oracle_score_channel_is_not_invertible_to_the_secret`). They learn
//! only perturbed scalar closeness scores on artifacts THEY submitted.
//!
//! The test asserts the four properties that make Scenario A work AND honest:
//!
//! 1. **Researchers improve over the baseline via bounded queries.** The leaderboard
//!    climbs: a black-box optimizer that has never seen the hidden reference reaches a
//!    high closeness through scalar queries alone, clearing the promotion gate over the
//!    origin baseline. This is the "beat the withheld benchmark" climb.
//! 2. **The winner is paid; payouts conserve the pool.** The hidden-oracle adjudication
//!    settles a conserving payout to the best researcher — the verifiable market works
//!    even when the referee is a secret oracle.
//! 3. **The hidden reference is NEVER exposed to researchers.** There is no path from
//!    researcher-visible data (the surface, the artifact ref, the feedback) to the
//!    secret — only scalar scores. The oracle has no accessor and no Debug/Serialize
//!    for its secret, and its dev split reveals nothing the held-out split does not.
//! 4. **The query budget is enforced.** An engine that would exceed the oracle's
//!    submission budget is cut off — unbounded probing of the score channel is
//!    prevented (PRIVACY §8: the budget bounds, not eliminates, the leak).
//!
//! # Honesty
//!
//! The oracle here is a deterministic LOCAL stand-in for a real private oracle (a
//! quantum device's figure-of-merit, a withheld benchmark backend). It does not run
//! real quantum hardware; what it faithfully models is the INTERFACE — solve-hard /
//! verify-easy through bounded scalar queries against a reference the researcher cannot
//! see — and that interface is what the whole Scenario A claim rests on. The climb and
//! every payout are real (measured through the same scorer the referee would use),
//! reproducible, and not hardcoded.

use autoresearch_protocol::{CompetitionConfig, ResearcherRun, run_oneshot_competitive};
use autoresearch_runtime::privacy::SubmissionBudget;
use autoresearch_runtime::reward::{RewardSchedule, total_wei};
use autoresearch_runtime::traits::{Engine, EngineContext, Scorer, Surface};
use autoresearch_runtime::types::{
    ArtifactRef, Cadence, Gate, Knobs, ScorerKind, Split, Structure, Visibility,
};
use autoresearch_verticals::{BlackBoxOptimizerEngine, HiddenTargetSurface, PrivateOracleScorer};

const POOL_WEI: u128 = 1_000_000;
/// Per-researcher query budget the black-box optimizer spends climbing toward the
/// hidden optimum. Generous enough to climb high, bounded so probing is finite.
const QUERY_BUDGET: u32 = 400;
/// The hidden reference's secret seed. The researcher never sees this and cannot
/// recover it from anything they receive.
const SECRET_SEED: u64 = 0xC0FF_EE15_900D;

/// Private-oracle (quantum) knobs: `Competitive × OneShot × Public × PrivateOracle`.
fn oracle_knobs() -> Knobs {
    Knobs {
        structure: Structure::Competitive,
        cadence: Cadence::OneShot,
        visibility: Visibility::Public,
        scorer_kind: ScorerKind::PrivateOracle,
    }
}

#[tokio::test]
async fn scenario_a_open_network_beats_the_withheld_oracle_and_winner_is_paid() {
    let surface = HiddenTargetSurface;
    // The oracle holds a HIDDEN reference, synthesized from SECRET_SEED. Researchers
    // never receive the seed or the reference — only scalar scores on what they submit.
    // An unbounded budget here so the runner's baseline/candidate certification plus
    // each engine's climb all draw freely; the budget-enforcement property is proven in
    // its own focused test below.
    let oracle = PrivateOracleScorer::new(SECRET_SEED, None);

    // The baseline researchers must beat: the origin, maximally uninformed about the
    // hidden target (the analogue of the all-zeros config-opt baseline). Its certified
    // closeness is the bar — the "withheld benchmark" value the open network must beat.
    let baseline = HiddenTargetSurface::origin();
    let baseline_score = oracle.score(&baseline, Split::HeldOut).await.unwrap().value;

    // Five researchers with distinct seeds => distinct, independently-good black-box
    // searches against the SAME hidden oracle.
    let researchers: Vec<ResearcherRun> = (1u64..=5)
        .map(|seed| ResearcherRun {
            researcher: format!("0xresearcher{seed}"),
            seed,
        })
        .collect();

    let cfg = CompetitionConfig {
        id: 7,
        gate: Gate::default(),
        reward: RewardSchedule::SnapshotTopK {
            weights_bps: vec![6_000, 3_000, 1_000],
        },
        reward_pool_wei: POOL_WEI,
        knobs: oracle_knobs(),
    };
    // The declared ScorerKind matches the scorer actually adjudicating — the property
    // the on-chain `scorerKind` records.
    assert_eq!(cfg.knobs.scorer_kind, ScorerKind::PrivateOracle);

    // Each researcher's engine borrows the SAME hidden oracle and improves ONLY through
    // bounded scalar queries to it — no surface gradient, no dev exemplars, no sight of
    // the secret.
    let outcome =
        run_oneshot_competitive(&cfg, &surface, &oracle, &baseline, &researchers, |run| {
            BlackBoxOptimizerEngine::new(run.seed, QUERY_BUDGET, &oracle)
        })
        .await
        .expect("private-oracle competition should run");

    // --- 1. The open network beat the withheld oracle (the climb) -----------
    assert!(
        outcome.winners >= 1,
        "at least one black-box researcher must clear the gate over the withheld baseline"
    );
    assert_eq!(outcome.winners, outcome.ranked.len());

    // The top candidate is a large, real improvement measured through the hidden oracle
    // — the researcher climbed via (perturbed) scalar queries alone from the origin
    // baseline to a point much closer to the hidden target. (Origin perturbed closeness
    // is ~0.59; the climb reaches ~1.0, a measured ~+0.41 lift — the "beat the withheld
    // benchmark" climb, real under the non-invertible score channel.)
    let top_delta = outcome.ranked[0].1.delta;
    assert!(
        top_delta > 0.30,
        "the open network must beat the withheld oracle by a real margin, got {top_delta}"
    );

    // Ranking is descending by delta, best first.
    for pair in outcome.ranked.windows(2) {
        assert!(
            pair[0].1.delta >= pair[1].1.delta,
            "ranking must be descending by delta: {:?}",
            outcome.ranked
        );
    }
    // Every winner's lift lower bound clears the gate's minimum, on real n.
    for (researcher, lift) in &outcome.ranked {
        assert!(
            lift.ci_lower >= cfg.gate.min_lift_ci_lower,
            "{researcher} ranked but did not clear the gate: {lift:?}"
        );
        assert!(lift.n >= cfg.gate.min_n);
    }

    // --- 2. The winner is paid; payouts conserve the pool -------------------
    let paid = total_wei(&outcome.payouts);
    assert!(paid <= POOL_WEI, "payouts {paid} exceeded pool {POOL_WEI}");
    if outcome.winners >= 3 {
        assert_eq!(
            paid, POOL_WEI,
            "with >=3 winners the SnapshotTopK pool is fully distributed"
        );
    }
    let top_researcher = &outcome.ranked[0].0;
    let top_payout = outcome
        .payouts
        .iter()
        .find(|p| &p.researcher == top_researcher)
        .expect("the top researcher must be paid")
        .wei;
    let max_payout = outcome.payouts.iter().map(|p| p.wei).max().unwrap();
    assert_eq!(
        top_payout, max_payout,
        "the #1 researcher receives the largest payout"
    );

    // --- 3. The hidden reference is NEVER exposed to researchers ------------
    // The only researcher-visible artifacts are the surface, the submitted artifact,
    // and its ref. None reveals the secret.
    //
    // (a) The surface id and a submitted artifact's ref carry no reference component.
    let winner_artifact = BlackBoxOptimizerEngine::new(1, QUERY_BUDGET, &oracle)
        .produce(&EngineContext {
            competition: cfg.id,
            baseline_ref: ArtifactRef("baseline".into()),
            dev_split_ref: None,
            budget_wei: 0,
            egress_policy: None,
        })
        .await
        .unwrap();
    let r = surface.to_ref(&winner_artifact).unwrap();
    assert!(
        r.0.starts_with("hidden-target:") && !r.0.contains("secret"),
        "the artifact ref is an opaque hash of the SUBMITTED artifact, not the secret: {}",
        r.0
    );

    // (b) The oracle's dev split reveals nothing the held-out split does not: scoring
    //     the same artifact on Dev and HeldOut yields the identical scalar, so a
    //     researcher cannot mine a "training" split for the reference.
    let dev = oracle
        .score(&winner_artifact, Split::Dev)
        .await
        .unwrap()
        .value;
    let held = oracle
        .score(&winner_artifact, Split::HeldOut)
        .await
        .unwrap()
        .value;
    assert_eq!(
        dev, held,
        "the oracle must expose no dev split that reveals more about the secret"
    );

    // (c) Two oracles with DIFFERENT secrets are indistinguishable to a researcher
    //     except through the scalar scores they return — there is no accessor or
    //     serialization that could tell them apart. (The struct derives no Debug and no
    //     Serialize, so even `format!("{:?}")` / `serde_json::to_string` of the oracle
    //     do not compile — the secret has no escape path. This is asserted at the unit
    //     level in `scorers::tests`; here we assert the observable consequence.)
    let other_oracle = PrivateOracleScorer::new(SECRET_SEED ^ 0xFFFF, None);
    let s_here = oracle
        .score(&winner_artifact, Split::HeldOut)
        .await
        .unwrap()
        .value;
    let s_other = other_oracle
        .score(&winner_artifact, Split::HeldOut)
        .await
        .unwrap()
        .value;
    assert_ne!(
        s_here, s_other,
        "different hidden references are observable ONLY through their scalar scores"
    );

    // The baseline (origin) was genuinely uninformed: the climb is real, not a baseline
    // already near the target.
    assert!(
        baseline_score < 0.75,
        "the withheld baseline must be a real bar to beat, not pre-solved: {baseline_score}"
    );
    assert!(
        top_delta > 0.30 && (baseline_score + top_delta) > 0.80,
        "the winner climbed from an uninformed baseline to near the hidden optimum"
    );
}

/// Property 4 (focused): the **query budget is enforced** — an engine that would
/// exceed the oracle's submission budget is cut off, preventing unbounded probing of
/// the score channel (PRIVACY §8).
#[tokio::test]
async fn scenario_a_query_budget_cuts_off_an_over_querying_engine() {
    // The oracle's submission budget is far smaller than the engine's requested query
    // budget. The engine must stop when the oracle refuses — it does not error the run,
    // it returns its best-so-far, and the oracle's budget is exactly and fully consumed.
    let oracle_budget = 30u32;
    let oracle = PrivateOracleScorer::new(SECRET_SEED, Some(SubmissionBudget::new(oracle_budget)));
    assert_eq!(oracle.remaining_queries(), Some(oracle_budget));

    // The engine asks for 10x the oracle's budget; the oracle is the binding limit.
    let engine = BlackBoxOptimizerEngine::new(1, oracle_budget * 10, &oracle);
    let ctx = EngineContext {
        competition: 7,
        baseline_ref: ArtifactRef("baseline".into()),
        dev_split_ref: None,
        budget_wei: 0,
        egress_policy: None,
    };
    let candidate = engine
        .produce(&ctx)
        .await
        .expect("an over-querying engine must be cut off gracefully, not error the run");

    // The oracle's probe budget was EXACTLY consumed — the over-querying engine was cut
    // off at the bound; unbounded probing is impossible.
    assert_eq!(
        oracle.remaining_queries(),
        Some(0),
        "the query budget must be fully consumed and not exceeded"
    );

    // A SECOND engine against the now-exhausted oracle cannot probe at all: its first
    // probe is refused, so it returns the origin unchanged. This is the rate-limit
    // biting — probing past the bound is impossible.
    let starved = BlackBoxOptimizerEngine::new(2, oracle_budget * 10, &oracle)
        .produce(&ctx)
        .await
        .expect("a starved engine returns its best-so-far (the origin), it does not error");
    assert_eq!(
        starved,
        HiddenTargetSurface::origin(),
        "an engine with no remaining probe budget cannot improve past the origin"
    );
    assert_eq!(
        oracle.remaining_queries(),
        Some(0),
        "a starved engine consumes nothing further"
    );

    // The referee CERTIFICATION path is decoupled from the probe budget: scoring still
    // works after the probe budget is exhausted (the referee measures, it does not probe).
    let certified = oracle
        .score(&HiddenTargetSurface::origin(), Split::HeldOut)
        .await;
    assert!(
        certified.is_ok(),
        "referee certification must not be blocked by an exhausted PROBE budget: {certified:?}"
    );

    // The candidate the cut-off engine returned is still valid and finite.
    assert!(candidate.params.iter().all(|p| p.is_finite()));
}

/// Determinism: the whole Scenario A flow is reproducible. The same secret seed and the
/// same researcher seeds produce byte-identical winning artifacts and lift, which is
/// what lets the leaderboard be recomputed and disputed.
#[tokio::test]
async fn scenario_a_is_fully_deterministic() {
    let run = || async {
        let oracle = PrivateOracleScorer::new(SECRET_SEED, None);
        let engine = BlackBoxOptimizerEngine::new(3, QUERY_BUDGET, &oracle);
        let ctx = EngineContext {
            competition: 7,
            baseline_ref: ArtifactRef("b".into()),
            dev_split_ref: None,
            budget_wei: 0,
            egress_policy: None,
        };
        let cand = engine.produce(&ctx).await.unwrap();
        let score = oracle.score(&cand, Split::HeldOut).await.unwrap().value;
        (cand, score)
    };
    let (a_cand, a_score) = run().await;
    let (b_cand, b_score) = run().await;
    assert_eq!(a_cand, b_cand, "same seeds => identical winning artifact");
    assert_eq!(a_score, b_score, "same seeds => identical certified score");
}

/// Findings 2 & 3 composed: the FULL `run_oneshot_competitive` with a PER-RESEARCHER
/// bounded probe budget and the multi-researcher field. This is the combination the
/// suite previously never exercised together (the climb was proven with budget=None, the
/// budget with no runner). It asserts the milestone's core claim holds under its own
/// privacy bound:
///
/// - every researcher gets its OWN full probe budget (budgets are not shared across
///   researchers, and the referee's certification does not draw from them);
/// - the run COMPLETES — a bounded oracle no longer aborts the competition mid-run;
/// - at least one researcher still clears the gate via bounded queries and is paid.
#[tokio::test]
async fn scenario_a_runner_with_per_researcher_budget_completes_and_pays() {
    let surface = HiddenTargetSurface;

    // Per-researcher probe budget: each researcher's engine gets its OWN bounded oracle
    // (same hidden secret, independent budget Cell). Generous enough to climb, bounded so
    // probing is finite. Because the budgets are per-researcher, the field does not drain
    // a shared counter (the conflation Finding 2 found).
    let per_researcher_budget = 200u32;
    let n_researchers = 5u64;
    let probe_oracles: Vec<PrivateOracleScorer> = (1..=n_researchers)
        .map(|_| {
            PrivateOracleScorer::new(
                SECRET_SEED,
                Some(SubmissionBudget::new(per_researcher_budget)),
            )
        })
        .collect();

    // The referee certifies through a SEPARATE, unbudgeted oracle on the same secret:
    // certification is the referee measuring, never the researcher probing, so it must
    // not consume any researcher's probe budget.
    let certifier = PrivateOracleScorer::new(SECRET_SEED, None);

    let baseline = HiddenTargetSurface::origin();
    let baseline_score = certifier
        .score(&baseline, Split::HeldOut)
        .await
        .unwrap()
        .value;

    let researchers: Vec<ResearcherRun> = (1..=n_researchers)
        .map(|seed| ResearcherRun {
            researcher: format!("0xresearcher{seed}"),
            seed,
        })
        .collect();

    let cfg = CompetitionConfig {
        id: 11,
        gate: Gate::default(),
        reward: RewardSchedule::SnapshotTopK {
            weights_bps: vec![6_000, 3_000, 1_000],
        },
        reward_pool_wei: POOL_WEI,
        knobs: oracle_knobs(),
    };

    // Each researcher's engine probes its OWN bounded oracle (indexed by seed); the
    // runner certifies through the separate unbudgeted `certifier`. The engine's request
    // is larger than its budget, so the per-researcher bound is the binding limit.
    let outcome =
        run_oneshot_competitive(&cfg, &surface, &certifier, &baseline, &researchers, |run| {
            let oracle = &probe_oracles[(run.seed - 1) as usize];
            BlackBoxOptimizerEngine::new(run.seed, per_researcher_budget * 4, oracle)
        })
        .await
        .expect("a bounded per-researcher oracle must NOT abort the competition mid-run");

    // Every researcher consumed its OWN full budget — budgets are per-researcher, not a
    // shared global counter, and certification drew from none of them.
    for (i, oracle) in probe_oracles.iter().enumerate() {
        assert_eq!(
            oracle.remaining_queries(),
            Some(0),
            "researcher {} must have spent its full per-researcher probe budget",
            i + 1
        );
    }

    // At least one researcher cleared the gate via bounded queries and is paid.
    assert!(
        outcome.winners >= 1,
        "a bounded-budget researcher must still clear the gate over the withheld baseline"
    );
    assert_eq!(outcome.winners, outcome.ranked.len());
    let top_delta = outcome.ranked[0].1.delta;
    assert!(
        top_delta > 0.30,
        "the open network beats the withheld oracle under a real per-researcher bound, got {top_delta}"
    );
    for (researcher, lift) in &outcome.ranked {
        assert!(
            lift.ci_lower >= cfg.gate.min_lift_ci_lower,
            "{researcher} ranked but did not clear the gate: {lift:?}"
        );
        assert!(lift.n >= cfg.gate.min_n);
    }

    // The winner is paid and payouts conserve the pool.
    let paid = total_wei(&outcome.payouts);
    assert!(paid <= POOL_WEI, "payouts {paid} exceeded pool {POOL_WEI}");
    assert!(
        baseline_score < 0.75 && (baseline_score + top_delta) > 0.80,
        "climb is real from an uninformed baseline under the budget: base={baseline_score} delta={top_delta}"
    );
}
