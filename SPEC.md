# Autoresearch Competitions — Central Specification

> **Status:** design spec for an in-development Tangle Blueprint. The repository
> currently holds a hello-world scaffold (Rust workspace `-lib`/`-bin`,
> `contracts/`, `metadata/`). Everything below describes the product we are
> building. Type sketches and mechanisms marked **(proposed)** are not yet
> implemented.
>
> **This is the canonical spec.** Other documents reference its terminology
> verbatim. See also:
> [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) (system structure, sandbox/TEE
> substrate, operator/domain APIs), [`docs/MECHANISM.md`](docs/MECHANISM.md)
> (incentives, reward schedules, dispute math), [`docs/PRIVACY.md`](docs/PRIVACY.md)
> (visibility tiers, leakage bounds), and [`ROADMAP.md`](ROADMAP.md) (delivery
> phases).

---

## 1. Vision & one-liner

A decentralized market for **verifiable improvement**.

> Post a bounty for a better *anything*, measured by a test you define, on any
> cadence, public or private — the network competes or collaborates to build
> it; you pay for proven results on a leaderboard anyone can verify.

A **Proposer** posts a competition: a **Surface** to improve, a **Scorer** that
measures it, a **Reward**, and four knobs. A crowd of **Researchers** — human,
agent, or automated research loop, type-agnostic — **submit a method** (an
auto-research agent / improvement code). They do **not** bring compute and do
**not** run it: the **Node Operator provides the sandboxed compute and runs the
researcher's method** inside it, next to the proposer's sealed target. A
**Referee** runs the Scorer on a held-out measure, certifies the result, and
commits it on-chain. Payment settles for the **outcome** (a certified score), not
the **effort** (hours, GPUs, headcount).

The whole mechanism rests on research's **solve-hard / verify-easy asymmetry**:
producing a better artifact can take enormous compute and ingenuity; confirming
it scored higher on a held-out test is one cheap, reproducible run. Pricing the
outcome makes verification collapse to "run the Scorer," and makes privacy
mostly evaporate — Researchers see *scores, not data*. The Proposer's held-out
set, private oracle, or sealed eval never leaves the Referee, yet still produces
a number everyone can trust.

---

## 2. Glossary

Canon terms. Other docs use these exact names.

### Roles (five — never conflate Researcher and Node Operator)

| Term | Definition |
| --- | --- |
| **Proposer** | The demand side. Defines and posts a competition (Surface + Scorer + RewardSchedule + knobs + deadline/policy) and escrows the reward. Pays only for certified improvement. |
| **Researcher** | The supply side. **Submits a method** — an auto-research agent / improvement code that knows how to improve things — which the **Node Operator runs** on operator-provided sandboxed compute. The researcher brings the *method*, NOT the compute, and never runs it themselves. **Type-agnostic**: a human, an autonomous agent, or an automated research loop. Earns payout for top-ranked / contributing artifacts. |
| **Referee** | Runs the Scorer on the held-out measure, certifies the score, and commits it on-chain with a TEE attestation hash. Implemented as a TEE service, the Proposer itself, or a committee. The scarce trusted resource. |
| **Validator** | The dispute backstop. An m-of-n committee that re-scores a challenged result and signs the outcome (EIP-712), enabling slash. Not in the common path. |
| **Node Operator** | A Tangle infrastructure node running the blueprint binary. **Provides the sandboxed compute and RUNS the researcher's submitted method** inside it (plain Docker sandbox for no-TEE, sealed TEE enclave for TEE — a one-field toggle), then hosts Referee scoring. The operator is the compute and the referee. Operates the *plane*, not the *research*; earns operator fees. **Distinct from Researcher** — the Researcher submits the method, the Operator runs it. |

### Interfaces (four — pluggable)

| Term | Definition |
| --- | --- |
| **Surface** | What may change and how a candidate is represented and applied. Examples: agent-profile artifacts (`{skills, prompts, tools, memory}`), model weights, algorithm source, config, a product surface. |
| **Scorer** | `score(artifact, split) -> {value, ci, cost, diagnostics}`. Runs on a held-out split. May wrap an eval suite, a private oracle, privileged hardware, or a human panel. The thing being paid against. |
| **Engine** | The method that *produces* candidates: a sandboxed agent self-improvement loop, a DeMo distributed-training run, a black-box optimizer, or a raw human submission. The Researcher **submits** the Engine/method; the **Operator runs it** on operator-provided sandboxed compute (the `SandboxMethodEngine` + `SandboxHost` seam, `autoresearch-sandbox`). The method's internals are the Researcher's business; the protocol is engine-agnostic. |
| **RewardSchedule** | How escrow converts certified scores into payouts: `RecordBounty` (marginal lift over best), `TimeAtTopStreaming`, `SnapshotTopK`, `TerminalPrize`. |

