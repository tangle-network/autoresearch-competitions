# Autoresearch Competitions — Implementation Plan

> **Status:** build plan for the in-development Tangle Blueprint. The repo today
> is a hello-world scaffold (`autoresearch-competitions-lib` + `-bin`,
> `contracts/HelloBlueprint.sol`, `metadata/`). This document maps that scaffold
> to the M1 MVP and beyond. Cross-references: [`SPEC.md`](../SPEC.md) (canonical
> terminology, jobs §7, interfaces §5, scenarios §8, acceptance §9, evals §10),
> `docs/ARCHITECTURE.md` (system structure — *to be written*), and `ROADMAP.md`
> (delivery phases — *to be written*). Names and signatures marked **(proposed)**
> are not yet implemented and will be refined against tnt-core 0.13 and the
> sandbox-runtime L1 traits.

The reference codebases this plan models after, by absolute path:

- `~/code/ai-trading-blueprint/` — workspace shape (lib/bin/runtime/validator),
  `trading-blueprint-lib/src/{lib.rs,jobs/mod.rs}` Router wiring, `contracts/src/`
  layout, `.github/workflows/`, `EVALS.md`, operator/domain split.
- `~/code/ai-agent-sandbox-blueprint/sandbox-runtime/` — the L1 crate we consume
  as a git dependency (`SandboxProvider`/`RuntimeAdapter`, `auth`/`runtime`/`store`).
- `~/code/training-blueprint/operator/src/lib.rs` — `DistributedTrainingBSM`
  dispatch pattern (`TRAINING_JOB`/`CHECKPOINT_JOB`/`LEAVE_JOB`, coordinator).

---

## 1. Workspace & crate layout

Extends the existing two-crate scaffold into a multi-crate workspace mirroring
ai-trading-blueprint. The split rule: **`-runtime`** holds engine-agnostic core
traits and pure logic (no chain, no SDK); **`-lib`** holds thin settlement-only
job handlers + Router; **`-bin`** is the runner; **`-referee-*`** isolates the
trusted scoring path so it can run in a separate TEE process.

```
autoresearch-competitions/
├── Cargo.toml                          # [workspace] resolver=3, add members below
├── autoresearch-runtime/               # (proposed, NEW) core traits + pure state
│   └── src/
│       ├── lib.rs
│       ├── surface.rs                  # trait Surface
│       ├── scorer.rs                   # trait Scorer, Score, Split, ScorerKind
│       ├── engine.rs                   # trait Engine, DevFeedback
│       ├── reward.rs                   # RewardSchedule, ContributionShare, payout calc
│       ├── competition.rs             # Competition, Knobs, lifecycle state machine
│       ├── candidate.rs                # Candidate, Commit, Reveal
│       ├── evidence.rs                 # EvidenceLedger, certified Score + CI
│       └── coherence.rs                # §4 knob/reward coherence validation
├── autoresearch-competitions-lib/      # EXISTS — job handlers + Router (extend)
│   └── src/
│       ├── lib.rs                      # job-id consts, sol! types, router()
│       ├── jobs/
│       │   ├── mod.rs
│       │   ├── create_competition.rs   # JOB 0
│       │   ├── join.rs                 # JOB 1
│       │   ├── commit_candidate.rs     # JOB 2
│       │   ├── reveal_candidate.rs     # JOB 3
│       │   ├── report_score.rs         # JOB 4
│       │   ├── settle.rs               # JOB 5  (SETTLE/FINALIZE)
│       │   ├── challenge.rs            # JOB 6
│       │   ├── tick.rs                 # JOB 7  (cadence/epoch driver)
│       │   ├── provision.rs            # inherited, delegates to sandbox-runtime
│       │   └── deprovision.rs          # inherited
│       ├── on_chain.rs                 # contract reads/writes (model trading on_chain.rs)
│       ├── state/                      # SQLite operator state (mod §6)
│       ├── api/                        # operator :9200 + domain :9100 (§7)
│       └── tangle_compat.rs            # EvmTangleMetadataCompatLayer (copy from trading)
├── autoresearch-competitions-bin/      # EXISTS — runner (extend with cron producer)
├── autoresearch-referee-lib/           # (proposed, NEW) Scorer execution + certify
│   └── src/
│       ├── lib.rs
│       ├── certify.rs                  # run Scorer on HeldOut, emit Score+CI+attest hash
│       ├── attestation.rs              # TEE attestation evidence → hash (structural today)
│       └── scorers/                    # adapter registry
├── autoresearch-referee-bin/           # (proposed, NEW) standalone Referee TEE process
├── autoresearch-validator-lib/         # (proposed, NEW) m-of-n re-score + EIP-712 sign
├── autoresearch-validator-bin/         # (proposed, NEW)
├── engines/                            # (proposed, NEW) Engine adapter crates
│   ├── engine-sandbox-agent/           # SandboxAgentLoopEngine (drives sidecar)
│   ├── engine-demo-training/           # DeMoTrainingEngine → training-blueprint
│   ├── engine-blackbox-opt/            # BlackBoxOptimizerEngine
│   └── engine-human-submission/        # HumanSubmissionEngine
├── scorers/                            # (proposed, NEW) Scorer adapter crates
│   ├── scorer-agent-profile/           # AgentProfileScorer (local stand-in)
│   ├── scorer-private-oracle/          # PrivateOracleScorer
│   ├── scorer-privileged-hw/           # PrivilegedHardwareScorer (QPU/rig)
│   └── scorer-human-panel/             # HumanPanelScorer
├── contracts/                          # EXISTS — Solidity (foundry/soldeer, tnt-core 0.13)
│   └── src/
│       ├── blueprints/
│       │   └── CompetitionBlueprint.sol  # BlueprintServiceManagerBase (replaces Hello)
│       ├── CompetitionManager.sol
│       ├── Escrow.sol
│       ├── Leaderboard.sol
│       ├── RewardDistributor.sol
│       ├── AttestationRegistry.sol
│       ├── DisputeManager.sol
│       └── interfaces/
└── metadata/blueprint-metadata.json    # EXISTS — regenerate via cargo-tangle
```

