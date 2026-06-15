//! End-to-end proof that M1 works: a `Competitive x OneShot x Public x HeldOutEval`
//! competition runs fully in-process, produces a real positive held-out lift, gates
//! out the noise, ranks the survivors, and settles `SnapshotTopK` payouts that
//! conserve the reward pool.
//!
//! Every number here is measured on synthetic-but-real held-out data through the
//! same scorer the Referee would use — nothing is mocked or hardcoded.

use autoresearch_protocol::{CompetitionConfig, ResearcherRun, run_oneshot_competitive};
use autoresearch_runtime::reward::{RewardSchedule, total_wei};
use autoresearch_runtime::types::{Cadence, Gate, Knobs, ScorerKind, Structure, Visibility};
use autoresearch_verticals::{ConfigArtifact, ConfigSurface, LinearScorer, LocalSearchEngine};

const POOL_WEI: u128 = 1_000_000;

fn knobs() -> Knobs {
    Knobs {
        structure: Structure::Competitive,
        cadence: Cadence::OneShot,
        visibility: Visibility::Public,
        scorer_kind: ScorerKind::HeldOutEval,
    }
}

#[tokio::test]
async fn competitive_oneshot_produces_real_lift_and_conserving_payouts() {
    let surface = ConfigSurface;
    let scorer = LinearScorer::new();
    let baseline = ConfigArtifact::baseline();

    // Five researchers with distinct seeds => distinct, independently-good searches.
    let researchers: Vec<ResearcherRun> = (1u64..=5)
        .map(|seed| ResearcherRun {
            researcher: format!("0xresearcher{seed}"),
            seed,
        })
        .collect();

    let cfg = CompetitionConfig {
        id: 1,
        gate: Gate::default(),
        reward: RewardSchedule::SnapshotTopK {
            weights_bps: vec![5_000, 3_000, 2_000],
        },
        reward_pool_wei: POOL_WEI,
        knobs: knobs(),
    };

    let outcome =
        run_oneshot_competitive(&cfg, &surface, &scorer, &baseline, &researchers, |run| {
            LocalSearchEngine::new(run.seed)
        })
        .await
        .expect("competition should run");

    // 1. At least one candidate cleared the promotion gate.
    assert!(
        outcome.winners >= 1,
        "expected at least one gate-clearing candidate, got {}",
        outcome.winners
    );
    assert_eq!(outcome.winners, outcome.ranked.len());

    // 2. The top candidate is a large, real improvement on held-out data.
    //    Measured top delta is ~0.46 (held-out accuracy ~0.93 vs ~0.50 baseline);
    //    floor at 0.30 so a regression that halved the real lift fails the test,
    //    without being brittle to per-seed search variation.
    let top_delta = outcome.ranked[0].1.delta;
    assert!(
        top_delta > 0.30,
        "top lift delta should exceed 0.30, got {top_delta}"
    );

    // 3. Ranking is strictly sorted by delta, best first.
    for pair in outcome.ranked.windows(2) {
        assert!(
            pair[0].1.delta >= pair[1].1.delta,
            "ranking must be descending by delta: {:?}",
            outcome.ranked
        );
    }

    // 4. Every winner's lift lower bound clears the gate's minimum.
    for (researcher, lift) in &outcome.ranked {
        assert!(
            lift.ci_lower >= cfg.gate.min_lift_ci_lower,
            "{researcher} cleared ranking but not the gate: {lift:?}"
        );
        assert!(lift.n >= cfg.gate.min_n);
    }

    // 5. Payouts conserve the pool: never mint more than escrowed.
    let paid = total_wei(&outcome.payouts);
    assert!(paid <= POOL_WEI, "payouts {paid} exceeded pool {POOL_WEI}");

    // 6. With >= 3 winners the full pool is distributed (weights sum to 10_000 bps).
    if outcome.winners >= 3 {
        assert_eq!(
            paid, POOL_WEI,
            "with >=3 winners the SnapshotTopK pool must be fully distributed"
        );
    }

    // 7. The #1 researcher receives the largest payout.
    let top_researcher = &outcome.ranked[0].0;
    let top_payout = outcome
        .payouts
        .iter()
        .find(|p| &p.researcher == top_researcher)
        .expect("top researcher must be paid")
        .wei;
    let max_payout = outcome.payouts.iter().map(|p| p.wei).max().unwrap();
    assert_eq!(
        top_payout, max_payout,
        "the #1 researcher must receive the largest payout"
    );
}

