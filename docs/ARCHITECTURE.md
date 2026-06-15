# Autoresearch Competitions — Architecture

> **Status:** technical spec for an in-development Tangle Blueprint. The repo
> holds a hello-world scaffold (`autoresearch-competitions-lib`/`-bin`,
> `contracts/`, `metadata/`). This document describes the system we are
> building. Contract names, crate names, and type sketches marked
> **(proposed)** are not yet implemented. Canon terminology
> (roles, jobs, interfaces, knobs) is fixed by [`SPEC.md`](../SPEC.md) and used
> verbatim here.

This document covers system structure: the settlement spine on-chain, the
sandbox/TEE substrate it is built on, the operator/domain APIs, the Engine and
Scorer adapter models, the Referee, data flows per scenario, scale, and the
honest attestation status. For incentives and dispute math see
[`docs/MECHANISM.md`](MECHANISM.md); for privacy tiers and leakage bounds see
[`docs/PRIVACY.md`](PRIVACY.md).

---

## 1. System overview

A **Proposer** posts a competition `(Surface, Scorer, RewardSchedule, four
knobs)` and escrows a reward. **Researchers** (human, agent, or automated loop —
type-agnostic) **submit a method** (an **Engine** — agent self-improvement loop,
optimizer, training step). They do **not** bring compute. The **Node Operator
provides the sandboxed compute and runs the researcher's method** inside it, next
to the proposer's sealed target — a plain Docker sandbox (no-TEE) or a sealed TEE
enclave, selected by a **one-field toggle** (`SandboxBackend`). A **Referee** runs
the **Scorer** on the **held-out** split inside a TEE, certifies `{value, ci,
cost, diagnostics}`, and commits the score plus a TEE attestation hash on-chain.
Escrow pays out for the **outcome** — a certified score — not the **effort**.
**Validators** are an m-of-n dispute backstop only. **Node Operators** run the AVS
plane (blueprint binary + sandboxes) and are the **compute and the referee**; they
do not author candidates.

The whole mechanism rests on research's **solve-hard / verify-easy asymmetry**:
producing a better artifact is expensive; confirming it scored higher on a
held-out test is one cheap reproducible run. So the chain stores only the cheap,
verifiable part — commitments, certified scores, attestation hashes, payouts —
and the heavy part — **operator-hosted sandboxes running researcher methods**,
datasets, traces — lives off-chain.

```
   DEMAND                  SETTLEMENT SPINE (EVM, tnt-core 0.13)              SUPPLY / COMPUTE
   ──────                  ─────────────────────────────────────             ────────────────

  ┌──────────┐  CREATE_     ┌──────────────────────────────────────┐
  │ PROPOSER │  COMPETITION │   CompetitionFactory  (proposed)     │
  │          │─────────────▶│   CompetitionManager  (BSM subclass) │
  │ Surface  │   escrow     │   Escrow            Leaderboard       │
  │ Scorer   │              │   RewardDistributor AttestationReg.  │
  │ Reward   │              │   DisputeManager                     │
  │ 4 knobs  │◀── payout ───│                                      │
  └──────────┘              └───┬───────────────▲──────────────┬───┘
                       jobs 0-7 │   REPORT_SCORE │  CHALLENGE   │ PROVISION/
                                │   (+attest hash)│ (m-of-n)    │ DEPROVISION
                                ▼                 │             ▼
  ┌──────────┐            ┌─────────────────┐  ┌──┴──────────┐  ┌────────────────────┐
  │RESEARCHER│  JOIN /    │  NODE OPERATOR  │  │  VALIDATOR  │  │  NODE OPERATOR     │
  │(human/   │  COMMIT /  │  FLEET          │  │  COMMITTEE  │  │  (compute+referee) │
  │ agent/   │  REVEAL    │                 │  │  2-of-3     │  │                    │
  │ loop)    │───────────▶│ operator API    │  │  EIP-712    │  │ sandbox-runtime L1 │
  └────┬─────┘  via API   │  :9200          │  │  re-score   │  │ ┌────────────────┐ │
       │ SUBMITS a        │ domain  API     │  │  on dispute │  │ │ OPERATOR runs  │ │
       │ method (Engine)  │  :9100          │  └─────────────┘  │ │ method sandbox │ │
       ▼                  └────────┬────────┘                   │ │ Docker|TEE     │ │
  ┌──────────────────┐             │ provision+run              │ │ (1-field tgl)  │ │
  │ method ref (the  │             ▼                            │ └────────────────┘ │
  │ submitted Engine;│     ┌────────────────────────────────┐   │ ┌────────────────┐ │
  │ operator runs it │     │  REFEREE (attested TEE eval)   │   │ │ REFEREE        │ │
  │ on its compute,  │────▶│  score(candidate, HeldOut)     │──▶│ │ Scorer in TEE  │ │
  │ candidate off-   │     │  → certified lift + attest.    │   │ │ held-out split │ │
  │ chain)           │     └────────────────────────────────┘   │ │ → {value,ci,   │ │
  └──────────────────┘                                          │ │    cost} + hash│ │
                                                                 │ └────────────────┘ │
                                                                 └────────────────────┘
                                    │
                                    ▼
                  ┌───────────────────────────────────────┐
                  │ VERIFIABLE LEADERBOARD  +  ARTIFACT     │
                  │ MARKETPLACE                            │
                  │ ranks recomputable from on-chain       │
                  │ scores + attestation hashes;           │
                  │ challengeable via CHALLENGE → slash    │
                  └───────────────────────────────────────┘
```