**Extends the scaffold:** `autoresearch-competitions-lib/src/lib.rs` keeps its
`router()` shape but swaps `HELLO_JOB_ID`/`hello` for the eight competition jobs;
the empty `engines/`, `scorers/`, `autoresearch-runtime/`, and `-referee-*` crates
are new workspace members. `contracts/src/HelloBlueprint.sol` is replaced by
`CompetitionBlueprint.sol`.

Workspace `Cargo.toml` gains the new members and a `[workspace.dependencies]`
block. The sandbox-runtime git dep follows the exact trading pattern:

```toml
# autoresearch-competitions-lib/Cargo.toml
[dependencies]
sandbox-runtime = { git = "https://github.com/tangle-network/ai-agent-sandbox-blueprint.git", branch = "main" }
blueprint-sdk    = { workspace = true }
autoresearch-runtime = { path = "../autoresearch-runtime" }
```

---

## 2. Core trait definitions

All in `autoresearch-runtime` (no SDK/chain deps so they compile + unit-test fast).
These elaborate SPEC §5; signatures **(proposed)**.

```rust
// autoresearch-runtime/src/surface.rs   (proposed)
use alloy_primitives::B256 as H256;

pub trait Surface {
    type Artifact: serde::Serialize + serde::de::DeserializeOwned + Send;

    /// Stable content hash for commit-reveal and on-chain reference.
    fn artifact_hash(&self, a: &Self::Artifact) -> H256;

    /// Well-formedness + declared-surface bounds (only touches {skills,prompts,
    /// tools,memory}; size/egress caps). Fails closed.
    fn validate(&self, a: &Self::Artifact) -> Result<(), SurfaceError>;

    /// Materialize into a runnable Target the Scorer can execute.
    fn apply(&self, a: &Self::Artifact, ctx: &ApplyCtx) -> Result<Target, SurfaceError>;
}
```

```rust
// autoresearch-runtime/src/scorer.rs    (proposed)
pub enum Split { Dev, HeldOut }

#[derive(Clone, Copy)]
pub enum ScorerKind { HeldOutEval, PrivateOracle, PrivilegedHardware, HumanPanel }

pub struct Cost { pub tokens: u64, pub gpu_min: f64, pub qpu_sec: f64, pub usd_micros: u64 }

pub struct Diagnostics { pub redacted: bool, pub blob: serde_json::Value }

pub struct Score {
    pub value: f64,
    pub ci: (f64, f64),       // (lower, upper); ci.0 gates lift
    pub cost: Cost,
    pub diagnostics: Diagnostics,
    pub n: u32,               // validity guard: n >= 12
}

pub trait Scorer {
    type Artifact;
    fn kind(&self) -> ScorerKind;
    /// Dev = feedback (may be redacted to Researcher); HeldOut = settlement-only.
    fn score(&self, a: &Self::Artifact, split: Split) -> Result<Score, ScorerError>;
}
```