### The four knobs

| Term | Definition |
| --- | --- |
| **Structure** | `Competitive` vs `Collaborative`. Whether Researchers submit separate ranked artifacts or pool compute on one shared artifact. |
| **Cadence** | `OneShot` vs `Continuous`. Whether the competition settles once at a deadline or keeps running as king-of-the-hill. |
| **Visibility** | `Public` vs `Private`. Whether the arena is open and viral or sealed behind access control. |
| **Scorer type** | `HeldOutEval` \| `PrivateOracle` \| `PrivilegedHardware` \| `HumanPanel`. The substrate the Scorer measures against. |

### Key concepts

| Term | Definition |
| --- | --- |
| **Held-out vs dev split** | The **dev split** is feedback a Researcher may see (scores, redacted diagnostics) to steer their Engine. The **held-out split** is the secret measure the Referee scores against for settlement; Researchers never touch it. Paying against held-out is what stops overfitting-to-the-test. |
| **Certified lift** | A Referee-attested improvement on the held-out measure: `value` with a confidence interval `ci`, signed with a TEE attestation hash. The unit a Proposer pays for. |
| **Marginal improvement** | In `Continuous` cadence, reward is paid for the **lift over the current best** (the record), not for absolute score. Beating the leaderboard by a hair pays; matching it pays nothing. |
| **Commit-reveal** | Two-phase submission: a Researcher first commits a hash of their artifact (`COMMIT_CANDIDATE`), then reveals it (`REVEAL_CANDIDATE`). Prevents copying a rival's revealed artifact and re-submitting it as your own. |
| **Agent-profile stand-in** | The local closed-form Scorer substrate for the agent vertical (`AgentProfileScorer`): models skill/prompt/tool/memory/overfit knobs, a held-out gate (`minLiftCiLower 0.02`), and validity guards (`n ≥ 12`, model parity, state-complete). A real external agent evaluator plugs into the same seam. |
| **Attestation hash** | A hash of the TEE attestation evidence (enclave measurement + inputs) committed on-chain alongside a certified score, so the chain stays O(competitions) while the heavy evidence lives off-chain and is re-checked only on dispute. **Known gap:** attestation is structural-only today (see §10, ARCHITECTURE). |

---

## 3. Personas (concrete)

Each persona lists **Goal / Supplies / Gets**.

### Proposer subtypes

**Enterprise with a private eval.** A company with a proprietary task and a
private held-out evaluation set.
- *Goal:* a better artifact (agent, model, config) on *their* metric without
  exposing data or hiring a research team.
- *Supplies:* a sealed `PrivateOracle` or `HeldOutEval` Scorer ref, escrow,
  `Private` visibility, access policy.
- *Gets:* certified lift on their metric; the winning artifact under license;
  nothing leaks because Researchers see scores, not data.

**Open-science host.** A lab or foundation running a public benchmark
(quantum circuits, protein folding, math).
- *Goal:* mobilize a global crowd against a hard public problem; a credible,
  verifiable leaderboard.
- *Supplies:* a `Public` `HeldOutEval` Scorer (often with a hidden test split),
  escrow or sponsor pool, `Continuous` or `OneShot` cadence.
- *Gets:* a forkable, challengeable leaderboard; published state-of-the-art;
  marketing.

**Model owner.** Owns a base model or product surface and wants it improved.
- *Goal:* squeeze marginal gains (accuracy, cost, latency) from a crowd.
- *Supplies:* the Surface (weights / config / agent-profile), a Scorer, a
  `RecordBounty` or `SnapshotTopK` schedule.
- *Gets:* a stream of certified improvements over their current best; pays only
  when the record moves.

### Researcher subtypes

**Solo agent.** A method author with one strong self-improvement agent.
- *Goal:* win top-k payouts across many competitions cheaply.
- *Supplies:* a **method** (the agent-loop Engine code) **submitted** to the
  operator; stake. They do **not** supply compute — the operator runs the method.