### One request, end to end

A Researcher running an agent self-improvement loop in Scenario B (public
continuous arena):

1. **Proposer** calls `CREATE_COMPETITION` (job 0): escrows reward, commits the
   sealed Scorer ref, four knobs, and RewardSchedule. The competition goes
   `Draft → Open`. One on-chain row.
2. **Researcher** calls `JOIN` (job 1, posts stake) and **submits a method**
   (its Engine, by reference). The **Node Operator provisions a sandbox and runs
   the submitted method** inside it (the `SandboxMethodEngine` + `SandboxHost`
   seam, §2.x), producing a candidate next to the proposer's sealed target. The
   Researcher / operator calls `COMMIT_CANDIDATE` (job 2) with only the **artifact
   hash**.
3. After the commit window, `REVEAL_CANDIDATE` (job 3) discloses the artifact to
   the Referee — over the domain API, not on-chain. The chain stores the hash; the
   artifact bytes never touch it.
4. The **Referee** (a TEE eval service inside a Node Operator sandbox) runs the
   Scorer on the **held-out** split, gets `{value, ci, cost, diagnostics}`,
   computes a TEE attestation hash, and calls `REPORT_SCORE` (job 4). The
   competition goes `Submitting → Scoring`. Researchers see *their score*, never
   the held-out data.
5. `TICK` (job 7) closes the epoch; `SETTLE` (job 5) pays the **marginal lift**
   over the prior record per `RecordBounty`, plus `TimeAtTopStreaming` accrual.
6. Anyone can **recompute** the public leaderboard from on-chain scores +
   attestation hashes + revealed artifacts + the public Scorer, and `CHALLENGE`
   (job 6) a suspect score, which forces an m-of-n Validator re-score with slash.

N candidates produce N off-chain attested scores but only N hashes and one
settlement per epoch hit the chain. The chain footprint is **O(competitions)**,
not O(artifacts).

---

## 2. Layered model

This blueprint is built **on** the agent-sandbox blueprint family and composes
the training blueprint. Three layers, with a strict one-way dependency rule.

```
  L2  autoresearch-competitions (this blueprint)
      ─ competition lifecycle, jobs, Escrow/Leaderboard/Reward, Referee, adapters
      ─ consumes L1 traits; never touches L0 directly
              │  (allowed: L2 → L1 only)
              ▼
  L1  sandbox-runtime
      ─ SandboxProvider / RuntimeAdapter / TemplatePack / TenantProfile
      ─ jobs SANDBOX_CREATE / DELETE / WORKFLOW_*; modes cloud / instance /
        tee-instance; TEE backends Phala(TDX) / AWS Nitro / GCP / Azure / direct
              │  (allowed: L1 → L0)
              ▼
  L0  microvm-runtime  (firecracker driver, in-process)
      ─ microVM lifecycle; operator is the Firecracker host (no host-agent svc)
```

### Dependency rules (from sandbox-runtime CONTRACTS)

| Rule | Status |
| --- | --- |
| `Product (L2) → RuntimeAdapter (L1) → SandboxProvider (L0)` | **Allowed** |
| `L2 → L0` direct dependency | **Forbidden** |
| `L2 → L2` cross-product dependency | **Forbidden** (training-blueprint is composed via a *job dispatch*, not a code dependency on its internals) |
| Additive fields with safe defaults | Allowed without major bump |

### What we reuse vs add

| Layer | Reused as-is | Added by L2 |
| --- | --- | --- |
| L0 microvm-runtime | Full microVM lifecycle | — |
| L1 sandbox-runtime | `SandboxProvider`, `RuntimeAdapter`, `TemplatePack`, `TenantProfile`, `InstanceLifecycleReporter`; sandbox jobs `SANDBOX_CREATE/DELETE/WORKFLOW_*`; TEE backends; sealed secrets (x25519); cloud/instance/tee-instance modes; `PROVISION`/`DEPROVISION` | TemplatePacks for Engine sidecars and the Referee Scorer image |
| L2 (this) | — | Competition jobs 0–7; `CompetitionManager` (BSM subclass) + Escrow/Leaderboard/RewardDistributor/AttestationRegistry/DisputeManager; `Surface`/`Scorer`/`Engine`/`RewardSchedule` traits; Engine + Scorer adapters; Referee TEE eval path; operator API :9200 + domain API :9100 |

The agent **Scorer** vertical (`HeldOutEval` over an AgentProfile) reuses the
**Tangle Intelligence Improvement-Plane**: AgentProfile, agent-eval
(`EvalRunEvent`/`TraceSpanEvent`), replay Tiers A/B/C, the held-out gate, the
evidence ledger, and the R2 validity guards. The **Collaborative** path reuses
the **training blueprint** (DeMo, `DistributedTrainingBSM.sol`) by dispatching
its jobs — not by importing its internals (that would violate `L2 → L2`).

### 2.x The operator-compute seam — `SandboxHost` + the TEE/no-TEE toggle

The point where a **researcher-submitted method runs on operator compute** is the
`autoresearch-sandbox` crate. It is the clean seam between the engine-agnostic
orchestrator (L2) and the real `sandbox-runtime` (L1):

- **`SandboxBackend`** — the **one-field TEE/no-TEE toggle**. `Local` (in-process,
  test/dev), `Docker` (a real plain sandbox, no-TEE), `Tee(TeeType)` (a sealed
  enclave). `is_tee()` / `requires_sealed_inputs()` derive from it; `for_competition(tier,
  required_tee)` selects it (a `WhiteBoxNoEgress`/`AttestedHarness` tier with a real
  TEE → `Tee`, else `Docker`). Flipping the field is the *only* change between a
  plain-container and a sealed-enclave competition — the method, engine, and scorer
  are identical.