```rust
// autoresearch-runtime/src/engine.rs     (proposed)
pub struct DevFeedback { pub score: Option<Score>, pub redacted_diag: serde_json::Value }

/// What a Researcher runs to PRODUCE candidates. Protocol never inspects beyond
/// the emitted artifact. Implemented in the engines/* adapter crates.
pub trait Engine {
    type Artifact;
    fn produce(&mut self, feedback: Option<DevFeedback>)
        -> Result<Self::Artifact, EngineError>;
}
```

```rust
// autoresearch-runtime/src/reward.rs      (proposed)
use alloy_primitives::{Address, U256};

pub enum RewardSchedule {
    RecordBounty { reward_per_unit_lift: U256, min_lift_ci_lower: f64 }, // Continuous
    TimeAtTopStreaming { rate_per_epoch: U256 },                          // Continuous
    SnapshotTopK { k: u8, weights: Vec<U256> },                          // OneShot
    TerminalPrize { amount: U256 },                                       // OneShot
}

pub struct ContributionShare { pub researcher: Address, pub share_bps: u16 } // Σ = 10_000

impl RewardSchedule {
    /// Pure payout math (SPEC §9.4). Pays only when ci.lower − prior ≥ gate.
    pub fn payout(&self, prior_record: f64, certified: &Score, prior_at_top_epochs: u64)
        -> U256 { /* ... */ U256::ZERO }
}
```

```rust
// autoresearch-runtime/src/competition.rs (proposed)
pub enum Structure  { Competitive, Collaborative }
pub enum Cadence    { OneShot, Continuous }
pub enum Visibility { Public, Private }

pub struct Knobs { pub structure: Structure, pub cadence: Cadence,
                   pub visibility: Visibility, pub scorer: ScorerKind }

pub enum LifecycleState { Draft, Open, Submitting, Scoring, Settling, Disputed, Closed }

pub struct Competition {
    pub id: u64,
    pub proposer: Address,
    pub knobs: Knobs,
    pub scorer_ref: H256,         // sealed ref; resolved only inside Referee
    pub reward: RewardSchedule,
    pub escrow: U256,
    pub deadline: u64,
    pub epoch: u64,               // Continuous only
    pub record: f64,              // current best (held-out), 0 at open
    pub state: LifecycleState,
}

pub struct Candidate {
    pub competition: u64,
    pub researcher: Address,
    pub commit_hash: H256,        // COMMIT_CANDIDATE
    pub revealed: Option<H256>,   // artifact_hash after REVEAL_CANDIDATE
    pub certified: Option<Score>, // after REPORT_SCORE
    pub attestation: Option<H256>,
}
```

```rust
// autoresearch-runtime/src/evidence.rs    (proposed) — scorer evidence ledger
pub struct EvidenceEntry {
    pub kind: String,   // e.g. "replay_tier_b", "held_out_gate"
    pub delta: f64, pub ci: (f64, f64), pub n: u32, pub confounded: bool,
}
```

```rust
// autoresearch-runtime/src/coherence.rs   (proposed) — SPEC §4 rule
pub fn validate_coherence(k: &Knobs, r: &RewardSchedule) -> Result<(), CoherenceError> {
    use {Cadence::*, RewardSchedule::*};
    match (&k.cadence, r) {
        (OneShot,   RecordBounty{..} | TimeAtTopStreaming{..}) => Err(CoherenceError::RewardCadence),
        (Continuous, SnapshotTopK{..} | TerminalPrize{..})     => Err(CoherenceError::RewardCadence),
        _ => Ok(()),
    }
}
```

---

## 3. Smart contracts

Foundry + soldeer, `tnt-core = "0.13.0"`. The on-chain footprint is
**O(competitions)** (SPEC §9.8): only commitments, certified scores+CIs,
attestation hashes, and payouts touch the chain — never artifacts/data/traces.
The service-manager extends `tnt-core/BlueprintServiceManagerBase.sol` exactly as
`~/code/ai-trading-blueprint/contracts/src/blueprints/TradingBlueprint.sol` does.