- *Gets:* payout for certified top-ranked artifacts; redacted dev-split
  feedback to steer.

**Auto-research firm.** A team that authors many methods.
- *Goal:* industrialize improvement across a portfolio of competitions.
- *Supplies:* diverse **methods** / Engines and methodology, submitted to the
  operator; stake. The operator provides and runs the compute, not the firm.
- *Gets:* aggregate payouts; reputation on public leaderboards.

**Collaborative method author.** Submits a training-method contribution to a
`Collaborative` competition.
- *Goal:* earn a contribution-share of a shared-artifact training run.
- *Supplies:* a **method** contribution (e.g. a `DistributedTrainingBSM` / DeMo
  training step) that the operator runs on operator compute.
- *Gets:* contribution-share payout, priced by **held-out-gated marginal
  contribution** (the collaborative runner improves on the GPU-minutes baseline,
  whose statistical-only verification is a **known gap** — see §10).

### Referee
- *Goal:* certify scores correctly and cheaply; never leak the held-out set.
- *Supplies:* a TEE-isolated scoring service that runs the Scorer on held-out
  data and emits `{value, ci, cost, diagnostics}` + attestation hash.
- *Gets:* fees per certified score; slashed if a dispute proves miscertification.

### Validator
- *Goal:* keep the system honest as a backstop.
- *Supplies:* m-of-n re-scoring capacity; EIP-712 signatures on dispute outcomes
  (default 2-of-3, score threshold ≥ 50, mirroring the trading blueprint).
- *Gets:* dispute fees / slashed-stake share; only activated on `CHALLENGE`.

### Node Operator
- *Goal:* run reliable infrastructure for the blueprint service and earn fees for hosting the
  compute.
- *Supplies:* a Tangle node running the blueprint binary; the **sandboxed compute
  that runs each researcher's submitted method** next to the proposer's sealed
  target — a plain Docker sandbox (no-TEE) or a sealed TEE enclave, selected by a
  one-field toggle (`SandboxBackend`, `autoresearch-sandbox`); and Referee scoring.
  The operator is the compute **and** the referee.
- *Gets:* operator rewards / x402 service revenue; slashed for downtime or
  faulty provisioning. **Distinct from Researcher** — the Researcher *submits* the
  method, the Operator *runs* it; the operator does not author candidates.

---

## 4. The four-knob model in full

Every competition is `(Structure, Cadence, Visibility, ScorerType)`. The knobs
are **orthogonal**: each is chosen independently. Below: the semantics of each
option, then which combinations are coherent.

### Knob 1 — Structure

| Option | Semantics |
| --- | --- |
| `Competitive` | Researchers submit **separate** artifacts. Each is scored independently; the leaderboard ranks them; payout goes to the top-k per RewardSchedule. Commit-reveal applies (anti-copy). |
| `Collaborative` | Researchers **pool compute** on **one shared artifact** (e.g. a model checkpoint trained by many GPU pools). No ranking of rival artifacts; payout is split by **contribution share**. Composes the training-blueprint engine (DeMo). |

### Knob 2 — Cadence

| Option | Semantics |
| --- | --- |
| `OneShot` | A deadline and a **terminal** payout. The competition opens, accepts submissions until the deadline, scores, settles once, closes. |
| `Continuous` | **King-of-the-hill.** The leaderboard keeps moving; reward flows for **marginal lift over the current best**. Settles per-epoch (or streaming) via `TICK`. Never terminally closes unless escrow is exhausted or the Proposer ends it. |

### Knob 3 — Visibility

| Option | Semantics |
| --- | --- |
| `Public` | Open arena. Anyone can watch, enter, and recompute the leaderboard. Viral / marketing surface. |
| `Private` | Sealed. Access-controlled entry; redacted or black-box feedback; enterprise business surface. Leakage-bounded (see PRIVACY). |

### Knob 4 — Scorer type

| Option | Semantics |
| --- | --- |
| `HeldOutEval` | Score against a held-out evaluation split (the default; e.g. an eval suite over an AgentProfile). |
| `PrivateOracle` | A hidden reference answer / oracle the Researcher cannot see (e.g. a withheld ground-truth circuit result). |
| `PrivilegedHardware` | Scoring requires hardware only the Referee has (real QPU, specialized rig, licensed simulator). |
| `HumanPanel` | A panel of human judges produces the score (subjective surfaces; design, writing, alignment). |

### Coherence matrix

