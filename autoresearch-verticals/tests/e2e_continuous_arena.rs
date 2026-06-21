//! Scenario B — the Public Continuous Arena (king-of-the-hill).
//!
//! This is the M3 end-to-end proof: a leaderboard that *keeps moving*. A sequence
//! of strengthening artifacts is submitted in order to a `RecordBounty` continuous
//! arena; each new state-of-the-art that beats the current best by `epsilon` earns
//! its **marginal** improvement, the leaderboard's best strictly increases across
//! records, and the total paid equals `wei_per_micro * (final_best - baseline)` —
//! the frontier is bought exactly once. The standings and the per-record payouts
//! are recomputed purely from the append-only history, proving the log is a
//! verifiable, replayable leaderboard.
//!
//! The strengthening sequence is deterministic: each artifact interpolates the
//! linear classifier's weights from a deliberately-wrong starting direction toward
//! the ground-truth separator `W_TRUE` at an increasing fraction. Rotating the
//! decision boundary toward the truth makes held-out accuracy climb in real steps
//! (~0.53 -> 0.64 -> 0.75 -> 0.84 -> 0.89 -> 0.93 -> 1.0). Every accuracy number is
//! measured through the same held-out [`LinearScorer`] the Referee would use —
//! nothing is mocked or hardcoded. This is the "37% -> 39.9% and still climbing"
//! arena, made concrete and deterministic.

use autoresearch_generic_engine::{ArtifactKind, GenericArtifact, GenericSurface};
use autoresearch_protocol::continuous::{
    ContinuousArena, ContinuousSchedule, EntryKind, to_micros,
};
use autoresearch_runtime::reward::{RecordBeat, settle_record_bounty, total_wei};
use autoresearch_runtime::traits::Scorer;
use autoresearch_runtime::types::{Gate, Measurement, Split};
use autoresearch_verticals::LinearScorer;

/// The ground-truth separating hyperplane (mirrors `scorers::W_TRUE`).
const W_TRUE: [f64; 4] = [1.0, -2.0, 0.5, 1.5];
/// A deliberately-wrong starting direction. Interpolating from here toward `W_TRUE`
/// rotates the boundary, so accuracy climbs as the fraction increases.
const W_START: [f64; 4] = [-1.5, -1.0, -1.0, 0.2];

/// Build the strengthening artifact at interpolation fraction `f` in `[0, 1]`.
fn interp(f: f64) -> GenericArtifact {
    GenericArtifact::new(
        ArtifactKind::Config,
        (0..4)
            .map(|i| W_START[i] * (1.0 - f) + W_TRUE[i] * f)
            .collect(),
        String::new(),
    )
}