| Contract (proposed) | Extends | Key storage | Key functions | Events |
| --- | --- | --- | --- | --- |
| **CompetitionBlueprint** (BSM) | `BlueprintServiceManagerBase` | `mapping(uint64=>Competition)`; operator set | `onJobResult` (route job results), `onRegister`, `onRequest` | `JobResultReported` |
| **CompetitionManager** | Ownable | `competitions[id]`, `knobs`, `scorerRef`, `state`, `deadline`, `epoch`, `record` | `create()`, `open()`, `tick()`, `transition()` (lifecycle guard) | `CompetitionCreated`, `StateChanged`, `EpochAdvanced` |
| **Escrow** | — | `escrowed[id]`, `stake[id][addr]` | `deposit()`, `postStake()`, `release()`, `refund()` | `Escrowed`, `Staked`, `Released` |
| **Leaderboard** | — | `record[id]`, `topK[id]`, `certified[id][cand]` (value, ciLower, attHash) | `report()` (Referee-only), `rank()`, `recompute()` view | `ScoreReported`, `RecordMoved` |
| **RewardDistributor** | — | `claimable[id][addr]`, `shareBps[id][addr]` | `settle()` (per RewardSchedule), `claim()`, `streamAccrue()` | `Settled`, `Claimed`, `MarginalPaid` |
| **AttestationRegistry** | — | `attHash[id][cand]`, `enclaveMeasurement` | `commitAttestation()`, `verifyStructural()` | `AttestationCommitted` |
| **DisputeManager** | — | `challenges[id][cand]`, `committee` (m-of-n) | `challenge()` (staked), `submitReScore()` (EIP-712), `resolve()` (slash) | `Challenged`, `ReScored`, `Resolved`, `Slashed` |

**Commit-reveal in Solidity (sketch):**

```solidity
// CompetitionManager.sol  (proposed)
mapping(uint64 => mapping(address => bytes32)) public commit;   // candidate hash
mapping(uint64 => mapping(address => bytes32)) public reveal;   // artifact_hash
mapping(uint64 => mapping(bytes32 => bool))    public seenReveal;// anti-copy (SPEC §9.3)

function commitCandidate(uint64 id, bytes32 h) external onlyEntrant(id) inState(id, Submitting) {
    require(commit[id][msg.sender] == 0, "already committed");
    commit[id][msg.sender] = h;
    emit Committed(id, msg.sender, h);
}

function revealCandidate(uint64 id, bytes calldata artifact, bytes32 salt) external {
    bytes32 a = keccak256(artifact);
    require(keccak256(abi.encodePacked(a, salt)) == commit[id][msg.sender], "mismatch");
    require(!seenReveal[id][a], "copy");   // a rival already revealed this artifact
    seenReveal[id][a] = true;
    reveal[id][msg.sender] = a;
    emit Revealed(id, msg.sender, a);
}
```

`REPORT_SCORE` reverts unless an attestation hash is supplied (SPEC §9.12);
`create()` reverts on incoherent knob/reward pairs (SPEC §9.10) by calling the
same coherence check encoded on-chain.

---

## 4. Job handlers

Router wiring copies the trading pattern verbatim
(`~/code/ai-trading-blueprint/trading-blueprint-lib/src/lib.rs` `router()`):
`Router::new().route(ID, handler.layer(tangle_layer()))` with
`EvmTangleMetadataCompatLayer<TangleLayer>`. Handlers are thin — they read the
inbound `sol!` payload, mutate on-chain state via `on_chain.rs`, and (for
heavy work) hand off to an Engine/Scorer adapter or the sandbox runtime.

```rust
// autoresearch-competitions-lib/src/lib.rs   (proposed, replaces hello)
pub const JOB_CREATE_COMPETITION: u8 = 0;
pub const JOB_JOIN: u8               = 1;
pub const JOB_COMMIT_CANDIDATE: u8   = 2;
pub const JOB_REVEAL_CANDIDATE: u8   = 3;
pub const JOB_REPORT_SCORE: u8       = 4;
pub const JOB_SETTLE: u8             = 5;
pub const JOB_CHALLENGE: u8          = 6;
pub const JOB_TICK: u8               = 7;
pub const JOB_PROVISION: u8          = 8;   // inherited from sandbox-runtime
pub const JOB_DEPROVISION: u8        = 9;   // inherited

pub fn router() -> Router {
    Router::new()
        .route(JOB_CREATE_COMPETITION, jobs::create_competition.layer(tangle_layer()))
        .route(JOB_JOIN,               jobs::join.layer(tangle_layer()))
        .route(JOB_COMMIT_CANDIDATE,   jobs::commit_candidate.layer(tangle_layer()))
        .route(JOB_REVEAL_CANDIDATE,   jobs::reveal_candidate.layer(tangle_layer()))
        .route(JOB_REPORT_SCORE,       jobs::report_score.layer(tangle_layer()))
        .route(JOB_SETTLE,             jobs::settle.layer(tangle_layer()))
        .route(JOB_CHALLENGE,          jobs::challenge.layer(tangle_layer()))
        .route(JOB_TICK,               jobs::tick)              // cron-driven, no caller
        .route(JOB_PROVISION,          jobs::provision.layer(tangle_layer()))
        .route(JOB_DEPROVISION,        jobs::deprovision.layer(tangle_layer()))
}
```