Most combinations are valid. The interactions that constrain each other:

| Combination | Coherent? | Note |
| --- | --- | --- |
| `Competitive × OneShot × Public × HeldOutEval` | ✅ | Classic Kaggle-style contest. |
| `Competitive × Continuous × Public × HeldOutEval` | ✅ | Open record-bounty arena (Scenario B). |
| `Competitive × OneShot × Private × PrivateOracle` | ✅ | Quantum withheld-circuit (Scenario A). |
| `Competitive × Continuous × Private × HeldOutEval` | ✅ | Enterprise streaming bounty (Scenario C). |
| `Collaborative × OneShot × Public × HeldOutEval` | ✅ (M6) | **Shipped M6 mode.** Pool many contributors onto ONE shared artifact, fold each delta in (held-out-gated), and pay by held-out-gated single-permutation marginal contribution at a terminal settlement. This is what `Knobs::validate` accepts and `run_collaborative` implements today. |
| `Collaborative × Continuous × * × *` | 🔜 proposed | The natural-grain target: pay per-epoch by contribution as a distributed-training run streams. **Not implemented in M6** — `Knobs::validate` rejects it. It is deferred because a single shared artifact has no "current best to beat by a margin" the way the Continuous (marginal-over-best) cadence assumes, so the per-epoch contribution split needs a defined epoch boundary + per-epoch attribution that M6 does not ship. Tracked for a later milestone; until then Collaborative is OneShot. |
| `Collaborative × * × * × HumanPanel` | ⚠️ rare | A shared artifact judged by humans is possible but contribution attribution gets noisy; prefer an automatable Scorer. |
| `Competitive × * × * × HumanPanel × RecordBounty` | ⚠️ | Marginal-lift requires a metric with a stable CI; human panels have high variance, so `minLiftCiLower` gating is weak. Prefer `SnapshotTopK` with HumanPanel. |
| `RecordBounty (marginal)` with `OneShot` | ❌ nonsensical | Marginal-over-best is a *streaming* concept; with one terminal settlement there is no "current best" to beat incrementally. Use `TerminalPrize` / `SnapshotTopK` for OneShot. |
| `TimeAtTopStreaming` with `OneShot` | ❌ nonsensical | "Time held at #1" needs a clock that runs across epochs. OneShot has none. |
| `Private × Public-leaderboard recompute claim` | ⚠️ | A Private competition cannot offer *public* verifiable recompute; verification is to permitted parties only. |

**Rule:** RewardSchedule must match Cadence. `Continuous` → `RecordBounty` or
`TimeAtTopStreaming`. `OneShot` → `SnapshotTopK` or `TerminalPrize`.

---

## 5. Core interfaces (Rust-flavored, **(proposed)**)

Type sketches to anchor the implementation. Names are canon; signatures are
**(proposed)** and will be refined against tnt-core 0.13 and the sandbox-runtime
L1 traits.

```rust
/// (proposed) What may change and how a candidate is represented/applied.
pub trait Surface {
    type Artifact: Serialize + DeserializeOwned;

    /// Stable content hash for commit-reveal and on-chain reference.
    fn artifact_hash(&self, a: &Self::Artifact) -> H256;

    /// Validate an artifact is well-formed and within the declared surface
    /// (e.g. only touches {skills, prompts, tools, memory}; size/egress bounds).
    fn validate(&self, a: &Self::Artifact) -> Result<(), SurfaceError>;

    /// Materialize the artifact into a runnable target for the Scorer.
    fn apply(&self, a: &Self::Artifact, ctx: &ApplyCtx) -> Result<Target, SurfaceError>;
}

/// (proposed) Measures an artifact on a split. Runs inside the Referee TEE.
pub trait Scorer {
    type Artifact;

    fn kind(&self) -> ScorerKind; // HeldOutEval | PrivateOracle | PrivilegedHardware | HumanPanel

    /// `split` selects Dev (feedback, may be redacted to Researcher) or
    /// HeldOut (settlement-only, never exposed).
    fn score(&self, a: &Self::Artifact, split: Split) -> Result<Score, ScorerError>;
}

pub enum Split { Dev, HeldOut }

pub struct Score {
    pub value: f64,
    pub ci: (f64, f64),          // confidence interval; lower bound gates lift
    pub cost: Cost,              // tokens / GPU-min / QPU-sec / panel-cost
    pub diagnostics: Diagnostics,// redacted before reaching a Researcher
    pub n: u32,                  // sample count; validity guard n >= 12
}

/// (proposed) What a Researcher runs to PRODUCE candidates. Engine-agnostic;
/// the protocol never inspects it beyond the artifacts it emits.
pub trait Engine {
    type Artifact;
    fn produce(&mut self, feedback: Option<DevFeedback>) -> Result<Self::Artifact, EngineError>;
}
// Concrete engines (out of protocol scope): SandboxedAgentLoop, DeMoTrainingRun,
// BlackBoxOptimizer, RawHumanSubmission.

/// (proposed) Converts certified scores into payouts. Must match Cadence.
pub enum RewardSchedule {
    /// Continuous: pay the marginal lift over the current record.
    RecordBounty { reward_per_unit_lift: U256, min_lift_ci_lower: f64 },
    /// Continuous: pay proportional to time held at #1.
    TimeAtTopStreaming { rate_per_epoch: U256 },
    /// OneShot: pay ranked top-k at the deadline snapshot.
    SnapshotTopK { k: u8, weights: Vec<U256> },
    /// OneShot: single terminal prize to the winner.
    TerminalPrize { amount: U256 },
}

/// (proposed) Collaborative payout: contribution share of a shared artifact.
pub struct ContributionShare { pub researcher: Address, pub share_bps: u16 } // sum = 10_000
```