- **`SandboxHost`** — `provision(req) → run_method(handle, method, ctx) → teardown(handle)`.
  The operator's job: stand up the sandbox (mapping `Tee` → `tee_required` + sealed
  inputs + a captured **structural** attestation on the handle), run the researcher's
  submitted `method` against the proposer's sealed target, and tear down. For a TEE
  backend, `SandboxProvisionReq::validate` is **fail-closed**: an open or missing
  egress policy is rejected, so the no-egress invariant (PRIVACY §5.3, M4) holds.
- **`SandboxMethodEngine<H>`** — implements the orchestrator's `Engine`. Its `produce`
  is the full operator flow (provision → run method → resolve candidate → teardown),
  so it drops into `run_oneshot_competitive` / `run_private_competitive` unchanged.
  This **replaces the in-process stand-in** (`LocalSearchEngine` running on the
  researcher side) with operator-hosted execution.
- **Backends.** `LocalSandboxHost` (DEFAULT) runs the method in-process — no Docker,
  no network, deterministic — so the default test suite and all six gates stay green;
  it is honestly a **stand-in** (its TEE attestation is synthetic and structural-only).
  The **real** operator compute is `SandboxRuntimeHost` in the
  `autoresearch-sandbox-runtime` crate (a workspace member), which calls the real
  `sandbox-runtime` (`create_sidecar` with `runtime_backend`/`tee_required` from the
  toggle, exec to run the method, `delete_sidecar` on teardown, sealed secrets for TEE).
  The whole workspace is pinned to `blueprint-sdk = "=0.2.0-alpha.6"` (matching
  `sandbox-runtime`), so the real backend builds **in-workspace** behind the
  `sandbox-runtime` feature (`cargo build -p autoresearch-sandbox-runtime --features
  sandbox-runtime`); the default build leaves it off to stay fast.

**Honesty:** attestation is **structural-only** today (PRIVACY §12, §7) — capturing
a report's bytes is not verifying its hardware quote; `verify_structural` never
returns `Verified`. The `agent-sandbox-blueprint` (`sandbox-runtime`) is the **wired**
operator compute (feature-gated real backend + local default), not an aspiration.

---

## 3. On-chain architecture (the settlement spine)

The chain is a **settlement and commitment spine**. It holds the state needed to
make payouts and to make the leaderboard recomputable and challengeable — and
nothing heavier. Contract names below are **(proposed naming)**.

### 3.0 What is and is NOT on-chain — the load-bearing boundary

| On-chain (YES) | Off-chain (NO) |
| --- | --- |
| Competition definition: knobs, sealed Scorer ref, RewardSchedule, deadline/policy | The Scorer *implementation* and its held-out dataset |
| Escrowed reward and stakes | Researcher Engines, methods, and compute |
| Artifact **commitment hashes** (commit-reveal) | Artifact **bytes** (revealed to the Referee over the domain API) |
| Certified `{value, ci.lower, ci.upper, cost, n}` per candidate | Raw diagnostics, traces, eval transcripts |
| TEE **attestation hash** per `REPORT_SCORE` | TEE attestation *evidence* (enclave measurement + inputs blob) |
| Per-epoch record, ranking, marginal-lift / streaming payouts | Dev-split feedback (redacted, served off-chain) |
| Dispute state, slash outcomes, Validator EIP-712 signatures | Re-score compute (runs off-chain, only the verdict lands) |

The rule from sandbox-runtime CONTRACTS holds: **on-chain jobs are for state
transitions only**. If it mutates authoritative settlement state → on-chain job.
If it is read-only or operational I/O → `eth_call` or the operator/domain HTTP
API.

### 3.1 Contracts (proposed naming)

```
  CompetitionFactory ──deploys──▶ CompetitionManager (one per competition or shared registry)
                                        │ owns
        ┌───────────────┬───────────────┼───────────────┬──────────────────┐
        ▼               ▼               ▼               ▼                  ▼
     Escrow        Leaderboard   AttestationRegistry  RewardDistributor  DisputeManager
                                  / Referee registry
```

**`CompetitionFactory`** — *(proposed naming)*
- *Responsibility:* create competitions; validate Reward/Cadence coherence
  (§4 matrix in SPEC) at creation; register the competition row.
- *Key state:* `competitionId → CompetitionManager` (or a single manager with a
  `competitions` mapping); next id.
- *Key functions:* `createCompetition(knobs, sealedScorerRef, schedule,
  deadlinePolicy)` — reverts on incoherent `(Cadence, RewardSchedule)` pairs
  (e.g. `RecordBounty × OneShot`), entry point for job 0 `CREATE_COMPETITION`.

**`CompetitionManager` (BSM subclass)** — *(proposed naming)*
- *Responsibility:* the competition state machine
  (`Draft → Open → Submitting → Scoring → Settling → Closed`, plus the Continuous
  loop). Subclasses the tnt-core **Blueprint Service Manager**, so it inherits
  operator registration, job routing, payments, and slashing hooks. Mirrors how
  the trading blueprint subclasses BSM and how `DistributedTrainingBSM.sol` does
  for training.
- *Key state:* per-competition status, knobs, sealed Scorer ref, RewardSchedule,
  current record, epoch counter, entrant set, per-candidate commitment hashes and
  certified scores.
