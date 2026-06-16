# Distributed-Training Integration — Spec & Checklist

How the autoresearch-competitions market drives **communication-efficient
distributed training**, and the full path from the in-repo seam shipped today to a
production multi-node, multi-instance trainer wrapping proven open-source cores.

> **Status legend:** ✅ shipped in this repo · 🟡 spec'd, not built · 🔶 lives in
> the sibling `training-blueprint` repo · ⛔ blocked on external infra (GPUs /
> service instances).

## 1. The thesis

The market is **substrate-agnostic about how a candidate is produced** — that is the
entire purpose of the `Engine` seam. A distributed-training competition is one
`Surface`/`Scorer`/`Engine` triple where the engine *delegates* the heavy training
to a cluster and the Referee *re-scores the returned artifact on a held-out split*.

**Delegating the compute never delegates the trust.** The cluster's self-reported
loss is ignored; only the market's held-out re-score decides payment. This is what
keeps "pay only for certified lift" intact when someone else's GPUs did the work.

## 2. Two scaling regimes (why "k m-of-n clusters" is two different things)

| | Tightly-coupled: one model across k clusters | Loosely-coupled: k clusters → k candidates |
| --- | --- | --- |
| Shared each step | gradients/weights | nothing (only final artifacts) |
| Cross-cluster traffic | high → bandwidth-walled | ~none during training |
| Scaling in k | sublinear, needs hierarchy | **near-linear** |
| Mechanism | DiLoCo outer loop / Psyche coordinator across clusters | branch-train-merge; the **market** is the merge/select |
| In this design | Phase 4 (hierarchical) | **native** — Phase 0 already models it |

A single instance is bandwidth-bound (m-of-n is a *result-agreement* boundary, not
an inner-loop vote; realistic `n` ≈ tens of operators). The market's *native*
scaling is the loosely-coupled regime — k instances each train a candidate, the
held-out gate merges/selects — which is near-linear and is the regime where the
"trillion auto-researching agents" framing is real rather than marketing.

## 3. What we wrap (licenses verified)

| Project | Repo | License | Role |
| --- | --- | --- | --- |
| Prime Intellect `prime` | `PrimeIntellect-ai/prime` | **MIT** | production globally-distributed training framework (INTELLECT-1, 10B) |
| OpenDiLoCo | `PrimeIntellect-ai/OpenDiloco` | Apache-2.0 | DiLoCo reference impl (archived; superseded by `prime`) |
| `prime-rl` | `PrimeIntellect-ai/prime-rl` | Apache-2.0 | distributed RL post-training |
| Psyche | `PsycheFoundation/psyche` | Apache-2.0 | decentralized training network (Coordinator/Client/Data-Provider; DeMo optimizer) |
| DeMo | Nous (vendored in Psyche) | Apache-2.0 (via Psyche) | Decoupled-Momentum optimizer; standalone repo license to re-confirm before vendoring |

All permissive. **Design stance:** borrow the *training core* (`prime` / DeMo),
**own the coordinator + economic + gate layer as Tangle-native** — do not adopt
Psyche's Solana chain layer. Same two-level shape (coordinator + clients), our
m-of-n + held-out gate + reward settlement is the differentiator.

## 4. Phased checklist

### Phase 0 — In-repo seam ✅ (shipped)

The cluster-agnostic engine seam + a deterministic local stand-in that models the
real DiLoCo/DeMo tradeoffs, proving the market mechanism end-to-end with no GPUs.

- [x] `TrainingCluster` trait — the one interface a real backend implements
      (`train(recipe, seed) -> TrainedArtifact`, `provides_sealed_isolation()`).
- [x] `TrainingRecipe` — islands, sync interval `H`, gradient `keep_fraction`
      (DeMo top-k), inner/outer LR.
- [x] `LocalSimCluster` — deterministic dynamics model: optimal sync interval,
      compression cliff, large-batch penalty, held-out generalization gap.
- [x] `DistributedTrainingEngine<C>` — generic over the cluster; forwards the
      cluster's sealed-isolation property (the TEE→backend binding).
- [x] `DistributedTrainingSurface` / `DistributedTrainingScorer` — recipe
      validation + held-out re-scoring with a 95% CI over eval shards.
- [x] `tests/e2e_distributed_training.rs` — **CI-runnable** (deterministic): the
      market certifies the two genuine improvements (+0.28, +0.19 val-loss) and
      gates all three failure modes (island drift, over-compression, bad LR).

### Phase 1 — Real cluster adapter ✅ ⛔ (code shipped; execution infra-gated)

Crate `autoresearch-training-runtime`, mirroring how `autoresearch-sandbox-runtime`
gates the real sandbox backend (`default = []`; `prime-backend` / `psyche-backend`
gate the heavy execution path).