The agent-profile stand-in instantiates these for the agent case: `Surface =
AgentProfile`, `Scorer = AgentProfileScorer` — a closed-form model of agent
pass-rate dynamics with a held-out gate (`min_lift_ci_lower = 0.02`), guarded by
`n >= 12`, model parity, and state-completeness. A real external agent evaluator
plugs into the same seam.

---

## 6. Competition lifecycle (state machine)

The chain holds **settlement state only**; heavy work is off-chain.

```
                         CREATE_COMPETITION
                                 │
                                 ▼
            ┌──────────────┐  publish  ┌──────────────┐
            │    Draft     │──────────▶│     Open      │
            └──────────────┘           └──────┬───────┘
                                              │ JOIN (stake)
                                              ▼
                                   ┌──────────────────────┐
                                   │     Submitting        │
                                   │  ┌────────┐ ┌───────┐ │
                                   │  │ commit │▶│reveal │ │
                                   │  └────────┘ └───────┘ │
                                   └──────────┬───────────┘
                                  deadline / epoch boundary (TICK)
                                              ▼
                                   ┌──────────────────────┐
                                   │       Scoring         │  REPORT_SCORE
                                   │  (Referee: held-out,  │  (+ attestation)
                                   │   certify, attest)    │
                                   └──────────┬───────────┘
                                              │
                          CHALLENGE ──────────┤────────── no challenge / window passed
                              │               │
                              ▼               ▼
                       ┌────────────┐   ┌──────────────┐
                       │  Disputed  │   │   Settling    │ SETTLE/FINALIZE
                       │ (m-of-n    │   │ (rank+payout  │ (per RewardSchedule)
                       │  re-score, │   │  per schedule)│
                       │  slash)    │   └──────┬───────┘
                       └─────┬──────┘          │
                             │ resolved        │
                             └────────┬────────┘
                                      ▼
                              ┌──────────────┐
              OneShot ───────▶│    Closed     │
                              └──────────────┘

   Continuous variant: after Settling, loop back to Submitting for the next
   epoch (TICK drives the epoch boundary). Never reaches Closed until escrow
   is exhausted or the Proposer ends the competition.
```

| State | Description | Entering job |
| --- | --- | --- |
| **Draft** | Proposer has defined the competition off-chain; not yet escrowed/published. | — (off-chain) |
| **Open** | On-chain, escrowed, accepting Researchers. Knobs, sealed Scorer ref, RewardSchedule, deadline/policy are committed. | `CREATE_COMPETITION` |
| **Submitting** | Researchers join and submit. Two phases under commit-reveal: `commit` (hash on-chain) then `reveal` (artifact disclosed to Referee). | `JOIN`, `COMMIT_CANDIDATE`, `REVEAL_CANDIDATE` |
| **Scoring** | Referee runs the Scorer on the held-out split, certifies `{value, ci, cost}`, commits the attestation hash. | `REPORT_SCORE` |
| **Settling** | Rank revealed artifacts; compute payouts per RewardSchedule (marginal lift / top-k / streaming). | `SETTLE` / `FINALIZE` |
| **Disputed** | A staked `CHALLENGE` triggers m-of-n re-score; mismatch slashes the faulty party (Referee or challenger). | `CHALLENGE` |
| **Closed** | OneShot terminal state: escrow distributed, leaderboard frozen. | terminal of `FINALIZE` |
| **(Continuous loop)** | After Settling, `TICK` advances the epoch and re-enters Submitting; the record persists as the new baseline. | `TICK` |