- *Key functions / jobs:* routes jobs 0–7 (see §3.3); enforces legal transitions
  (illegal transitions revert, AC §9.1); commits the record on `SETTLE`.

**`Escrow`** — *(proposed naming)*
- *Responsibility:* custody the Proposer reward and Researcher/Referee/challenger
  stakes; release per `RewardDistributor`. Mirrors the trading blueprint's
  ERC-7575-style vault/escrow pattern.
- *Key state:* `competitionId → balance`; per-account stake; locked vs released.
- *Key functions:* `fund`, `lockStake`, `release(to, amount)`, `slashTo(...)`.

**`Leaderboard`** — *(proposed naming)*
- *Responsibility:* the verifiable ranking. Stores certified scores + attestation
  hashes so any third party can **recompute** ranks (AC §9.7; Eigen/OpenRank bar).
- *Key state:* per-candidate `{commitmentHash, value, ci, n, cost, attestHash,
  refereeId}`; per-epoch record and `recordHolder`.
- *Key functions:* `recordScore(...)` (called via `REPORT_SCORE`), `currentRecord`,
  `rankAt(epoch)` (view; ranking is deterministic from stored scores).

**`RewardDistributor`** — *(proposed naming)*
- *Responsibility:* turn certified scores into payouts per RewardSchedule.
- *Key state:* schedule params; paid-out totals; `recordHolder` for streaming.
- *Key functions:* `settle(epoch)` — `RecordBounty`:
  `reward_per_unit_lift × max(0, new_record − prior_record)`, paid **only when**
  `ci.lower − prior_record ≥ min_lift_ci_lower` (AC §9.4); `TimeAtTopStreaming`,
  `SnapshotTopK`, `TerminalPrize`. Collaborative: split by `ContributionShare`
  (bps sum = 10,000, AC §9.11).

**`AttestationRegistry` / Referee registry** — *(proposed naming)*
- *Responsibility:* record which Referee certified each score and the attestation
  hash; gate `REPORT_SCORE` to registered Referees.
- *Key state:* `refereeId → {operator, stake, status}`; `(competitionId,
  candidateHash) → attestHash`.
- *Key functions:* `registerReferee`, `commitAttestation(candidateHash,
  attestHash)`. **Every `REPORT_SCORE` must carry an attestation hash; settlement
  without one reverts** (AC §9.12).

**`DisputeManager`** — *(proposed naming)*
- *Responsibility:* the dispute backstop. A staked `CHALLENGE` triggers an m-of-n
  Validator re-score; the committee signs the verdict EIP-712; slash flows to the
  faulty party. Mirrors the trading blueprint's `TradeValidator` m-of-n EIP-712
  pattern (default 2-of-3, score threshold ≥ 50).
- *Key state:* open disputes; Validator signer set + threshold; tolerance.
- *Key functions:* `challenge(candidateHash, stake)`, `submitReScore(verdict,
  signatures[])`, `resolve()` — slashes Referee if re-score disagrees beyond
  tolerance, else slashes the challenger (AC §9.6).

### 3.2 Commit-reveal on-chain

Two phases prevent copying a rival's revealed artifact:

```
   COMMIT_CANDIDATE (job 2)              REVEAL_CANDIDATE (job 3)
   ────────────────────────             ──────────────────────────
   on-chain: store H = artifact_hash    off-chain: send artifact bytes to Referee
             (commitment only)          on-chain: store reveal record; manager checks
                                                   artifact_hash(bytes) == H
   reveals with no matching prior commit → REVERT
```

The **commitment hash** is on-chain; the **artifact bytes** are not — they are
revealed to the Referee over the domain API. A reveal whose hash does not match a
prior commit is rejected; an artifact revealed by A in epoch *t* cannot be
committed by B for credit in *t* or later (AC §9.3).

### 3.3 tnt-core primitives in use

| tnt-core primitive | Use here |
| --- | --- |
| **Jobs** | The 8 competition jobs (0–7) plus inherited `PROVISION`/`DEPROVISION` route through the BSM job dispatcher. Each job is a settlement-only state transition. |
| **Payments** | Escrow funding and payouts settle in the service's payment asset; x402 gates paid operator/domain API calls (Referee fees, private-arena access). |
| **Operators** | Node Operators register against the service; `isServiceOperator` gates `PROVISION`/`DEPROVISION` and operator-signed lifecycle sync (`InstanceLifecycleReporter`). Referees and Validators are operator-bound, staked roles. |
| **Slashing** | tnt-core slashing hooks fire on dispute resolution (faulty Referee or challenger), on over-querying in Private tiers (PRIVACY), and on operator downtime/faulty provisioning. |

`FeeDistributor` splits service revenue **70% operator / 30% validators**,
mirroring the trading blueprint.

---

## 4. Off-chain runtime

The off-chain plane mirrors the trading blueprint's two-API split and
per-sidecar agent loop. Crate names are **(proposed naming)**.