**Implementation order** (dependency-first; each maps a SPEC §7 job):

| # | Handler | Does | Depends on |
| --- | --- | --- | --- |
| 1 | `provision` / `deprovision` | Delegate to `sandbox_runtime::runtime` to stand up/tear down the per-Researcher sidecar. Thin pass-through (model trading `jobs/provision.rs`). | sandbox-runtime dep |
| 2 | `create_competition` | Validate coherence (§2 `coherence.rs`), escrow reward, write `Competition` + sealed scorer ref + knobs + schedule + deadline. `Draft→Open`. | Escrow, CompetitionManager |
| 3 | `join` | Post stake, register entrant. `Open→Submitting`. | Escrow |
| 4 | `commit_candidate` | Write artifact hash on-chain (commit phase). | CompetitionManager |
| 5 | `reveal_candidate` | Disclose artifact to Referee; enforce hash match + anti-copy `seenReveal`. | §4 commit-reveal |
| 6 | `report_score` | Referee-only: accept certified `{value,ci,cost,diag}` + attestation hash; reject if missing. `Submitting→Scoring`. | AttestationRegistry, referee-lib |
| 7 | `settle` | Rank revealed candidates; compute payouts via `RewardSchedule::payout`; write `claimable`. Continuous=per-epoch, OneShot=terminal. | RewardDistributor |
| 8 | `tick` | Cron/keeper: advance deadlines, close epochs, accrue `TimeAtTopStreaming`, trigger Continuous `settle`. Driven by the bin's CronJob producer. | settle |
| 9 | `challenge` | Staked dispute → DisputeManager m-of-n re-score → slash faulty party. `→Disputed`. | validator-lib, DisputeManager |

`tick` reuses the standalone-cron fallback pattern from trading
(`run_standalone_cron`) for local dev; production uses the `BlueprintRunner`
CronJob producer registered in `autoresearch-competitions-bin`.

---

## 5. Integration points

**sandbox-runtime (L1, git dep).** Add the git dependency (exact trading
pattern, §1). Re-export `CreateSandboxParams`, `SandboxRecord`, `SandboxState`,
`auth`, `runtime`, `store` from our lib as trading does. `provision`/`deprovision`
handlers call `sandbox_runtime::runtime` directly. Auth flow: operator API
`:9200` EIP-191 → PASETO; domain API `:9100`; TEE backends; sealed secrets — all
inherited, not reimplemented.

**SandboxAgentLoopEngine** (`engines/engine-sandbox-agent`). Drives a
per-Researcher sidecar Docker agent loop (Claude/GLM, SQLite+JSONL state, cron
ticks). The Engine `produce()` triggers one agent-loop iteration in the sidecar
and returns the emitted artifact; the protocol never inspects the loop. Wiring:
`PROVISION` creates the sidecar, the Engine pokes it via the domain API `:9100`,
the sidecar writes candidate artifacts to its store, the Researcher submits via
`COMMIT/REVEAL`.

**AgentProfileScorer adapter** (`scorers/scorer-agent-profile`). Local closed-form
stand-in for an agent evaluator. The contract:

- *In:* `Surface::Target` (an applied AgentProfile artifact) + `Split`.
- *Process:* model pass-rate dynamics from skill/prompt/tool/memory/overfit knobs;
  apply the held-out gate (`min_lift_ci_lower 0.02`); build the evidence ledger
  (`{kind, delta, ci, n, confounded}`).
- *Out:* `Score { value, ci, cost, diagnostics, n }` + validity verdict
  (`n ≥ 12` ∧ model-parity ∧ state-complete). Diagnostics redacted before they
  reach a Researcher.