---

## 7. Blueprint jobs

Thin, settlement-only. Heavy compute runs off-chain in sandboxes; the chain
records commitments and payouts. IDs are **(proposed)**.

| ID | Name | Caller | Effect |
| --- | --- | --- | --- |
| 0 | `CREATE_COMPETITION` | Proposer | Escrow reward; commit sealed Scorer ref + four knobs + RewardSchedule + deadline/policy. `Draft → Open`. |
| 1 | `JOIN` | Researcher | Post stake; register as an entrant. |
| 2 | `COMMIT_CANDIDATE` | Researcher | Submit artifact **hash** (commit-reveal anti-copy). |
| 3 | `REVEAL_CANDIDATE` | Researcher | Disclose the artifact matching the committed hash to the Referee. |
| 4 | `REPORT_SCORE` | Referee | Submit certified `{value, ci, cost, diagnostics}` + TEE attestation hash. `Submitting → Scoring`. |
| 5 | `SETTLE` / `FINALIZE` | Referee / anyone | Rank + pay out per RewardSchedule. Continuous settles per-epoch; OneShot terminal. |
| 6 | `CHALLENGE` | anyone (staked) | Dispute a score → m-of-n re-score + slash. `→ Disputed`. |
| 7 | `TICK` | cron / keeper | Advance deadlines, continuous epochs, streaming accrual. Drives Cadence. |
| — | `PROVISION` | Node Operator | **Inherited** from agent-sandbox lifecycle: stand up a sandbox instance. |
| — | `DEPROVISION` | Node Operator | **Inherited** from agent-sandbox lifecycle: tear down a sandbox instance. |

`PROVISION` / `DEPROVISION` are inherited from the agent-sandbox blueprint this
one is built ON; the rest are new to this blueprint. See ARCHITECTURE for the
job→contract→runtime wiring.

---

## 8. Three reference scenarios

Each is a full actor walkthrough: knobs, Engine, Scorer, RewardSchedule, and
what settles on-chain.

### A — Private Oracle (quantum withheld circuit)

**Knobs:** `Competitive × OneShot × Private × PrivateOracle`.
**Engine:** quantum circuit optimizers / classical simulators — **submitted by the
Researcher, run by the Operator** in the sandbox against the Referee's oracle.
**Scorer:** `PrivateOracle` (often also `PrivilegedHardware`) — the Referee
holds a withheld reference circuit / real QPU and a hidden ground-truth result.
**RewardSchedule:** `TerminalPrize` (or `SnapshotTopK`).

Walkthrough:
1. **Proposer** (open-science host or hardware lab) posts a circuit-optimization
   problem with a withheld reference, escrows the prize → `CREATE_COMPETITION`.
2. **Researchers** `JOIN` with stake and **submit their optimizer methods** — the
   Operator runs them on its compute (the method queries the hidden oracle only via
   the Referee and never sees the reference) — then `COMMIT_CANDIDATE` (circuit hash)
   and `REVEAL_CANDIDATE`.
3. **Referee** runs each revealed circuit against the hidden oracle on
   privileged hardware, certifies fidelity / cost with `ci`, `REPORT_SCORE` +
   attestation. Researchers never see the reference — only their score.
4. At the deadline, `FINALIZE` ranks and pays the winner `TerminalPrize`.
5. **On-chain:** escrow, the per-candidate certified score + attestation hash,
   final ranking, payout. (Edge mirrors the quantum flagship result — open net
   beating a withheld circuit by +39.9% over 72h.)

### B — Public Continuous Arena (Eigen-style)

**Knobs:** `Competitive × Continuous × Public × HeldOutEval`.
**Engine:** sandboxed agent self-improvement loops (diagnose → propose-variant →
backtest → promote-if-metric+10%-&-no-regression, walk-forward holdout).
**Scorer:** `HeldOutEval` over an AgentProfile (`AgentProfileScorer` stand-in;
held-out gate `min_lift_ci_lower 0.02`).
**RewardSchedule:** `RecordBounty` (marginal lift over current best) and/or
`TimeAtTopStreaming`.