```
  ┌──────────────────────────────────────────────────────────────────────────┐
  │  NODE OPERATOR  (autoresearch-competitions-bin)                          │
  │                                                                          │
  │  ┌────────────────────┐   ┌──────────────────────────────────────────┐  │
  │  │ operator API :9200 │   │ domain API :9100                         │  │
  │  │ EIP-191 → PASETO   │   │ submission, reveal, dev-split feedback,  │  │
  │  │ provisioning,      │   │ score reporting, leaderboard reads       │  │
  │  │ lifecycle, status  │   │ (x402-gated for paid/private calls)      │  │
  │  └─────────┬──────────┘   └────────────────────┬─────────────────────┘  │
  │            │ provision via RuntimeAdapter (L1)  │                        │
  │            ▼                                     ▼                        │
  │  ┌──────────────────────────┐   ┌────────────────────────────────────┐  │
  │  │ Researcher sidecar(s)    │   │ Referee sidecar (TEE)              │  │
  │  │ Docker/microVM agent loop│   │ Scorer image; held-out split;      │  │
  │  │ Engine adapter           │   │ emits {value,ci,cost} + attest hash│  │
  │  └──────────────────────────┘   └────────────────────────────────────┘  │
  │                                                                          │
  │  local state:  SQLite (competitions, candidates, scores)                │
  │                JSONL  (eval transcripts, evidence ledger, decisions)    │
  └──────────────────────────────────────────────────────────────────────────┘
```

### Proposed crates

| Crate (proposed) | Role |
| --- | --- |
| `autoresearch-runtime` | Core types, the `Surface`/`Scorer`/`Engine`/`RewardSchedule` traits, the adapter registries, chain client, local-state store. The analogue of `trading-runtime`. |
| `autoresearch-competitions-lib` | Tangle blueprint jobs (0–7) + competition-lifecycle orchestration; BSM wiring. Analogue of `trading-blueprint-lib`. |
| `autoresearch-competitions-bin` | Operator binary: processes jobs, hosts operator API :9200 + domain API :9100, manages Researcher/Referee sidecars, registers the `TICK` keeper cron. Analogue of `trading-blueprint-bin`. |
| `referee-lib` | Referee server: held-out scoring, certification, attestation-hash production, `REPORT_SCORE` submission. |
| `referee-bin` | Referee binary (runs a Referee node; may co-locate with an operator or run standalone in a TEE). Mirrors the validator-lib/-bin split in trading. |
| Engine adapters | `SandboxAgentLoopEngine`, `DeMoTrainingEngine`, `BlackBoxOptimizerEngine`, `HumanSubmissionEngine` (see §5). |

### Operator vs domain API

- **Operator API :9200** — operator-only. EIP-191 challenge → PASETO v4.local
  tokens (1h TTL), exactly as trading. Provisioning, sandbox lifecycle, status,
  liveness. `verify_submitter()`-style checks bind the caller to the registered
  operator/role.
- **Domain API :9100** — the competition surface consumed by Researcher sidecars
  and Proposers: submit/reveal candidates, fetch redacted dev-split feedback,
  read the leaderboard, and (Referee side) post certified scores. x402 gates paid
  and Private-visibility calls.

### Per-Researcher sidecar agent loop

Each Researcher running an agent Engine gets a fresh per-tick session
(`autoresearch-{competition}-{researcher}-{epoch}`), no conversation context
across ticks, persistent filesystem state (SQLite + JSONL) — the trading
sidecar's session-isolation model. The loop: read state → diagnose →
propose-variant → score on **dev** split → promote-if-gate-passes →
`COMMIT_CANDIDATE`/`REVEAL_CANDIDATE`. The protocol never inspects the loop; it
only sees the artifacts it emits (SPEC non-goal: we never audit how a Researcher
worked).

### Local state

SQLite holds structured rows (competitions, entrants, candidate commitments,
certified scores, payouts). JSONL append-only logs hold eval transcripts, the
evidence ledger (`{kind, delta, ci, n, confounded}`), and decision logs. None of
this is authoritative for settlement — the chain is; local state is the operator's
working set and the re-score input on dispute.

---

## 5. Engine adapter model

An **Engine** is what a Researcher runs to **produce** candidates. The protocol
is engine-agnostic: it never inspects an Engine beyond the artifacts it emits.

```rust
/// (proposed) What a Researcher runs to PRODUCE candidates.
pub trait Engine {
    type Artifact;
    fn produce(&mut self, feedback: Option<DevFeedback>) -> Result<Self::Artifact, EngineError>;
}
```

Four concrete adapters:

| Adapter (proposed) | Backed by | Produces | Notes |
| --- | --- | --- | --- |
| **`SandboxAgentLoopEngine`** | sandbox-runtime L1 (`RuntimeAdapter::provision/prompt/task`) | AgentProfile artifacts `{skills, prompts, tools, memory}` | The Scenario-B default; diagnose → propose-variant → backtest → promote-if-+metric-&-no-regression, walk-forward holdout. |
| **`DeMoTrainingEngine`** | dispatch to the **training blueprint** (`TRAINING_JOB`/`CHECKPOINT_JOB`/`LEAVE_JOB`) | model checkpoints on one **shared** artifact | The only `Collaborative`-mode engine. DeMo = DCT + top-0.1% sparsification + libp2p gossip momentum sync. Contribution = GPU-minutes. **Verification statistical-only today** (§11). |
| **`BlackBoxOptimizerEngine`** | Researcher's own optimizer | config / algorithm / weights candidates | No agent loop; just emits artifacts. |
| **`HumanSubmissionEngine`** | a human, via the domain API | any artifact | Raw submission; commit-reveal still applies. |

### How a competition selects one

The Engine is the **Researcher's** choice, constrained by the competition's
`(Structure, Surface)`:

- `Structure = Collaborative` → `DeMoTrainingEngine` (pooled compute on one
  shared artifact, contribution-share payout).
- `Structure = Competitive` → any of `SandboxAgentLoopEngine`,
  `BlackBoxOptimizerEngine`, `HumanSubmissionEngine`, whichever produces a valid
  `Surface::Artifact`.