- [x] `PrimeCluster` (prime, MIT) + `PsycheCluster` (Psyche, Apache-2.0) implement
      `TrainingCluster`; drop into `DistributedTrainingEngine` unchanged.
- [x] `recipe_to_prime_config` / `recipe_to_psyche_config` — pure, unit-tested
      mapping of `TrainingRecipe` → a real prime/DiLoCo (or Psyche/DeMo) run config.
- [x] Feature-off `train()` returns a named `EngineError::Backend`; feature-on
      builds the real `tokio::process` invocation + checkpoint parse.
- [x] `provides_sealed_isolation()` tracks a TEE flag (`.with_tee()`).
- [ ] ⛔ Actually *running* a checkpoint needs `prime`/Psyche installed + GPUs /
      operator instances. The code is real; the execution is not exercised here.

### Phase 2 — `training-blueprint` realism ✅ (PR open in sibling repo)

> **Assessment correction:** the repo was *not* pricing by GPU-minutes. It is a real
> Tangle Blueprint already using **DeMo** (Nous) for comm-efficient training; the
> actual gap was that an on-chain result was just a **checkpoint hash + a loose
> gradient-norm check** — nothing proved the checkpoint was *better* than the base
> model. An operator could submit a well-formed hash and get paid for no improvement.

- [x] `operator/src/eval_gate.rs` — held-out eval gate certifying a checkpoint only
      if it beats the base model on a private held-out split with a **CI lower bound
      ≥ 0.02** (the same bar as this market). Deterministic seeded bootstrap, 8 tests.
- [x] `TrainingJobResult` ABI gains `heldOutCertified` / `improvementBps` /
      `ciLowerBoundBps`; reward attaches to certified improvement, not a bare hash.
- [x] Shipped as **tangle-network/training-blueprint#10** (build/test/clippy/fmt green).
- [ ] On-chain *enforcement*: the BSM does not yet withhold/slash reward when
      `certified == false` (ABI carries the fields; Solidity gating is the follow-up).
- [ ] Backend `/eval_held_out` endpoint (the operator-side contract is wired; the
      training server must implement the private held-out split).

### Phase 3 — Trust & verification ✅

`autoresearch-verticals/src/training_market.rs`:

- [x] `ContinuousTrainingMarket` — king-of-the-hill leaderboard paying the
      **marginal** held-out loss reduction via `settle_record_bounty` (the
      frontier is bought exactly once; non-improving resubmission pays zero).
- [x] `RescorePanel` — m-of-n independent referees re-score the same artifact;
      majority rejects a divergent self-reported score. Deterministic, 10 tests.

### Phase 4 — Cross-instance hierarchy ✅ ⛔ (composition shipped; real run infra-gated)

`autoresearch-verticals/src/hierarchical.rs`:

- [x] `HierarchicalCluster<C>` — composes k inner `TrainingCluster`s and **is itself
      a `TrainingCluster`** (nests), so it drops into the market unchanged. Models
      the scale bonus (`-k_bonus·ln k`) net of a cross-cluster **drift penalty** that
      grows with a too-loose outer-sync interval. 8 tests.
- [ ] ⛔ A real run across k live instances (the coordinator-of-coordinators as a
      Tangle service) needs operator infra; the dynamics composition is simulated.

### Phase 5 — Privacy / TEE binding ✅

`autoresearch-verticals/src/tee_cluster.rs`:

- [x] `TeeSimCluster<C>` — wraps an inner cluster, reports sealed isolation, exposes
      a **structural-only** attestation (honest: same gap as `docs/PRIVACY.md`; not a
      verified enclave). A test drives the **real** `run_private_competitive`: an
      unsealed training engine fails the tier→cluster binding with `AttestationRequired`,
      while a `TeeSimCluster`-backed one clears it (then fails later at the honest
      structural-attestation seam — the documented §12 gap, not a defect).

## 5. Honest status

Shipped: **Phase 0** (one-shot training market) plus **Phases 1, 3, 4, 5** as real,
tested, CI-green code in this repo, and **Phase 2** as an open PR in the sibling
`training-blueprint` repo. What is **simulated** (not real GPU training): the
`LocalSimCluster` / `HierarchicalCluster` dynamics and the prime/Psyche *execution*
path (feature-gated, needs the frameworks + GPUs). What is **real and exercised**:
the cluster-agnostic seam, the recipe→config mappings, the continuous-market and
m-of-n re-score mechanics, the TEE→cluster binding, and (in `training-blueprint`) the
held-out certification gate. The remaining ⛔ items all need live GPU/operator infra
or on-chain enforcement wiring — the algorithms (DiLoCo/DeMo) are solved and open.