Walkthrough:
1. **Proposer** posts a public agent benchmark, escrows a streaming pool →
   `CREATE_COMPETITION` (Continuous).
2. **Researchers** continuously `JOIN`, `COMMIT_CANDIDATE` / `REVEAL_CANDIDATE`
   each epoch; redacted dev-split feedback steers their loops.
3. **Referee** scores on held-out each epoch, `REPORT_SCORE` + attestation.
4. **`TICK`** closes each epoch; `SETTLE` pays the **marginal lift** over the
   prior record to whoever moved it, and streaming reward for time held at #1.
5. **On-chain:** a forkable, challengeable, recomputable **public leaderboard**
   (OpenRank-style), per-epoch records, marginal payouts. This is the marketing
   surface; the open arena beats a closed one on credibility.

### C — Private Enterprise Bounty

**Knobs:** `Competitive × Continuous × Private × HeldOutEval`.
**Engine:** any (agent loops, optimizers) — **submitted by the Researcher and run
by the Operator** inside a sealed TEE sandbox (the `SandboxBackend::Tee` toggle),
no-egress, next to the enterprise's sealed data.
**Scorer:** `HeldOutEval` / `PrivateOracle` over the enterprise's proprietary
task; **Redacted-feedback** privacy tier (default when feedback is needed).
**RewardSchedule:** `RecordBounty` (pay only when the record moves) +
`costPerTaskCeiling`.

Walkthrough:
1. **Enterprise Proposer** seals a private eval set, sets access policy,
   escrows → `CREATE_COMPETITION` (Private, Continuous).
2. **Permitted Researchers** `JOIN` with stake; submit under commit-reveal;
   feedback is redacted (scores + bounded diagnostics, no raw data).
3. **Referee** (enterprise-run or committee TEE) scores on the private held-out
   set, `REPORT_SCORE` + attestation. Leakage-bounded per PRIVACY (score is a
   channel → rate-limit + CI + rotation + leakage tests + slash).
4. **`TICK`** + `SETTLE` pay marginal lift over the current best each epoch.
   Verification is to **permitted parties only** (not a public recompute).
5. **On-chain:** escrow, access-controlled records, marginal payouts, dispute
   hooks. The business surface; nothing proprietary leaks because Researchers
   see scores, not data.

---

## 9. Acceptance criteria

Numbered, testable. The implementation must satisfy each.

1. **Lifecycle integrity.** A competition transitions `Draft → Open →
   Submitting → Scoring → Settling → Closed` only via the jobs in §7; no state
   is skippable and illegal transitions revert.
2. **Anti-overfit via held-out.** Settlement scores are computed on the
   **held-out** split only; a Researcher with unlimited dev-split access cannot
   raise their held-out settlement score without genuine generalization
   (validated by the anti-overfit eval, §10).
3. **Commit-reveal anti-copy.** An artifact revealed by Researcher A in epoch
   `t` cannot be committed by Researcher B for credit in epoch `t` or later;
   reveals that do not match a prior commit are rejected.
4. **Continuous marginal reward.** In `RecordBounty`, payout equals
   `reward_per_unit_lift × max(0, new_record − prior_record)` and is paid **only
   when `ci.lower − prior_record ≥ min_lift_ci_lower`**; matching the record
   pays zero.
5. **Private-tier leakage bound.** In `Private` competitions, the information a
   Researcher can extract about the held-out set via repeated scoring is bounded
   (rate-limit + CI noise + rotation); a leakage test (§10) confirms the bound
   holds and over-querying is slashable. (See PRIVACY for the formal bound.)
6. **Dispute + slash.** A `CHALLENGE` with stake forces an m-of-n
   (default 2-of-3) re-score; if re-score disagrees with the certified score
   beyond tolerance, the faulty party is slashed and the challenger rewarded;
   otherwise the challenger's stake is slashed.
7. **Verifiable leaderboard recompute.** For a `Public` competition, any third
   party can recompute the leaderboard from on-chain records + revealed
   artifacts + the public Scorer and obtain a bit-identical ranking.
8. **Multi-instance scale.** The chain footprint is **O(competitions)**, not
   O(artifacts): N candidates produce N off-chain attested scores but only their
   hashes hit the chain; competitions run across many sandbox instances
   concurrently with consistent settlement (validated by the scale eval, §10).