Swapping the Engine requires **no change** to jobs, lifecycle, or settlement
(AC §9.9). The `Surface::validate` and `Surface::artifact_hash` hooks are the only
contract between Engine output and the protocol.

---

## 6. Scorer adapter model

A **Scorer** measures an artifact on a split and runs **inside the Referee TEE**.

```rust
/// (proposed) Measures an artifact on a split. Runs inside the Referee TEE.
pub trait Scorer {
    type Artifact;
    fn kind(&self) -> ScorerKind; // HeldOutEval | PrivateOracle | PrivilegedHardware | HumanPanel
    fn score(&self, a: &Self::Artifact, split: Split) -> Result<Score, ScorerError>;
}
pub enum Split { Dev, HeldOut }
```

Four concrete adapters, one per `Scorer type` knob:

| Adapter (proposed) | knob | Substrate | Certified output |
| --- | --- | --- | --- |
| **`ImprovementPlaneScorer`** | `HeldOutEval` | agent-eval over an AgentProfile; replay Tiers A (full re-exec) / B (tool-mocked deterministic) / C (observational, **never promotes**); held-out gate `minLiftCiLower 0.02`, `costPerTaskCeiling` | `{value, ci, cost, diagnostics, n}` + evidence-ledger entry |
| **`PrivateOracleScorer`** | `PrivateOracle` | a hidden reference answer the Researcher cannot see (e.g. withheld ground-truth circuit result) | same |
| **`PrivilegedHardwareScorer`** | `PrivilegedHardware` | hardware only the Referee has (real QPU, licensed simulator, specialized rig) | same |
| **`HumanPanelScorer`** | `HumanPanel` | a panel of human judges (subjective surfaces: design, writing, alignment) | same; higher variance, so prefer `SnapshotTopK` over `RecordBounty` (SPEC §4) |

### The held-out / dev split boundary

```
   DEV split                                 HELD-OUT split
   ─────────                                 ──────────────
   Researcher MAY see                        Researcher NEVER sees
   scores + redacted diagnostics             secret measure used for settlement
   steers the Engine                         Referee scores against it; certifies
        │                                          │
        ▼  feedback (Option<DevFeedback>)          ▼  Score{value, ci, cost, n}
   produce() next candidate                   committed via REPORT_SCORE + attest hash
```

Paying against **held-out** is what stops overfitting-to-the-test: a Researcher
with unlimited dev access cannot raise their held-out settlement score without
genuine generalization (AC §9.2). In Private tiers, even dev feedback is
leakage-bounded — rate-limit + CI noise + rotation + slash on over-query
(PRIVACY).

### How certified lift `{value, ci, n}` is produced and committed

1. Referee receives the revealed artifact; `Surface::apply` materializes a target.
2. Scorer runs on the **held-out** split → `Score{value, ci, cost, diagnostics, n}`.
3. Validity guards (Improvement-Plane R2): `n ≥ 12`, model parity across compared
   runs, state-complete snapshot. Failing a guard blocks certification.
4. **Certified lift** = `value` with `ci`, signed with the TEE attestation hash.
5. `REPORT_SCORE` (job 4) writes `{value, ci, n, cost}` + attestation hash to the
   `Leaderboard`/`AttestationRegistry`. Diagnostics are **redacted before reaching
   a Researcher** and never stored on-chain.

Swapping the Scorer substrate (`HeldOutEval → PrivateOracle`) requires no change
to jobs, lifecycle, or settlement (AC §9.9).

---

## 7. Referee architecture

The Referee is the **scarce trusted resource**: held-out eval is the only thing
the chain can't cheaply re-derive. Two paths, by design asymmetric.

```
   COMMON PATH (cheap, scalable)             DISPUTE BACKSTOP (rare, expensive)
   ─────────────────────────────             ──────────────────────────────────
   attested-TEE eval service                 m-of-n Validator re-score
   runs Scorer on held-out once              triggered only by CHALLENGE
   commits {value,ci,n} + attest HASH        re-runs Scorer; EIP-712 verdict
   parallel across competitions              2-of-3, score ≥ 50; slash on mismatch
```

### Default path — attested-TEE eval service

A Referee is a TEE-isolated Scorer sidecar (Phala TDX / AWS Nitro / GCP / Azure /
direct, per L1). It runs the Scorer **once** per candidate on held-out data,
produces `{value, ci, cost, diagnostics}`, and computes a **TEE attestation hash**
= hash of the attestation evidence (enclave measurement + inputs). Only the hash
hits the chain via `REPORT_SCORE`; the heavy evidence blob stays off-chain and is
re-checked **only on dispute**. This is what keeps the chain O(competitions) — we
**attest once, re-score only on dispute, never as the common path** (SPEC
non-goal).

### Dispute backstop — m-of-n Validator re-score

A staked `CHALLENGE` (job 6) hands the candidate to a Validator committee
(default 2-of-3, score threshold ≥ 50). Each Validator re-runs the Scorer,
signs the verdict EIP-712, and `DisputeManager.resolve()` compares against the
certified score. Disagreement beyond tolerance → slash the Referee + reward the
challenger; agreement → slash the challenger's stake (AC §9.6, §9.7).

### How the attestation hash is committed

`REPORT_SCORE` carries `attestHash` alongside the certified score; the
`AttestationRegistry` stores `(competitionId, candidateHash) → attestHash` and the
`Leaderboard` references it so ranks are recomputable. **Settlement without an
attestation hash reverts** (AC §9.12).

### The honest attestation gap

