# Speedrun Competitions — Spec & Checklist

> **Status: DESIGN PROPOSAL — nothing here is built.** This document specifies a
> new competition shape (the *speedrun*) for the autoresearch market: reach a
> target score as cheaply as possible, on a public, watchable, prize-backed
> leaderboard. It reuses the existing market/gate/settlement primitives almost
> entirely; the one genuinely new piece is a **cost-keyed ranking mode**. Every
> claim about what already exists is filed with a code reference; everything else
> is proposed. Design decisions in §8 are **locked** (Drew-approved picks).

## The system is general; the instances are evidence

The product is the **general market mechanism** — recipe-submitted,
held-out-gated, cost-ranked, prize-settled — not any one speedrun. Concrete
speedruns (modded-nanogpt, Sokoban, ScaleAutoResearch) are *instances* that
prove the mechanism fits a viral genre and spans substrates; they are **not the
target**. modded-nanogpt is the clearest instance, so we use it to make the
mechanism vivid (`github.com/KellerJordan/modded-nanogpt`):

> *Find the fastest algorithm to use 8× NVIDIA H100 GPUs to train a language
> model that attains ≤3.28 cross-entropy loss on the FineWeb validation set.*

That single sentence is the entire spec. The target 3.28 matches Karpathy's
GPT-2-small replication in llm.c. The arc is why it travels:

- **45 minutes** (May 2024, llm.c baseline) → **1.32 minutes** (record #84, May 2026).
  ~**34× faster wall-clock**.
- **10B tokens → under 400M tokens** to reach target. ~**25× more token-efficient**.
- **84 sequential records** over two years, dozens of contributors, each record a
  PR landing one technique (Muon optimizer, rotary embeddings, FP8, Flash-Attention-3,
  value embeddings, …).

Its rules confirm our design almost line-for-line:

| modded-nanogpt rule | Our primitive |
| --- | --- |
| Must hit target with **p<0.01** statistical significance across run logs | `Gate::clears` keys off the CI lower bound (`autoresearch-runtime/src/types.rs:204`) — same significance gate, renamed |
| Records validated by **PrimeIntellect** re-running on their 8× H100s | m-of-n `RescorePanel` independent re-score (`training_market.rs:334`). PrimeIntellect's `prime` repo is *already* our DiLoCo reference (`docs/DISTRIBUTED-TRAINING.md:49`) — same ecosystem |
| Don't touch the data pipeline; change the *algorithm* | `TrainingRecipe` is the researcher's submission (`distributed_training.rs:115`) |
| Must run **faster than prior on the same hardware** | the cost-keyed marginal record bounty (this doc) |

The virality mechanism is the one Ben Recht named: *"AI advances by inventing
games and gloating to goad others to play."* Our version swaps clout for
**on-chain prize money** — strictly stronger incentive.

## 1. The thesis

A speedrun is one point in the existing knob space with one extra axis:

- **Structure = Competitive** — ranked submissions.
- **Cadence = Continuous** — king-of-the-hill; the frontier is "current cheapest
  verified cost to reach the target," and each new record is paid for its
  **marginal cost reduction** over the standing best.
- **Visibility = Public** — open, watchable arena (this is what makes it travel).
- **Scorer = HeldOutEval** — value is a held-out score, exactly as today.

The extra axis is the **ranking objective**. The market today ranks by *value*
(score); a speedrun ranks by **cost-to-target** (resource units consumed to
verifiably reach a target). Lower cost is better. This is **not a fifth knob** —
it is a ranking mode available on any `Continuous` competition, where the
objective flips from "maximize value" to "minimize cost subject to reaching the
target." **Three of the four pieces already exist here** — the surface, the
held-out scorer + CI gate, and the m-of-n referee — so this is the training
vertical with the ranking axis flipped, not a new vertical.

## 2. Why it fits (and what makes it defensible)

The fit is strong precisely because the speedrun's hardest problem — *proving
the cost claim is real* — is the problem this market already solves for score.
The whole architecture rests on one invariant (`docs/DISTRIBUTED-TRAINING.md:20`):

> **Delegating the compute never delegates the trust.** The cluster's
> self-reported numbers are ignored; only the Referee's re-score decides payment.

A speedrun extends that invariant from *score* to *cost*: the cluster's
self-reported cost is never paid on — the Referee recomputes or re-times it, and
re-scores the value on held-out. *Those two* decide payment. A researcher who
lies about either moves nothing. That is what informal speedrun boards cannot do,
and why they fill with fraud: they trust the runner's self-reported time. This
market does not.

## 3. The ranking objective — where it slots in

Today `ContinuousTrainingMarket::run` (`training_market.rs:213`) builds a
`RecordBeat` sequence keyed on `new_best_micros` (a held-out *value* in
micro-units) and settles via `settle_record_bounty`, which pays `wei_per_micro ×
marginal` and buys the frontier exactly once.

A speedrun needs the **mirror**: a beat sequence keyed on `new_best_cost_micros`
(a *cost* in micro-units, **lower is better**), settled by a `settle_record_bounty`
variant that pays for the **marginal cost reduction** `prior_best − this_cost`.
The marginal-once invariant carries over unchanged: across a monotone-improving
(minimizing) cost sequence the total paid is `wei_per_micro × (baseline_cost −
final_best_cost)`, and a non-improving resubmission pays zero.

The ranking is a **two-stage filter** on each submission:

1. **Value gate** — did it verifiably reach the target? "Reach target `T`" is
   `value_ci_lower >= T`, which is **exactly the existing lift gate** with
   `min_lift_ci_lower = T − baseline.value` (Decision §8.2). No new gate field —
   reuses `Gate::clears` (`types.rs:204`) verbatim, fail-closed on `NaN`/`inf`.
2. **Cost rank** — among survivors, lowest **referee-verified** cost wins.

This composes with the existing m-of-n `RescorePanel` (`training_market.rs:334`):
the panel corroborates the value claim before it is trusted, exactly as it
corroborates a held-out loss today.

## 4. The two cost axes (the design crux)

> **Correction from the first draft:** the initial draft recommended *only* a
> derived, hardware-agnostic cost and dismissed wall-clock. The modded-nanogpt
> precedent corrects this — its **main track IS wall-clock** (84 records), and
> it is fair because the **hardware is pinned**. Both axes are real and both are
> viral. The design supports both.

A speedrun can rank on one of two cost axes, and modded-nanogpt runs both as
separate tracks:

| Axis | What `cost` is | Why it's fair | What it needs | Precedent |
| --- | --- | --- | --- | --- |
| **A. Wall-clock, pinned hardware** | seconds on a proposer-pinned spec (e.g. 8× H100) | everyone runs on the *same* hardware | proposer pins the spec; operator **attests** it ran on it | modded-nanogpt **Track 1** (the main 84-record board) |
| **B. Resource efficiency** | tokens-to-target (or steps / episodes / evals) | hardware-agnostic — a pure *method* measure | trainer emits a deterministic count the Referee **recomputes** from recipe+seed | modded-nanogpt **Track 3** ("minimize steps, unlimited wall-clock") |

Both map to `Measurement.cost` (`types.rs:138`); the proposer picks which axis a
competition ranks on (and, for Axis A, pins the hardware spec).

**Axis B is the verify-cheap one.** The cost is **derived** — a deterministic
count the Referee recomputes from the same recipe+seed the held-out re-score
already consumes, with no GPU re-run. So a lie about cost is *irrelevant*: the
market uses the recomputed count and ignores the self-report, exactly like
`train_loss` today (`distributed_training.rs:186`).

**Axis A is the viral one** (it's the track that produced the 34× speedup arc),
but it needs honest hardware-pinning. The repo already has the seam for that —
`TrainingCluster::provides_sealed_isolation` (`distributed_training.rs:380`) and
the TEE→cluster binding — though the attestation is **structural-only today**
(see `docs/DISTRIBUTED-TRAINING.md` §5 / the documented §12 gap). Until
attestation is a real enclave, Axis A is gated the same way the rest of the TEE
work is. Axis A also tolerates a weaker alternative: a PrimeIntellect-style
trusted re-time (an independent referee re-runs on the pinned spec), which the
`RescorePanel` models directly.

**Recommendation:** ship the mechanism (cost-keyed ranking + gate) so *either*
axis plugs in; lead with **Axis B (derived)** for the CI-runnable sim because it
needs no new infra and proves the market mechanism end-to-end; treat **Axis A**
as the headline track that lights up once the operator-attestation path is real.

## 5. Instances and prior art

The mechanism fits a range of concrete speedruns; each instance below stresses a
different facet. None of them is the product — together they show the mechanism
is substrate-agnostic and that the genre is viral.

| Instance | Surface | Cost axis | What it proves |
| --- | --- | --- | --- |
| **modded-nanogpt** Track 1 (Keller Jordan et al.) | LLM pretraining recipe (≤3.28 FineWeb val loss on 8× H100; 84 records, p<0.01 gate, PrimeIntellect-validated) | A — wall-clock, pinned HW | the canonical viral speedrun; the held-out CI gate + independent re-time are its rules verbatim |
| **modded-nanogpt** Track 3 / **ScaleAutoResearch** (Wang: CC+Codex, 2875→2690 steps on 1–2 A40 nodes) | same surface, but an **automated research loop** is the competitor | B — steps-to-target, derived | an autoresearch loop wins on cheap, heterogeneous hardware — permissionless operators, no hyperscale GPUs. This is README's "humans, agents, or automated research loops submit a method" made real |
| **Sokoban Speedrun** (Kaddour: RL+GRPO on Qwen3-4B, 87min/8×H100 baseline) | an RL pipeline that solves Sokoban puzzles | A — wall-clock | speedruns span substrates: pretraining, RL/agents, optimizers. The repo's GRPO adapter already speaks this domain |
| CoreWeave / MLPerf (DeepSeek-V3 671B in 2min, 8192 Blackwell Ultra) | *vendor infra*, not a recipe | n/a — **boundary case** | the limit instance: when the hardware IS the differentiator it is a `ScorerKind::PrivilegedHardware` (`types.rs:70`) competition, not a recipe speedrun. Names the boundary; do not claim it as a speedrun win |

The vertical composes with `autoresearch-verticals::distributed_training` — the
existing `TrainingCluster` seam, `LocalSimCluster` dynamics model, held-out
scorer, and m-of-n panel that every instance above would run on top of.

**Design stance (mirrors `docs/DISTRIBUTED-TRAINING.md:55`):** borrow the
*algorithmic shape* (cost-to-target ranking, two cost axes), **own the
leaderboard + gate + settlement layer as Tangle-native.** The actual trainer that
emits the cost is a cluster backend behind the `TrainingCluster` trait, exactly
as DeMo/DiLoCo are today — this repo does not implement trainers.

## 6. Phased checklist

> Legend: ✅ shipped · 🟡 spec'd, not built · 🔶 lives in a sibling repo · ⛔
> blocked on external infra (GPUs / operators / a real TEE).

### Phase 0 — In-repo sim, CI-runnable 🟡 (proposed, not built)

The market mechanism on a deterministic stand-in, no GPUs — same pattern as the
training vertical's `LocalSimCluster` (`distributed_training.rs:388`).

- [ ] 🟡 `SpeedrunMarket` — mirrors `ContinuousTrainingMarket`, but the beat
      sequence is keyed on **cost** (lower is better) and settled by a
      marginal-**reduction** bounty. Frontier bought exactly once. Target = the
      existing lift gate with `min_lift_ci_lower = T − baseline.value`.
- [ ] 🟡 `settle_record_bounty_min` — the lower-is-better mirror of
      `settle_record_bounty`, paying `wei_per_micro × (prior_best − this_cost)`.
- [ ] 🟡 `SpeedrunRecipe` cost model — a closed-form Axis-B `resource_count`
      (tokens-to-target as a function of recipe + target) so a tuned recipe wins
      on cost and a wasteful one loses; deterministic, CI-runnable.
- [ ] 🟡 `tests/e2e_speedrun.rs` — certifies the genuinely cheaper recipe, gates
      the failure modes (didn't reach target, reached it on train only, claimed
      fewer tokens than used).

### Phase 1 — Real cluster adapter 🟡 (reuses the existing gap, no new infra)

The speedrun does **not** need a new cluster path — it reuses
`autoresearch-training-runtime`'s `SubprocessTrainingCluster` /
`ServiceTrainingCluster` (`autoresearch-training-runtime/src/lib.rs:267`). The
only addition is the **cost-report contract** the backend must satisfy, per axis:

- [ ] 🟡 **Axis B contract** — the external trainer emits a deterministic
      `resource_count` the Referee recomputes (tokens processed before the
      held-out value first crosses `T`). Specified, not enforced here.
- [ ] 🟡 **Axis A contract** — the proposer pins a hardware spec in the
      competition; the cluster attests it ran on that spec (reuses
      `provides_sealed_isolation`); cost = referee-re-timed wall-clock.
- [ ] ⛔ A real run needs the same caller-supplied trainer + GPUs the training
      vertical already waits on (`docs/DISTRIBUTED-TRAINING.md` Phase 1 ⛔). The
      speedrun adds **no new** external-infra dependency beyond that shared one.

### Phase 2 — Trust & verification 🟡

- [ ] 🟡 Reuse `RescorePanel` unchanged: m-of-n corroborates the *value* claim
      before any cost is ranked. No new panel code.
- [ ] 🟡 Document the cheat matrix and which primitive rejects each (§7 below).
- [ ] 🔶 Axis-A attestation as a real (non-structural) TEE tier — deferred until
      the in-repo attestation is a real enclave.

## 7. Cheat matrix (what refuses what)

| Attack | Gate / primitive that rejects it |
| --- | --- |
| Claims it reached target, didn't | Value gate `value_ci_lower >= T` — the existing lift gate, fail-closed on `NaN`/`inf` |
| Reached target on train, not held-out (overfit) | Gate is on the **held-out** lower bound, not train |
| Inflated value claim | m-of-n `RescorePanel` majority rejects a divergent self-report (`training_market.rs:400`) |
| Understated cost (Axis B: claims fewer tokens than used) | Irrelevant — market uses the **referee-recomputed** cost; self-report is provenance only |
| Wrong-hardware lie (Axis A: ran on 16× H100, claimed 8×) | Hardware-pinned spec + operator attestation (TEE seam); or PrimeIntellect-style independent re-time via `RescorePanel` |
| Cheap junk that never reaches target | Value gate; never enters the cost rank |
| Near-boundary value cheat (modest inflation) | Panel rejects at `< m` accepting — same regime proven by `majority_rejects_a_near_boundary_cheat` (`training_market.rs:729`) |

Every attack resolves to an existing primitive or to "the market ignores the
self-report." That is the design's central claim: **the speedrun adds ranking,
not new trust machinery.**

## 8. Decisions (locked)

Drew-approved picks, each chosen because it reuses an existing primitive or is a
strict superset of the alternative:

1. **Resource unit (Axis B): substrate-agnostic contract** — "a deterministic
   count the Referee recomputes," not hard-coded tokens. The market never learns
   the word "token"; survives substrate change (RL episodes, evals, joules).
2. **Target: beat-baseline-by-Δ** — target `T = baseline.value + Δ`, so "reach
   target" is *literally the existing lift gate* with `min_lift_ci_lower = Δ`.
   **No new gate field.** Scale-invariant (works for loss, return, accuracy).
3. **Cadence: Continuous-only for v1** — king-of-the-hill is the watchable,
   viral form; OneShot adds only a settlement variant, deferred.
4. **Ranking: fixed-target total order** — pure cost-rank at one target. Pareto
   (researchers pick how far to push) is *already* the existing value-market
   running alongside, so it emerges from composition, not duplication.
5. **Verification: derived cost (Axis B) first** — ship the mechanism so both
   axes plug in; lead with the derived Axis-B sim (no new infra); Axis-A
   wall-clock lights up when operator-attestation is real.

**The unification that falls out:** the existing value-market and the new
cost-market are *the same mechanism with the `Measurement` axis flipped*
(value→max, cost→min), under one gate. The long-term-ideal refactor —
`ContinuousMarket<Axis>` parameterized by which column it optimizes — is latent,
not required: build the cost-mirror first (zero refactor, no risk to the working
value-market); unify under an axis parameter only once the pattern is proven.
Fully reversible; forecloses nothing.

## 9. Honest status

Nothing in this document is implemented. What **exists today** and the speedrun
composes against: the four-knob model + coherence (`types.rs:77`), `Measurement.cost`
(`types.rs:138`), `Gate` with fail-closed cost ceiling (`types.rs:161`), the
marginal `ContinuousTrainingMarket` (`training_market.rs:129`), the m-of-n
`RescorePanel` (`training_market.rs:334`), and the cluster-agnostic
`TrainingCluster` seam (`distributed_training.rs:364`). What is **proposed**: a
lower-is-better `settle_record_bounty` mirror, a `SpeedrunMarket`, the Axis-B
derived-cost contract, and the Axis-A hardware-pinning contract. What is
**simulated** would be a `LocalSimCluster`-style cost model so the market
mechanism is provable in CI. What is **blocked on external infra** (a real
trainer + GPUs, and — for Axis A only — real operator attestation) is **shared**
with the training vertical's existing Phase-1 ⛔ — the speedrun's Axis B adds no
new external dependency of its own.