A real external agent evaluator can later replace this adapter behind the same
`Scorer` seam.

**DeMoTrainingEngine dispatch** (`engines/engine-demo-training`). For
`Collaborative` competitions, dispatch to the training-blueprint's
`DistributedTrainingBSM` (model `~/code/training-blueprint/operator/src/lib.rs`:
`TRAINING_JOB`/`CHECKPOINT_JOB`/`LEAVE_JOB`, shared `TrainingCoordinator`).
Contribution = GPU-minutes → `ContributionShare` (Σ = 10_000 bps). **Known gap:**
contribution verification is statistical-only/gameable — tracked as a hardening
task (§8), not assumed solved.

**x402 for payments.** Wire x402 at the API layer for off-chain service revenue
(operator fees, Referee scoring fees, dev-split feedback queries). On-chain payouts
stay in `RewardDistributor`; x402 covers metered off-chain calls (e.g. paid
score queries on `Private` competitions, which also feed the leakage rate-limit).

**Known-gap hardening (explicit tasks, not assumptions):**

1. *Sandbox attestation is structural-only* (no hardware quote verification /
   measurement pinning). Task: extend `autoresearch-referee-lib/attestation.rs`
   to pin the enclave measurement and, when the backend supports it, verify a
   real remote-attestation quote. Until then, `AttestationRegistry.verifyStructural`
   only checks shape — documented in SPEC §11.
2. *Training contribution is statistical-only.* Task: add cross-checkpoint
   consistency proofs and GPU-minute spot-audits in `engine-demo-training`.

---

## 6. Data model

### On-chain (O(competitions); SPEC §9.8)

```solidity
struct Competition {            // CompetitionManager
    address proposer;
    uint8   structure; uint8 cadence; uint8 visibility; uint8 scorerKind; // knobs
    bytes32 scorerRef;          // sealed; resolved only inside Referee
    uint8   rewardKind; bytes rewardParams;
    uint256 escrow;
    uint64  deadline; uint64 epoch;
    int256  record;             // current best held-out, scaled fixed-point
    uint8   state;              // LifecycleState
}
mapping(uint64 => Competition) competitions;
mapping(uint64 => mapping(address => bytes32)) commit;        // commit-reveal
mapping(uint64 => mapping(address => bytes32)) reveal;
mapping(uint64 => mapping(bytes32 => bool))    seenReveal;     // anti-copy
mapping(uint64 => mapping(bytes32 => Certified)) certified;    // Leaderboard
struct Certified { int256 value; int256 ciLower; uint256 cost; bytes32 attHash; uint32 n; }
mapping(uint64 => mapping(address => uint256))  claimable;     // RewardDistributor
mapping(uint64 => mapping(address => uint256))  stake;         // Escrow
```

### Off-chain operator state (SQLite + JSONL; mirrors trading `state/`)

| Table (proposed) | Columns | Purpose |
| --- | --- | --- |
| `competitions` | id, knobs, scorer_ref, reward_json, deadline, epoch, record, state | local mirror of chain + draft pre-publish |
| `entrants` | competition_id, researcher, sandbox_id, stake, joined_at | JOIN registry |
| `candidates` | competition_id, researcher, epoch, commit_hash, artifact_hash, salt, revealed_at | commit-reveal tracking |
| `scores` | competition_id, candidate_hash, value, ci_lower, ci_upper, cost, n, attestation_hash, certified_at | Referee output cache |
| `evidence_ledger` | competition_id, candidate_hash, kind, delta, ci, n, confounded | scorer evidence (§2) |
| `query_log` | competition_id, researcher, split, ts | leakage rate-limit (Private, SPEC §9.5) |
| `payouts` | competition_id, researcher, epoch, amount, schedule_kind, tx_hash | settlement audit |
| `disputes` | competition_id, candidate_hash, challenger, stake, rescore_json, outcome | CHALLENGE trail |

JSONL logs (one per sidecar, mirroring trading): `agent-loop.jsonl`
(Engine iterations), `eval-runs.jsonl` (`EvalRunEvent`/`TraceSpanEvent`),
`settlement.jsonl` (per-epoch payout decisions for verifiable recompute).

---

## 7. API surface

Two HTTP planes inherited from sandbox-runtime, extended with competition routes.
Model after `~/code/ai-trading-blueprint/trading-http-api/`.

**Operator API `:9200`** — node-operator control, auth EIP-191 → PASETO:

| Method | Path | Purpose |
| --- | --- | --- |
| POST | `/auth/challenge`, `/auth/verify` | EIP-191 → PASETO (inherited) |
| POST | `/sandboxes` / DELETE `/sandboxes/:id` | PROVISION / DEPROVISION |
| GET | `/operator/competitions` | competitions this operator hosts |
| POST | `/operator/referee/report` | Referee submits certified score + attestation |
| GET | `/operator/health`, `/operator/telemetry` | liveness, SLIs |

**Domain API `:9100`** — Proposer/Researcher product surface, x402-metered:

| Method | Path | Purpose |
| --- | --- | --- |
| POST | `/competitions` | create (Draft); validates coherence | 
| GET | `/competitions/:id` | knobs, state, deadline, escrow |
| POST | `/competitions/:id/join` | JOIN (stake) |
| POST | `/competitions/:id/commit` | COMMIT_CANDIDATE (hash) |
| POST | `/competitions/:id/reveal` | REVEAL_CANDIDATE (artifact + salt) |
| GET | `/competitions/:id/leaderboard` | ranked records (Public: open; Private: ACL) |
| GET | `/competitions/:id/feedback` | dev-split redacted diagnostics (x402-metered; rate-limited for leakage bound) |
| GET | `/competitions/:id/score/:candidate` | certified `{value, ci, cost, attHash}` |
| POST | `/competitions/:id/challenge` | CHALLENGE (staked) |
| GET | `/competitions/:id/recompute` | Public-only verifiable leaderboard recompute (SPEC §9.7) |

Auth: domain API gates `Private` competitions by access policy; `/feedback` and
`/score` queries write `query_log` for the leakage rate-limit.

---

## 8. Testing strategy & gates

Reuse trading test patterns (`#[tokio::test]` unit tests in handlers, forge tests
for contracts, `cargo nextest`). Mirror `~/code/ai-trading-blueprint/EVALS.md`:
the self-eval suites validate **this blueprint**, not the artifacts competing in it.

**Unit** (`autoresearch-runtime`, no chain): coherence matrix (every §4 cell),
`RewardSchedule::payout` math to the wei, commit-reveal hash/anti-copy logic,
lifecycle transition guards (illegal transitions revert).

**Contract** (forge): commit-reveal mismatch reverts; `seenReveal` blocks copy;
`REPORT_SCORE` without attestation reverts; incoherent `create()` reverts;
`settle` payout sums ≤ escrow; DisputeManager slashes both directions.

**Integration** (lib + mock sandbox + mock Referee): one handler-to-chain path
per job; Engine/Scorer adapter swap leaves jobs unchanged (SPEC §9.9).

**E2E — one per reference scenario** (SPEC §8, §10):

| Eval | Scenario | Gate |
| --- | --- | --- |
| E1 lifecycle | full OneShot + full Continuous | all transitions fire; payouts match schedule to the wei |
| Scenario A | `Competitive×OneShot×Private×PrivateOracle` (quantum) | winner gets `TerminalPrize`; Researchers never see the oracle |
| Scenario B | `Competitive×Continuous×Public×HeldOutEval` (Eigen arena) | record monotone; marginal payouts = lift; recompute bit-identical (§9.7) |
| Scenario C | `Competitive×Continuous×Private×HeldOutEval` (enterprise) | redacted feedback only; leakage ≤ bound; over-query slashed |
| E2 anti-overfit | dev-split overfitter | held-out `ci.lower` < gate ⇒ no payout |
| E3 anti-collusion | copy / sybil / reveal-mismatch | all earn zero or revert |
| E5 leakage | repeated Private scoring | recovered info ≤ bound; over-query rate-limited + slashable |
| E6 scale | many sandbox instances | cross-instance settlement bit-identical; writes O(competitions) |
| E7 dispute | bad Referee + bad challenger | re-score disagreement slashes the correct party |

Validity guards on any agent-Scorer eval: `n ≥ 12`, model parity, state-complete,
held-out gate (`min_lift_ci_lower 0.02`, `cost_per_task_ceiling`).

**CI** — mirror existing `.github/workflows/`: `ci.yml` (rustfmt nightly-2024-10-13,
clippy `-D warnings`, `forge build`, `cargo nextest run`) and `foundry.yml`
(forge fmt + test). Add a gated nightly job for the E-suite once M1 lands.

---

## 9. Build/deploy