Today, attestation is **structural-only**: the system checks the enclave's
*shape* (the structure of the attestation JSON returned by L1), **not** the full
remote-attestation chain. Specifically **not yet implemented**: hardware quote
**signature verification** and **measurement pinning** (asserting the enclave
measurement matches an expected, pinned value). Closing this gap requires:

1. Verifying the hardware quote signature against the TEE vendor's root of trust.
2. Pinning expected enclave measurements per Scorer image and rejecting mismatches.
3. Binding the attested measurement to the committed `attestHash` so a dispute can
   prove the score came from the *expected* code on *genuine* hardware.

Until then, the attestation hash proves *an* enclave of the right shape ran, not
that it was genuine and unmodified — see §11.

---

## 8. Data flow per reference scenario

Each sequence highlights **what crosses each trust boundary**. `═══` marks a
trust boundary; nothing proprietary crosses it the wrong way.

### A — Private Oracle (quantum withheld circuit)

`Competitive × OneShot × Private × PrivateOracle`, `TerminalPrize`.

```
  Proposer            Chain                 Researcher           Referee (TEE)
  ────────            ─────                 ──────────           ────────────
  CREATE_COMPETITION ─▶ escrow + sealed
   (withheld circuit)   Scorer ref
                                            JOIN (stake) ─────────▶ chain
                                            submit optimizer (operator-run)
                                            COMMIT_CANDIDATE ─────▶ chain (hash only)
            ═══════════════ artifact bytes never on-chain ═══════════════
                                            REVEAL ─(domain API)──────────▶ circuit bytes
                                                                   run vs hidden oracle
                                                                   on PRIVILEGED HW
            ═══════ held-out reference + QPU result never leave Referee ═══════
                       REPORT_SCORE ◀───── {fidelity, ci} + attest hash
  FINALIZE ─────────▶ rank, TerminalPrize
```

What crosses: Researcher → Referee, the **circuit** (their own work). Referee →
chain, only **score + hash**. The withheld reference and QPU result never leave
the Referee. Researcher sees *their fidelity score*, never the oracle.

### B — Public Continuous Arena (Eigen-style)

`Competitive × Continuous × Public × HeldOutEval`, `RecordBounty` +
`TimeAtTopStreaming`.

```
  loop per epoch:
    Researchers ─ COMMIT/REVEAL ─▶ Referee scores on HELD-OUT (Improvement-Plane)
                                   REPORT_SCORE + attest hash ─▶ Leaderboard (PUBLIC)
    TICK ─▶ close epoch ─▶ SETTLE: pay marginal lift over prior record + streaming
    anyone ─ recompute ranks from on-chain scores + attest hashes + revealed artifacts
    anyone ─ CHALLENGE ─▶ m-of-n re-score + slash
```

What crosses: everything is **public** except the held-out split itself (lives in
the Referee) and raw diagnostics (redacted). The leaderboard is the marketing
surface; its credibility comes from public recompute + challenge.

### C — Private Enterprise Bounty

`Competitive × Continuous × Private × HeldOutEval`, `RecordBounty` +
`costPerTaskCeiling`, Redacted-feedback tier.

```
  Enterprise ─ seal private eval set, set access policy ─▶ CREATE_COMPETITION (Private)
            ═══════ private eval set + raw data stay inside enterprise/committee TEE ═══════
  Permitted Researchers ─ JOIN (stake) ─ COMMIT/REVEAL ─▶ Referee (enterprise-run or committee)
                          feedback = scores + BOUNDED diagnostics (no raw data)
                          leakage-bounded: rate-limit + CI noise + rotation + slash
  Referee ─ score on private held-out ─ REPORT_SCORE + attest hash ─▶ chain (access-controlled)
  TICK + SETTLE ─ pay marginal lift each epoch
  verification ─ to PERMITTED parties only (not a public recompute)
```

What crosses: Researchers see **scores + bounded diagnostics**, never raw data
(SPEC: a Researcher cannot have all of {arbitrary code, raw data, free egress} —
pick ≤ 2). Nothing proprietary leaks because Researchers see scores, not data.

---

## 9. Scale & multi-instance

The scale principle is fixed: **chain = settlement/commitment spine
O(competitions)**; **compute = ephemeral sandboxes, multi-instance, horizontally
unbounded**; **held-out eval = the scarce trusted resource → parallel attested
Referee, commit attestation hash, re-score only on dispute**.

### One box (instance mode) vs a fleet (cloud / multi-operator)

| Dimension | Instance mode (one box) | Cloud / multi-operator (fleet) |
| --- | --- | --- |
| Job set | reduced (`configure/start/stop/status/extend` + competition jobs) | full (`PROVISION`/`DEPROVISION` + competition jobs) |
| Sandboxes | one operator hosts Researcher + Referee sidecars locally | sandboxes spread across many Node Operators |
| Referee | co-located TEE sidecar | dedicated TEE Referee operators, parallel |
| Settlement | same chain spine | same chain spine, identical settlement |
| Use | dev, single-tenant private bounty, `tee-instance` for a private enterprise | public arenas, large competitions |

Cross-instance settlement is **bit-identical** — ranking is a pure function of the
on-chain certified scores, so any instance settling the same competition state
produces the same payouts (AC §9.8, eval E6).

### Sharding

Competitions are independent; they shard trivially across operators. Within a
competition, **candidate scoring is embarrassingly parallel** — N candidates → N
independent Referee runs across N sandbox instances, each committing one
attestation hash. The only serialization point is `SETTLE` per epoch, which reads
the already-committed scores.

### Why on-chain stays O(competitions)

