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

### Phase 1 — Real cluster adapter 🟡 ⛔

Implement `TrainingCluster` against a live backend, behind a feature flag exactly
like `autoresearch-sandbox-runtime` gates the real sandbox backend.

- [ ] New crate `autoresearch-training-runtime` with feature `training-runtime`.
- [ ] `PrimeCluster` — submits a recipe as a training job to a `prime` run (MIT);
      maps `TrainingRecipe` → `prime` config; returns the trained checkpoint ref.
- [ ] `PsycheCluster` — alternative backend against a Psyche client (Apache-2.0).
- [ ] Artifact transport: the trained checkpoint is content-addressed; only the
      ref flows through the ledger, never the weights.
- [ ] Held-out re-score runs **on the market's operators**, not the training
      cluster — the trust boundary.

### Phase 2 — `training-blueprint` realism 🔶 ⛔

Make the sibling distributed-training blueprint a real comm-efficient trainer
(this is the "improve the realism of that blueprint" work).

- [ ] Replace the GPU-minutes contribution metric with held-out-gated lift
      (the known gap noted in `docs/MECHANISM.md §6.1`).
- [ ] Wrap `prime`/DeMo as the operator training core; Coordinator role becomes a
      Tangle service job, Clients are the m-of-n operators.
- [ ] Multi-node training *within* one instance (data/model/pipeline parallel).
- [ ] Expose a job interface the `PrimeCluster`/`PsycheCluster` adapter calls.

### Phase 3 — Trust & verification 🟡

- [ ] m-of-n re-score of the returned checkpoint on the held-out split (reuse the
      dispute/slash path already in `autoresearch-protocol`).
- [ ] Deterministic eval harness so re-scores are reproducible across operators.
- [ ] Reward schedules for training: `RecordBounty` for a continuous training
      leaderboard (pay marginal loss reduction), `SnapshotTopK` for one-shot.

### Phase 4 — Cross-instance hierarchy 🟡 ⛔

Train **one model across k m-of-n clusters** (the tightly-coupled regime).

- [ ] Hierarchical DiLoCo: tight sync *within* a cluster, infrequent outer sync
      *across* clusters (the DiLoCo outer optimizer / Psyche coordinator pattern).
- [ ] Coordinator-of-coordinators as a Tangle service spanning k instances.
- [ ] Bandwidth budget surfaced as the `Measurement.cost` the gate's
      `cost_per_task_ceiling` already prices.

### Phase 5 — Privacy / TEE binding 🟡

- [ ] `provides_sealed_isolation()` returns true for a TEE-backed training cluster;
      the private runner already refuses a non-sealed engine for attestation-
      mandating tiers (the binding is enforced, not convention).
- [ ] Structural attestation of the training enclave (inherits the same
      structural-only limitation documented in `docs/PRIVACY.md`).

## 5. Honest status

Shipped today: **Phase 0** — the market drives a real (simulated-dynamics)
distributed-training competition, green in CI. Everything in Phases 1–5 is design
+ checklist; the training itself is **not** wrapped yet, and Phases 1/2/4 need live
GPU service instances. The hard ML (DiLoCo/DeMo) is solved and open — the remaining
work is the adapters + the Tangle-native trust/economic layer around them, not the
training algorithms.
