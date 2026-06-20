# Ranking Objective — any optimization dimension, one parameter

> **Status: DESIGN PROPOSAL — nothing built.** The change is one parameter plus a
> sign-flip; with that, the existing record-bounty settler handles every
> optimization direction unchanged. "Speedrun" dissolves into one configuration.

## Thesis

A proposer posts a task and names **the dimension they want improved** and the
direction. The market ranks and pays on that dimension, under the existing gate.
There are **no special-cased verticals** for speedrun / best-model / cost-capped:
each is one configuration of one mechanism. Agents or users submit improvement
methods; the surface is agnostic to which.

## The surface is already two-dimensional

The primitives already carry two axes — nothing about a "second dimension" needs
inventing:

- `Measurement { value, ci_lower, ci_upper, n, cost }` (`autoresearch-runtime/src/types.rs:128`).
  `value` is the score; `cost` is **scorer-defined units** (`types.rs:137` — "e.g.
  USD" but equally tokens, steps, latency, joules). `cost` is *already* the
  proposer-chosen second dimension; the scorer picks its meaning.
- `Gate { min_lift_ci_lower, cost_per_task_ceiling, min_n }` (`types.rs:161`) —
  the gate already constrains **both** axes: a value-floor (`:163`) *and* a
  cost-ceiling (`:165`).
- The held-out scorer, the m-of-n `RescorePanel`, the commit-reveal, the
  continuous market, the record-bounty settler — all axis-agnostic.

So "any dimension a proposer is interested in" is already expressible as
`value` (the score) + `cost` (whatever secondary quantity the scorer reports).
The only thing missing is the choice of **which axis to rank on**.

## The one missing parameter

The continuous market and the settler are hardcoded to rank on `value`,
higher-is-better: `ContinuousTrainingMarket::run`
(`autoresearch-verticals/src/training_market.rs:213`) builds beats on held-out
value lift, and the result feeds `settle_record_bounty`.

Expose a **ranking objective** on the continuous market / competition spec:

```
RankingObjective { axis: Value | Cost, sign: +1 | -1 }
```

The market builds record beats on `sign × measurement.{axis}` (in micro-units)
and hands them to the settler. That is the entire change.

## Why no new settler — the sign-flip (verified)

`settle_record_bounty` (`autoresearch-runtime/src/reward.rs:96`) already operates
on **signed** `i64` micros: it pays a beat iff
`marginal = new_best − current_best >= epsilon && > 0`, with saturating math
built for the i64 extremes (`reward.rs:111`). Feed it `−cost` micros and a
cheaper submission produces a positive marginal `baseline_cost − new_cost`. The
frontier-bought-once invariant carries over unchanged
(`total = wei_per_micro × (final_signed − baseline_signed)`), and the `i64`
encoding + saturating sub/mul are explicitly designed for the sign-flip's
extremes. **One settler, every direction.**

## Instances — the genre is real; the mechanism isn't

Each is one row. None is a vertical.

| Instance | Ranking objective | Gate | What it is |
| --- | --- | --- | --- |
| best held-out model | `value, max` | lift ≥ Δ | today's default market |
| cost-capped best model | `value, max` | lift ≥ Δ **and** `cost_per_task_ceiling` | also works today |
| modded-nanogpt Track 1 | `cost (wall-clock), min` | value ≥ 3.28 | the canonical viral speedrun |
| modded-nanogpt Track 3 / ScaleAutoResearch | `cost (steps), min` | value ≥ target | an autoresearch loop wins on 1–2 A40 nodes — permissionless, no hyperscale |
| Sokoban Speedrun | `cost (wall-clock), min` | value ≥ solve-target | spans substrates (RL/agents); GRPO adapter speaks this |
| CoreWeave / MLPerf | — | — | **boundary**: vendor infra = `ScorerKind::PrivilegedHardware` (`types.rs:70`), not a recipe competition |

The speedrun genre is viral and worth naming for go-to-market (public,
continuous, cost-ranked, prize-backed — modded-nanogpt's 84 records, Sokoban,
ScaleAutoResearch are the evidence). It is **not** a mechanism. Naming it as a
feature was the framing error this doc corrects.

## Honest status

Nothing here is built. The proposed change is: one `RankingObjective` parameter
on the continuous market, beats built on `sign × axis`, the existing
`settle_record_bounty` reused unchanged. That single generalization subsumes the
speedrun, the best-model market, the cost-capped market, and any future
dimension-specialized idea — because `cost` is already the proposer-defined
second axis. The gate, scorer, panel, and settler are reused as-is. No new
vertical, no new surface type, no new settler.