```
   N candidates  →  N off-chain attested scores  →  N hashes + 1 settlement/epoch on-chain
   ───────────       ─────────────────────────       ──────────────────────────────────────
   unbounded         parallel sandboxes              chain writes scale with competitions,
   (Researchers)     (Node Operators)                not artifacts
```

The chain never sees an artifact, a dataset, or a trace. It sees: one competition
row, per-candidate commitment hashes + certified scores + attestation hashes, and
per-epoch settlements. Throughput is bounded by **off-chain Referee capacity**
(parallelizable by adding TEE operators), not by chain gas — re-scoring is the
expensive operation and it happens only on dispute.

---

## 10. Verifiable leaderboard + marketplace

### Verifiable leaderboard (Eigen / OpenRank bar)

A rank is **credible because it is recomputable and challengeable**, not because
a server asserts it.

- **Recompute (Public):** any third party reads on-chain per-candidate certified
  scores + attestation hashes, fetches the revealed artifacts + the public Scorer,
  re-runs, and obtains a **bit-identical ranking** (AC §9.7). Ranking is a pure
  function of stored scores; nothing is hidden in a server.
- **Recompute (Private):** the same, but to **permitted parties only** — a Private
  competition cannot offer *public* recompute (SPEC §4 caveat).
- **Challenge:** a suspect score → `CHALLENGE` (job 6) → m-of-n Validator re-score
  → slash on mismatch. The threat of slash is what makes the attested common path
  trustworthy without re-scoring everything.

```
   on-chain scores + attestation hashes
            │
            ├─▶ recompute ranks  ──▶  bit-identical leaderboard (Public)
            │
            └─▶ CHALLENGE a score ──▶ m-of-n re-score ──▶ slash faulty party
```

### Artifact marketplace

Competitions produce a stream of **certified, ranked artifacts** — natural
marketplace inventory.

- **Winning artifacts:** the top-k / record-holding artifacts, each with a
  Referee-certified `{value, ci, n}` and attestation hash. A Proposer takes the
  winner under license (enterprise bounty); a public arena publishes
  state-of-the-art.
- **Losing artifacts:** still carry a certified score. They are inventory too — a
  ranked, priced corpus of attempts. A different Proposer with a different Scorer
  may value an artifact this competition ranked low.
- **Provenance:** every marketplace item references its on-chain commitment hash,
  certified score, and attestation hash, so a buyer can verify the score is real
  (and challenge it) before paying.

The marketplace is a *read* over the settlement spine plus an off-chain artifact
store; it adds no new authoritative on-chain state beyond the per-candidate rows
already committed for settlement.

---

## 11. Trust & attestation status

Two **known gaps** are inherited/composed into this blueprint. Stating them
plainly is part of the contract: we claim what is proven, not what is aspirational.

### Gap 1 — Sandbox/TEE attestation is structural-only (inherited from L1)

| | Today | After hardening |
| --- | --- | --- |
| Enclave shape check | ✅ structure of the attestation JSON validated | ✅ |
| Hardware quote signature verification | ❌ not implemented | ✅ verify quote vs TEE vendor root of trust |
| Measurement pinning | ❌ not implemented | ✅ pin expected measurement per Scorer image, reject mismatch |
| Bind measurement → committed `attestHash` | ❌ | ✅ dispute can prove score came from expected code on genuine HW |

**Implication today:** the attestation hash proves *an enclave of the right
shape* ran — **not** that it was genuine, unmodified hardware running the expected
Scorer code. A malicious operator could, in principle, produce a structurally
valid attestation without running the real enclave. The **CHALLENGE → m-of-n
re-score → slash** backstop is the current line of defense; it makes
miscertification *catchable and punishable* even though it isn't yet
*cryptographically prevented*.

### Gap 2 — Distributed-training contribution is statistical-only (composed from training blueprint)

| | Today | After hardening |
| --- | --- | --- |
| Contribution unit | GPU-minutes | GPU-minutes |
| Proof of training | TeeLayer attests; TOPLOC state-transition hash + gradient-norm outlier checks | cryptographic / hardware-bound proof |
| Auto-slash on fake contribution | ❌ | ✅ |
| data-hash / base-model enforcement | ❌ | ✅ |

**Implication today:** in `Collaborative` (DeMo) competitions, contribution
verification is **statistical, gameable, and has no auto-slash**, and does not
enforce a data-hash or base-model. Contribution-share payouts (bps summing to
10,000, AC §9.11) are therefore as trustworthy as the statistical checks — adequate
for cooperative public runs, not yet for adversarial high-stakes ones.

### What we can claim today vs after hardening

| Claim | Today | After hardening |
| --- | --- | --- |
| Pay for certified held-out outcome, not effort | ✅ | ✅ |
| Recomputable, challengeable public leaderboard | ✅ (recompute + slash) | ✅ |
| Commit-reveal anti-copy; held-out anti-overfit | ✅ | ✅ |
| Chain footprint O(competitions) | ✅ | ✅ |
| **Cryptographically guaranteed** honest Referee | ❌ (structural attest + dispute backstop) | ✅ (verified quote + pinned measurement) |
| **Cryptographically guaranteed** Collaborative contribution | ❌ (statistical) | ✅ |

The honest framing: **today** the system is secured by *economic* guarantees
(stake + dispute + slash) layered over *structural* attestation; **after
hardening** it gains *cryptographic* guarantees (verified hardware attestation,
proof-of-training) that make the economic backstop a second line rather than the
only line. Everything in §§1–10 holds today; the two gaps above bound how strong
the trust claim can be until they close.