**`cargo-tangle`** builds the blueprint and regenerates
`metadata/blueprint-metadata.json` from the `sol!` job definitions + Router.
Regenerate whenever job IDs/payloads change.

**Three deploy modes** (inherited from sandbox-runtime, model trading's
`*-instance` / `*-tee-instance` crate split if per-instance binaries are needed):

| Mode | Use | Notes |
| --- | --- | --- |
| `cloud` | shared multi-tenant operator | default for Public competitions |
| `instance` | dedicated per-Proposer node | Private enterprise without TEE |
| `tee-instance` | TEE-isolated Referee + sealed secrets | required when Referee holds held-out / PrivateOracle / sealed eval |

**Env/config** (`settings.env.example` model from trading): `WORKFLOW_CRON_SCHEDULE`
(TICK cadence), operator keystore, sandbox-runtime backend selector, x402
endpoint + receiver, Referee TEE backend, RPC + contract addresses.

---

## 10. First-sprint checklist → M1 MVP

**M1 target:** `Competitive × OneShot × Public × HeldOutEval` on one box —
the classic contest path, end to end, with the agent Scorer. Ordered:

1. **Workspace skeleton.** Add `autoresearch-runtime`, `autoresearch-referee-lib`,
   `engines/engine-sandbox-agent`, `scorers/scorer-improvement-plane` as members
   in root `Cargo.toml`; add `[workspace.dependencies]`. Model:
   `~/code/ai-trading-blueprint/Cargo.toml`.
2. **Core traits compile.** Implement §2 sketches in `autoresearch-runtime`
   (no chain/SDK). Unit-test coherence + payout. Gate: `cargo nextest run -p autoresearch-runtime` green.
3. **Replace hello with jobs.** Rewrite `autoresearch-competitions-lib/src/lib.rs`
   job-id consts + `sol!` payloads + `router()` for the eight jobs. Stub handlers
   in `jobs/`. Model: `~/code/ai-trading-blueprint/trading-blueprint-lib/src/{lib.rs,jobs/mod.rs}`.
4. **Add sandbox-runtime dep + provision/deprovision.** Git dep; pass-through
   handlers. Model: `~/code/ai-agent-sandbox-blueprint/sandbox-runtime/` +
   trading `jobs/provision.rs`.
5. **Contracts: CompetitionBlueprint + CompetitionManager + Escrow + Leaderboard +
   RewardDistributor.** Replace `contracts/src/HelloBlueprint.sol`. Implement
   commit-reveal (§3) + `SnapshotTopK`/`TerminalPrize` settle. Forge tests for
   mismatch/anti-copy/no-attestation reverts. Model:
   `~/code/ai-trading-blueprint/contracts/src/blueprints/TradingBlueprint.sol`.
6. **Wire CREATE→JOIN→COMMIT→REVEAL→REPORT_SCORE→SETTLE** in lib handlers against
   on-chain.rs (model trading `on_chain.rs`). Integration test the full OneShot path.
7. **AgentProfileScorer + referee-lib.** Score an applied AgentProfile with the
   local closed-form stand-in, emit `Score`+CI, build evidence ledger, commit
   structural attestation hash. Wire `report_score` to it. A real external agent
   evaluator replaces this adapter later.
8. **SandboxAgentLoopEngine.** Drive one sidecar agent-loop iteration via domain
   API `:9100`; emit a candidate artifact. Model:
   `~/code/ai-agent-sandbox-blueprint` sidecar.
9. **Domain API `:9100` competition routes** (create/join/commit/reveal/leaderboard/
   score). Operator `:9200` referee report route. Model: trading `trading-http-api/`.
10. **E1 + Scenario-A-shaped E2E** (OneShot one-box): create → 2 researchers
    submit → referee certifies on held-out → settle pays top-1. Gate: payout to
    the wei; held-out-only settlement; attestation present.
11. **CI green.** Existing `ci.yml` + `foundry.yml` pass on the new crates;
    add E2E as a gated job.
12. **Regenerate metadata** via `cargo-tangle`; smoke-deploy `cloud` mode on testnet.

**Deferred past M1** (in ROADMAP order): Continuous cadence + `TICK` epochs
(Scenario B), Private visibility + leakage bound (Scenario C), Collaborative +
DeMoTrainingEngine, DisputeManager m-of-n + Validator crate, TEE-instance Referee
+ real attestation-quote verification (known-gap hardening §5).