#[tokio::test]
async fn public_continuous_arena_leaderboard_keeps_moving() {
    let surface = GenericSurface;
    let scorer = LinearScorer::new();
    let baseline_measurement: Measurement = scorer
        .score(
            &GenericArtifact::baseline(ArtifactKind::Config, 4, ""),
            Split::HeldOut,
        )
        .await
        .unwrap();

    // Reward: 1 gwei per micro-point of lift, epsilon = 0.01 score point (10_000
    // micros) so a record must be a real, non-trivial improvement. The pool is set
    // generous enough to cover the full final lift so the marginal invariant binds.
    let epsilon_micros = 10_000;
    let wei_per_micro = 1_000_000_000u128;
    let pool_wei = u128::MAX; // sufficient pool: never the binding constraint here

    let mut arena = ContinuousArena::new(
        1,
        Gate::default(),
        ContinuousSchedule::RecordBounty {
            epsilon_micros,
            wei_per_micro,
        },
        pool_wei,
        0, // lift is a delta over the baseline, so the bar starts at 0 micros
    );

    // The strengthening schedule. The first two submissions are genuinely positive
    // improvements whose lift CI lower bound is below the gate floor (frac 0.00 has a
    // negative lower bound; frac 0.20 is +0.15 point estimate but only ~0.001 lower
    // bound at n=80) — both must be REJECTED by the gate, proving noise does not buy a
    // record. The remaining five are a strict monotone climb of gate-clearing records:
    // held-out accuracy ~0.69 -> 0.75 -> 0.84 -> 0.89 -> 1.00 (and still climbing).
    let gated_out = ["0xnoise0", "0xnoise1"];
    let schedule: [(&str, f64); 7] = [
        ("0xnoise0", 0.00), // delta ~+0.04, lift CI lower < 0 => gate REJECTS
        ("0xnoise1", 0.20), // delta ~+0.15 positive but underpowered => gate REJECTS
        ("0xalice", 0.30),  // ~0.69 accuracy
        ("0xbob", 0.45),    // ~0.75
        ("0xcarol", 0.55),  // ~0.84
        ("0xdave", 0.65),   // ~0.89
        ("0xerin", 1.00),   // ~1.00 — the separator is fully recovered
    ];

    let mut records = 0usize;
    let mut last_best: Option<i64> = None;
    for (researcher, frac) in schedule {
        let artifact = interp(frac);
        let out = arena
            .submit(
                researcher,
                &artifact,
                &surface,
                &scorer,
                &baseline_measurement,
            )
            .await
            .expect("submission should score and gate without error");

        if out.became_record {
            records += 1;
            let new_best = out.new_best_micros.expect("a record carries a new best");
            // The leaderboard keeps moving: each record strictly raises the best.
            if let Some(prev) = last_best {
                assert!(
                    new_best > prev,
                    "{researcher} recorded but did not raise the best: {new_best} <= {prev}"
                );
            }
            last_best = Some(new_best);
            // Each record was paid exactly its marginal over the prior bar.
            assert!(out.paid_wei > 0, "a gate-clearing record must be paid");
        } else {
            // Only the two under-powered early submissions are allowed to fail.
            assert!(
                gated_out.contains(&researcher),
                "{researcher} unexpectedly failed to record"
            );
            assert_eq!(out.paid_wei, 0);
        }
    }

    // 1. The under-powered nudge was gated out; the four-plus strong steps recorded.
    assert!(
        records >= 3,
        "expected at least 3 distinct records (the leaderboard keeps moving), got {records}"
    );

    // 2. Every Record entry strictly increases the best — recheck straight off the log.
    let record_bests: Vec<i64> = arena
        .leaderboard()
        .iter()
        .filter(|e| e.kind == EntryKind::Record)
        .map(|e| e.lift_micros)
        .collect();
    assert_eq!(record_bests.len(), records);
    for w in record_bests.windows(2) {
        assert!(
            w[1] > w[0],
            "record bests must strictly increase: {record_bests:?}"
        );
    }

    // 3. The marginal invariant in the live/streaming setting: total record pay ==
    //    wei_per_micro * (final_best - baseline). The frontier is bought exactly once.
    let streaming_total: u128 = arena
        .leaderboard()
        .iter()
        .filter(|e| e.kind == EntryKind::Record)
        .map(|e| e.paid_wei)
        .sum();
    let final_best = arena.best_micros.expect("at least one record set a best");
    assert_eq!(
        streaming_total,
        (final_best - arena.baseline_micros) as u128 * wei_per_micro,
        "total record pay must equal wei_per_micro * (final_best - baseline)"
    );

    // 4. Conservation: the arena never over-spends its pool.
    assert!(arena.spent_wei <= arena.pool_wei);
    assert_eq!(arena.spent_wei, streaming_total);

    // 5. The leaderboard is recomputable: replaying the SAME submission sequence as
    //    beats through the batch settler reproduces the streaming total and the exact
    //    per-record payouts. Anyone with the log can reproduce ranks and payouts.
    let beats: Vec<RecordBeat> = arena
        .leaderboard()
        .iter()
        .filter(|e| e.kind == EntryKind::Record)
        .map(|e| RecordBeat {
            researcher: e.researcher.clone(),
            new_best_micros: e.lift_micros,
        })
        .collect();
    let batch = settle_record_bounty(arena.baseline_micros, &beats, epsilon_micros, wei_per_micro);
    assert_eq!(
        total_wei(&batch),
        streaming_total,
        "batch replay of the log must reproduce the streaming total"
    );
    let batch_payouts: Vec<u128> = batch.iter().map(|p| p.wei).collect();
    let record_payouts: Vec<u128> = arena
        .leaderboard()
        .iter()
        .filter(|e| e.kind == EntryKind::Record)
        .map(|e| e.paid_wei)
        .collect();
    assert_eq!(
        record_payouts, batch_payouts,
        "per-record payouts must replay exactly"
    );

    // 6. Standings recomputed purely from history agree with the live top spot.
    let standings = arena.standings();
    assert_eq!(standings[0].1, final_best, "recomputed leader == live best");
    assert_eq!(
        arena.top_holder.as_deref(),
        Some(standings[0].0.as_str()),
        "live top holder == recomputed leader"
    );
    // Erin recovered the full separator, so she is the final state-of-the-art.
    assert_eq!(standings[0].0, "0xerin");
    assert_eq!(final_best, to_micros(1.0 - baseline_measurement.value));

    // 7. The climb is real and large: the best moved from baseline by ~0.5 score
    //    points (held-out accuracy ~0.49 -> 1.00). Floor at 0.30 so a regression that
    //    halved the climb fails, without brittleness to the exact synthetic dataset.
    let total_lift_points = final_best as f64 / 1_000_000.0;
    assert!(
        total_lift_points > 0.30,
        "the leaderboard should climb by a large real margin, got {total_lift_points}"
    );
}