/// A degenerate field of zero-weight baselines clears nothing: the gate refuses to
/// pay for noise, and the payout set is empty. This proves the gate is load-bearing,
/// not decorative.
#[tokio::test]
async fn no_improvement_yields_no_winners_and_no_payouts() {
    let surface = ConfigSurface;
    let scorer = LinearScorer::new();
    let baseline = ConfigArtifact::baseline();

    // An "engine" that just returns the baseline (zero search): no lift possible.
    struct NullEngine;
    impl autoresearch_runtime::traits::Engine for NullEngine {
        type Artifact = ConfigArtifact;
        fn id(&self) -> &str {
            "null"
        }
        fn produce(
            &self,
            _ctx: &autoresearch_runtime::traits::EngineContext,
        ) -> impl std::future::Future<
            Output = Result<Self::Artifact, autoresearch_runtime::traits::EngineError>,
        > + Send {
            std::future::ready(Ok(ConfigArtifact::baseline()))
        }
    }

    let researchers = vec![ResearcherRun {
        researcher: "0xidle".into(),
        seed: 1,
    }];

    let cfg = CompetitionConfig {
        id: 2,
        gate: Gate::default(),
        reward: RewardSchedule::SnapshotTopK {
            weights_bps: vec![10_000],
        },
        reward_pool_wei: POOL_WEI,
        knobs: knobs(),
    };

    let outcome =
        run_oneshot_competitive(&cfg, &surface, &scorer, &baseline, &researchers, |_run| {
            NullEngine
        })
        .await
        .expect("run should succeed even with no winners");

    assert_eq!(outcome.winners, 0, "baseline-vs-baseline is zero lift");
    assert!(outcome.payouts.is_empty(), "no winners => no payouts");
    assert_eq!(total_wei(&outcome.payouts), 0);
}

/// The gate must exclude a *positive-but-insufficient* lift, not merely a zero one.
/// This is the independent proof that the promotion gate bites: a weak engine produces
/// a candidate with a genuinely positive held-out improvement whose lift CI lower bound
/// (~0.0014, measured) is below the gate's `min_lift_ci_lower` (0.02). It must be absent
/// from `ranked` and unpaid, while a strong engine in the same field clears and is paid.
///
/// Unlike the baseline-vs-baseline degenerate case (delta == 0), this exercises the gate
/// on a candidate that genuinely beats the baseline by a real point estimate but lacks
/// the statistical separation the gate requires.
#[tokio::test]
async fn gate_excludes_positive_but_insufficient_lift() {
    let surface = ConfigSurface;
    let scorer = LinearScorer::new();
    let baseline = ConfigArtifact::baseline();

    // Two researchers: one weak (sub-gate), one strong (a real local search). Both are
    // driven through the uniform `MixedEngine` so a single `make_engine` closure suffices.
    let researchers = vec![
        ResearcherRun {
            researcher: "0xweak".into(),
            seed: 1,
        },
        ResearcherRun {
            researcher: "0xstrong".into(),
            seed: 7,
        },
    ];

    let cfg = CompetitionConfig {
        id: 3,
        gate: Gate::default(),
        reward: RewardSchedule::SnapshotTopK {
            weights_bps: vec![10_000],
        },
        reward_pool_wei: POOL_WEI,
        knobs: knobs(),
    };

    let outcome =
        run_oneshot_competitive(&cfg, &surface, &scorer, &baseline, &researchers, |run| {
            if run.researcher == "0xweak" {
                MixedEngine::Weak
            } else {
                MixedEngine::Strong(LocalSearchEngine::new(run.seed))
            }
        })
        .await
        .expect("run should succeed");

    // The strong researcher cleared; the weak one did not.
    let ranked_ids: Vec<&str> = outcome.ranked.iter().map(|(r, _)| r.as_str()).collect();
    assert!(
        ranked_ids.contains(&"0xstrong"),
        "strong researcher must clear the gate: {ranked_ids:?}"
    );
    assert!(
        !ranked_ids.contains(&"0xweak"),
        "weak (positive-but-insufficient) researcher must be excluded by the gate: {ranked_ids:?}"
    );

    // The weak researcher is never paid.
    assert!(
        !outcome.payouts.iter().any(|p| p.researcher == "0xweak"),
        "the gate-excluded researcher must receive no payout"
    );
}

/// A uniform engine type so the per-researcher dispatch above can return either a weak
/// fixed artifact (a single tiny non-zero weight → real positive lift, sub-gate CI) or a
/// real `LocalSearchEngine`, from one `make_engine` closure.
enum MixedEngine {
    Weak,
    Strong(LocalSearchEngine),
}

impl autoresearch_runtime::traits::Engine for MixedEngine {
    type Artifact = ConfigArtifact;
    fn id(&self) -> &str {
        match self {
            MixedEngine::Weak => "weak",
            MixedEngine::Strong(e) => e.id(),
        }
    }
    fn produce(
        &self,
        ctx: &autoresearch_runtime::traits::EngineContext,
    ) -> impl std::future::Future<
        Output = Result<Self::Artifact, autoresearch_runtime::traits::EngineError>,
    > + Send {
        // `WEAK_PARAMS` yields a measured held-out delta of ~0.15 with a CI lower bound
        // of ~0.0014 — a real positive lift that is nonetheless below the gate floor.
        const WEAK_PARAMS: [f64; 4] = [0.01, 0.0, 0.0, 0.0];
        // Resolve the strong arm's (ready) future eagerly so we can wrap a single
        // concrete artifact result in one ready future for both arms.
        let strong = match self {
            MixedEngine::Strong(e) => Some(e.produce(ctx)),
            MixedEngine::Weak => None,
        };
        async move {
            match strong {
                Some(fut) => fut.await,
                None => Ok(ConfigArtifact {
                    params: WEAK_PARAMS.to_vec(),
                }),
            }
        }
    }
}