9. **Engine/Scorer/Surface pluggability.** Swapping the Engine (agent loop →
   optimizer) or Scorer substrate (`HeldOutEval` → `PrivateOracle`) requires no
   change to jobs, lifecycle, or settlement.
10. **Reward/Cadence coherence.** `CREATE_COMPETITION` rejects incoherent pairs
    (e.g. `RecordBounty` with `OneShot`) per the §4 matrix.
11. **Collaborative contribution accounting.** In `Collaborative` mode, payout
    shares sum to 10,000 bps and map to measured contribution (GPU-minutes for
    DeMo). (Note: contribution verification is statistical-only today — §11.)
12. **Attestation commitment.** Every `REPORT_SCORE` carries a TEE attestation
    hash; settlement without one reverts. (Note: attestation is structural-only
    today — §11.)

---

## 10. Blueprint self-eval plan

Mirrors the trading blueprint's EVALS approach: eval suites that validate
**this blueprint**, not the artifacts competing inside it. Each is a runnable
campaign with a pass gate.

| Eval suite | What it proves | Gate |
| --- | --- | --- |
| **E1 — E2E competition lifecycle** | A full `OneShot` and a full `Continuous` competition run through every state and settle correctly across the job set. | All transitions fire; payouts match RewardSchedule to the wei. |
| **E2 — Anti-overfit** | A Researcher that overfits the dev split does **not** win on held-out; held-out lift is required for payout. | Overfit candidate's held-out `ci.lower` < gate; no payout. |
| **E3 — Anti-collusion / commit-reveal** | Copying a rival's revealed artifact, sybil entries, and reveal-mismatch are all rejected or slashed. | Copy/replay attempts earn zero; reveal-mismatch reverts. |
| **E4 — Continuous-leaderboard correctness** | Across many epochs, the record is monotone, marginal payouts equal lift, and `TimeAtTopStreaming` accrues correctly. | Record never regresses; sum of marginal payouts ≤ escrow; streaming matches epochs-at-top. |
| **E5 — Private-tier leakage** | Repeated scoring against a private held-out set cannot reconstruct it beyond the bound; over-querying is rate-limited and slashable. | Recovered information ≤ leakage bound (PRIVACY); over-query slashed. |
| **E6 — Scale / fleet consistency** | The same competition state settles identically across many sandbox instances; chain footprint stays O(competitions). | Cross-instance settlement is bit-identical; on-chain writes scale with competitions, not artifacts. |
| **E7 — Dispute & slash** | `CHALLENGE` paths slash the faulty party in both directions (bad Referee, bad challenger). | Re-score disagreement → correct party slashed; tolerance honored. |

Validity guards inherited from the agent-profile stand-in apply to any
agent-Scorer eval: `n ≥ 12`, model parity across compared runs,
state-completeness, and the held-out gate (`min_lift_ci_lower 0.02`).

---

## 11. Non-goals

- **Auditing how a Researcher worked.** The protocol pays for certified scores,
  not for methods, hours, or compute. We never inspect an Engine.
- **Being a raw compute marketplace.** The Operator *does* provide the sandboxed
  compute that runs each submitted method (and charges a fee for it), but the
  **product** is **certified improvement**, not GPU-hours. We price the outcome a
  Proposer pays for, not metered compute; operator compute is the substrate, not
  the sold good.
- **General confidential compute for arbitrary code.** Privacy is bounded and
  tiered (Black-box / Redacted-feedback / White-box no-egress); a Researcher
  cannot have all of {arbitrary code, raw data access, free egress} — pick ≤ 2
  (see PRIVACY). We do not promise arbitrary-code-over-raw-data.
- **Cryptographic verification of distributed-training contribution.**
  Contribution (GPU-minutes) is verified **statistically** today — a known gap,
  not a solved problem.
- **Cryptographically complete attestation.** TEE attestation is
  **structural-only** today (enclave shape, not full remote-attestation chain) —
  a known gap inherited from the agent-sandbox blueprint.
- **Subjective scoring without a Scorer.** Every competition needs a Scorer
  (including `HumanPanel`); we do not settle on vibes or off-protocol judgment.
- **Re-scoring everything on-chain.** Held-out eval is the scarce trusted
  resource; we attest once and re-score **only on dispute**, never as the common
  path.
