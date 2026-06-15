# Autoresearch Competitions — Delivery Roadmap

> **Status:** phased delivery plan for an in-development Tangle Blueprint. The
> repository currently holds a hello-world scaffold (Rust workspace `-lib`/`-bin`,
> `contracts/`, `metadata/`). This document sequences how we get from scaffold to
> mainnet. Terminology is canon from [`SPEC.md`](SPEC.md); read it first.
>
> Companion docs (some still being written): [`SPEC.md`](SPEC.md) (canonical
> terminology, jobs, scenarios), [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
> (system structure, sandbox/TEE substrate, operator/domain APIs),
> [`docs/MECHANISM.md`](docs/MECHANISM.md) (incentives, reward schedules, dispute
> math), [`docs/PRIVACY.md`](docs/PRIVACY.md) (visibility tiers, leakage bounds),
> [`docs/IMPLEMENTATION-PLAN.md`](docs/IMPLEMENTATION-PLAN.md) (file-level build
> plan). Marks: **(proposed)** = not yet decided/built; **KNOWN GAP** = an
> inherited limitation that gates a milestone.

---

## 1. Strategy in one page

We are building a decentralized market for **verifiable improvement**: pay for the
**outcome** (a certified score on a held-out test) not the **effort**. The
whole edifice rests on research's solve-hard / verify-easy asymmetry — producing a
better artifact is expensive, confirming it scored higher is one cheap run. That
asymmetry is what makes the protocol thin, privacy mostly evaporate (Researchers
see *scores, not data*), and the chain footprint stay O(competitions).

The sequencing logic, in one sentence: **ship the smallest end-to-end verifiable
competition first in the one vertical where we already own the Scorer, then widen
the four knobs one at a time.**

Three principles drive the order:

1. **Earn the Scorer before you sell the market.** The hardest, most defensible
   part of this system is a *trusted, attestable, anti-overfit Scorer*. We already
   own one for the agent-improvement vertical — the **Improvement-Plane**
   (`Surface = AgentProfile`, `Scorer = agent-eval`, held-out gate
   `min_lift_ci_lower 0.02`, `n ≥ 12`). So the first real competition is an
   agent-improvement bounty. We do not start by inventing a quantum oracle or a
   distributed-training verifier; we start where the Scorer is already production
   substrate. Every later milestone reuses the same jobs and lifecycle and only
   swaps an interface.

2. **Widen one knob at a time.** The four knobs (Structure, Cadence, Visibility,
   Scorer type) are orthogonal by design (SPEC §4). The roadmap flips them one per
   milestone so each milestone has a single, testable EXIT CRITERIA and a clean
   blast radius. M1 pins all four to their simplest setting; M3 flips Cadence; M4
   flips Visibility; M5 flips Scorer type; M6 flips Structure.

3. **Market with the open arena, monetize with private bounties.** The flagship
   public continuous arena (Scenario B, Eigen-style viral leaderboard) is the
   **marketing**; private and enterprise bounties (Scenario C) are the
   **business**. Both fall out of the *same primitive* — flip the Visibility knob.
   So we build the public arena first (it needs no privacy hardening and produces a
   credible public leaderboard for demand-gen), then add the private tier on top
   of the exact same machinery to close revenue.

The two **KNOWN GAPS** inherited from the substrate set the hard gates:
attestation is **structural-only** today (gates the Private tier, M4/M5);
distributed-training contribution verification is **statistical-only** today
(gates the Collaborative engine, M6). We sequence the monetization-critical work
(M1–M3, public + enterprise-via-held-out) *ahead of* the gap-blocked work
(M4–M6) so revenue does not wait on a research-grade attestation chain.

```
M0 ─ scaffold/interfaces
 │
M1 ─ MVP: one verifiable competition, one box  ◀── make-or-break
 │     Competitive × OneShot × Public × HeldOutEval
 │
M2 ─ trust hardening (commit-reveal, stake, challenge, slash)
 │
M3 ─ Continuous cadence  ────────────────────▶ Scenario B passes (flagship/marketing)
 │     flip Cadence
M4 ─ Private tier (TEE referee, sealed inputs) ▶ Scenario C passes (the business)
 │     flip Visibility · gated by attestation gap
M5 ─ PrivateOracle + PrivilegedHardware ──────▶ Scenario A passes (quantum)
 │     flip Scorer type · gated by attestation gap
M6 ─ Collaborative engine (DeMo) ─────────────▶ fourth knob · gated by training gap
 │     flip Structure
M7 ─ mainnet promotion (Tier 2)
```

---

## 2. Milestone table

Effort sizing: **S** ≈ 1–2 wks, **M** ≈ 3–5 wks, **L** ≈ 6–10 wks (one or two
engineers; ranges, not commitments). EXIT CRITERIA are testable and map to the
self-eval suites E1–E7 (SPEC §10) and acceptance criteria (SPEC §9) where noted.

### M0 — Scaffolding + interfaces

| | |
| --- | --- |
| **Goal** | Turn the hello-world scaffold into a typed skeleton: the four core traits, contract surfaces for the job set, and stubbed jobs that compile and revert with "unimplemented". No mechanism yet. |
| **Knobs unlocked** | none (skeleton only) |
| **Key deliverables** | • `Surface`, `Scorer`, `Engine`, `RewardSchedule` traits in `-lib` per SPEC §5 (signatures **(proposed)**, refined against tnt-core 0.13 + sandbox-runtime L1 traits).<br>• Contract skeleton in `contracts/` exposing jobs 0–7 (`CREATE_COMPETITION` … `TICK`) + inherited `PROVISION`/`DEPROVISION` as stubs.<br>• Competition state enum (`Draft → Open → Submitting → Scoring → Settling → Closed`) and the lifecycle state-machine type.<br>• `metadata/` blueprint manifest wired to the job IDs.<br>• `docs/ARCHITECTURE.md`, `docs/MECHANISM.md`, `docs/PRIVACY.md`, `docs/IMPLEMENTATION-PLAN.md` first drafts. |
| **EXIT CRITERIA** | Workspace builds (`cargo build`, `forge build`); every job stub is callable on a local devnode and reverts with a typed error; state enum + transition table land in code and match SPEC §6; trait sketches reviewed against tnt-core 0.13. No mechanism asserted to work. |
| **Effort** | M |
| **Dependencies** | tnt-core 0.13; sandbox-runtime L1 trait surface; agent-sandbox `PROVISION`/`DEPROVISION` ABI. |

### M1 — MVP: one verifiable competition, one box  ⭐ make-or-break

| | |
| --- | --- |
| **Goal** | A single agent-improvement bounty runs end-to-end on **one box**: a Proposer posts, Researchers submit candidate AgentProfiles, the Referee scores on held-out via the Improvement-Plane, settlement pays the winner. This is the milestone that proves the entire thesis — pay-for-certified-outcome — works at all. Everything after widens it. |
| **Knobs unlocked** | `Competitive × OneShot × Public × HeldOutEval` |
| **Key deliverables** | • `ImprovementPlaneScorer` impl of `Scorer` (`Surface = AgentProfile`, agent-eval replay Tiers A/B/C, held-out gate `min_lift_ci_lower 0.02`, `n ≥ 12`, model parity, state-complete).<br>• `SandboxAgentLoopEngine` reference Engine (diagnose → propose-variant → backtest → promote-if-metric+regression-free) running in the agent-sandbox **cloud/instance** mode.<br>• `RewardSchedule::TerminalPrize` and `SnapshotTopK` payout paths.<br>• Jobs `CREATE_COMPETITION`, `JOIN`, `REVEAL_CANDIDATE` (commit phase stubbed/trivial here), `REPORT_SCORE`, `FINALIZE` fully wired Surface→Scorer→settlement.<br>• Self-eval **E1** (E2E lifecycle) + **E2** (anti-overfit) as runnable campaigns. |
| **EXIT CRITERIA** | E1 passes: a full OneShot competition transitions through every state and pays the winner **to the wei** per `TerminalPrize`/`SnapshotTopK` (SPEC §9.1). E2 passes: a Researcher that overfits the dev split does **not** win — its held-out `ci.lower` < gate, no payout (SPEC §9.2). Scorer pluggability holds: settlement reads `{value, ci, cost}` and never inspects the Engine (SPEC §9.9). Reward/Cadence coherence check rejects `RecordBounty`+`OneShot` at `CREATE_COMPETITION` (SPEC §9.10). Runs on a single box (Tier 0). |
| **Effort** | L |
| **Dependencies** | M0; **Improvement-Plane / agent-eval** version pin (external — see §5); agent-sandbox cloud/instance mode. |

### M2 — Trust hardening

| | |
| --- | --- |
| **Goal** | Make the MVP adversarial-safe: commit-reveal anti-copy, staking, the dispute path, and slashing. Until this lands, the MVP is a demo, not a market. |
| **Knobs unlocked** | (hardens M1's setting; no new knob) |
| **Key deliverables** | • `COMMIT_CANDIDATE` / `REVEAL_CANDIDATE` two-phase flow with reveal-mismatch rejection.<br>• Stake on `JOIN`; escrow accounting in `CREATE_COMPETITION`.<br>• `CHALLENGE` → m-of-n re-score (default **2-of-3**, score threshold ≥ 50, mirroring the trading blueprint) with EIP-712 Validator signatures.<br>• Slashing in both directions (faulty Referee vs faulty challenger) within tolerance.<br>• Self-eval **E3** (anti-collusion / commit-reveal) + **E7** (dispute & slash). |
| **EXIT CRITERIA** | E3 passes: copying a rival's revealed artifact, sybil entries, and reveal-mismatch all earn zero or revert (SPEC §9.3). E7 passes: re-score disagreement beyond tolerance slashes the correct party in both directions; in-tolerance challenge slashes the challenger (SPEC §9.6). Every `REPORT_SCORE` carries an attestation hash or settlement reverts (SPEC §9.12 — structural-only hash here; see §5). |
| **Effort** | M |
| **Dependencies** | M1; Validator committee wiring; tnt-core dispute/slash primitives. |

### M3 — Continuous cadence → unlocks Scenario B

| | |
| --- | --- |
| **Goal** | Flip the **Cadence** knob to `Continuous`: king-of-the-hill leaderboard, marginal-lift rewards, per-epoch settlement. This unlocks the **public flagship arena** that is our marketing surface. |
| **Knobs unlocked** | `Competitive × Continuous × Public × HeldOutEval` |
| **Key deliverables** | • `RewardSchedule::RecordBounty` (marginal lift over current best, gated by `min_lift_ci_lower`) and `TimeAtTopStreaming`.<br>• `TICK` job (cron/keeper) driving epoch boundaries + streaming accrual.<br>• Continuous lifecycle loop (Settling → Submitting re-entry; record persists as new baseline).<br>• Per-epoch `SETTLE` paying `reward_per_unit_lift × max(0, new_record − prior_record)`.<br>• **Public verifiable leaderboard** recompute path (on-chain records + revealed artifacts + public Scorer → bit-identical ranking).<br>• Self-eval **E4** (continuous-leaderboard correctness).<br>• Flagship **marketing microsite**: live, forkable, challengeable moving leaderboard. |
| **EXIT CRITERIA** | E4 passes: record is monotone, sum of marginal payouts ≤ escrow, `TimeAtTopStreaming` matches epochs-at-top (SPEC §9.4). Verifiable recompute (SPEC §9.7): any third party reproduces a bit-identical ranking. `RecordBounty` pays **only** when `ci.lower − prior_record ≥ min_lift_ci_lower`; matching the record pays zero. **Scenario B passes** end-to-end (SPEC §8.B). |
| **Effort** | L |
| **Dependencies** | M2; keeper/cron infra for `TICK`; leaderboard recompute tooling; microsite. |

### M4 — Private tier → unlocks Scenario C  ⚠ gated by attestation gap

| | |
| --- | --- |
| **Goal** | Flip the **Visibility** knob to `Private`: TEE-isolated Referee, sealed inputs, privacy tiers, brokered egress, leakage bounds. This unlocks the **enterprise bounty** — the monetization case. |
| **Knobs unlocked** | `Competitive × Continuous × Private × HeldOutEval` (and `OneShot` private) |
| **Key deliverables** | • Referee runs the Scorer inside a TEE (agent-sandbox **tee-instance** mode); sealed held-out set never leaves the enclave.<br>• Privacy tiers (Black-box / Redacted-feedback / White-box no-egress) per `docs/PRIVACY.md`; default Redacted-feedback when feedback is needed.<br>• Leakage controls: score rate-limit + CI noise + held-out rotation + over-query slashing.<br>• Brokered egress for Researcher engines under access policy.<br>• Self-eval **E5** (private-tier leakage).<br>• **Attestation-gap hardening** (see §5): move from structural-only toward a verifiable remote-attestation chain, *or* an explicit documented risk acceptance + committee fallback for launch. |
| **EXIT CRITERIA** | E5 passes: information recoverable about the held-out set via repeated scoring ≤ the leakage bound (`docs/PRIVACY.md`); over-querying is rate-limited and slashable (SPEC §9.5). Private verification is to **permitted parties only** (no public recompute claim). **Scenario C passes** end-to-end (SPEC §8.C). Attestation posture is either hardened past structural-only **or** the residual risk is explicitly accepted in writing with a committee-TEE fallback. |
| **Effort** | L |
| **Dependencies** | M3; **attestation-gap hardening** (KNOWN GAP, §5); TEE/tee-instance substrate; access-policy contract surface. |

### M5 — PrivateOracle + PrivilegedHardware → unlocks Scenario A  ⚠ gated by attestation gap

| | |
| --- | --- |
| **Goal** | Flip the **Scorer type** knob: add `PrivateOracle` (hidden reference answer) and `PrivilegedHardware` (real QPU / specialized rig the Referee alone holds). Unlocks the quantum-style flagship. |
| **Knobs unlocked** | `Competitive × OneShot × Private × PrivateOracle` (+ `PrivilegedHardware`) |
| **Key deliverables** | • `PrivateOracleScorer` and `PrivilegedHardwareScorer` impls of `Scorer` (same trait, new substrate — no job/lifecycle change).<br>• Privileged-hardware Referee adapter (drives a real or licensed device; certifies fidelity/cost with `ci`).<br>• Oracle sealing so Researchers see only their score, never the reference.<br>• Demonstrator competition: open net beating a withheld circuit (mirrors the quantum flagship result). |
| **EXIT CRITERIA** | Pluggability (SPEC §9.9): swapping `HeldOutEval → PrivateOracle/PrivilegedHardware` requires **no** change to jobs, lifecycle, or settlement. **Scenario A passes** end-to-end (SPEC §8.A): a `PrivateOracle × PrivilegedHardware` competition certifies per-candidate scores + attestation and pays `TerminalPrize`. Researchers provably never see the reference. |
| **Effort** | M |
| **Dependencies** | M4 (privacy + attestation posture carries over); a privileged-hardware Referee (external partner or owned rig). |

### M6 — Collaborative engine → fourth knob  ⚠ gated by training gap

| | |
| --- | --- |
| **Goal** | Flip the last knob, **Structure**, to `Collaborative`: integrate the training-blueprint (DeMo), pool compute on one shared artifact, attribute contribution, and pay by share. Plus fleet-scale multi-instance and the artifact marketplace. |
| **Knobs unlocked** | `Collaborative × OneShot × Public × HeldOutEval` (+ private variants). `Collaborative × Continuous` (per-epoch grain) is **proposed/deferred** — see the coherence note below. |
| **Key deliverables** | • Compose the **training-blueprint** as the `DeMoTraining` Engine (distributed training of one shared checkpoint).<br>• `ContributionShare` accounting (shares sum to 10,000 bps; contribution = **held-out-gated single-permutation marginal lift**, the improvement over raw GPU-minutes — a delta that does not move held-out earns nothing).<br>• Collaborative payout path at terminal `SETTLE` (OneShot): fold each contributor's delta into the shared artifact, gate on held-out, and split the pool by marginal contribution. **Per-epoch Continuous contribution split is the proposed natural grain but is NOT shipped in M6** (a single shared artifact has no per-epoch "current best to beat marginally"; `Knobs::validate` rejects `Collaborative × Continuous`).<br>• Multi-instance / fleet scale: many sandbox instances settle the same competition identically; chain stays O(competitions).<br>• Self-eval **E6** (scale / fleet consistency).<br>• **Verifiable leaderboard UI** + **artifact marketplace** (winning artifacts under license).<br>• **Training-gap handling** (see §5): document that contribution verification is statistical-only; ship with the statistical estimator + monitoring, not a crypto proof. |
| **EXIT CRITERIA** | E6 passes: the same competition settles bit-identically across many sandbox instances; on-chain writes scale with competitions, not artifacts (SPEC §9.8). Collaborative accounting (SPEC §9.11): payout shares sum to 10,000 bps and map to measured marginal contribution. The cadence shipped for Collaborative is **OneShot** (terminal contribution split); per-epoch Continuous accounting is deferred and explicitly tracked, not claimed shipped. Contribution-verification gap is explicitly documented as statistical-only with monitoring; not claimed as cryptographically verified. |
| **Effort** | L |
| **Dependencies** | M3 (the Continuous machinery the deferred per-epoch Collaborative grain will reuse once defined); **training-blueprint** integration; **training-gap** acceptance (KNOWN GAP, §5); fleet/multi-instance infra. |

### M7 — Mainnet promotion (Tier 2)

| | |
| --- | --- |
| **Goal** | Promote from testnet to mainnet: lock economic parameters, complete audits, onboard operators, and open the market for real escrow. |
| **Knobs unlocked** | (all; production posture) |
| **Key deliverables** | • Economic params finalized (stake sizes, slash fractions, dispute fees, escrow limits, x402 service pricing).<br>• Security + economic audits of contracts and mechanism; remediation.<br>• Node Operator onboarding (slashing-for-downtime, sandbox-mode SLAs).<br>• Mainnet deploy + monitoring + incident runbooks; mirror ai-trading-blueprint deploy tiers.<br>• x402 payment rails live for service revenue. |
| **EXIT CRITERIA** | All E1–E7 self-evals green on the mainnet config. External audit passes with no unresolved high-severity findings. ≥ N independent Node Operators live (N **(proposed)**). First real-escrow competition settles on mainnet. The two KNOWN GAPS are either closed or carry signed, public risk acceptances. |
| **Effort** | L |
| **Dependencies** | M1–M6; audit vendor; operator recruiting; mainnet deploy infra. |

---

## 3. Scenario-unlock map

The three must-pass scenarios (SPEC §8) and the milestone that makes each pass.
Each scenario depends on the cumulative trust machinery, so a "passing" milestone
assumes M1–M2 hardening is already in place.

| Scenario | Knobs | Engine / Scorer | Passes at | Why that milestone |
| --- | --- | --- | --- | --- |
| **B — Public Continuous Arena** (Eigen-style) | `Competitive × Continuous × Public × HeldOutEval` | SandboxAgentLoop / Improvement-Plane | **M3** | Continuous cadence + marginal `RecordBounty`/`TimeAtTopStreaming` + public verifiable recompute land here. No privacy, no new Scorer needed → first scenario to pass. **This is the marketing flagship.** |
| **C — Private Enterprise Bounty** (the business) | `Competitive × Continuous × Private × HeldOutEval` | any Engine / `HeldOutEval`–`PrivateOracle` | **M4** | Same machinery as B + the Visibility flip: TEE referee, sealed inputs, leakage bounds. Gated by the attestation gap. **This is the revenue case.** |
| **A — Private Oracle** (quantum) | `Competitive × OneShot × Private × PrivateOracle` (+ `PrivilegedHardware`) | quantum optimizers / `PrivateOracle` + `PrivilegedHardware` | **M5** | Needs the new Scorer substrates on top of M4's privacy posture. Last to pass because it needs both the private tier *and* a privileged-hardware Referee. |

Collaborative scenarios (`Collaborative × OneShot × Public × HeldOutEval`, the M6
shipped mode; SPEC §4 coherence matrix) unlock at **M6** but are not among the three
must-pass launch scenarios. The per-epoch `Collaborative × Continuous` variant is
proposed/deferred (see the SPEC §4 note and the M6 table above).

---

## 4. Deploy-tier progression

Tiers mirror the **ai-trading-blueprint** deploy model. A tier is an
*operational posture*, not a knob; milestones graduate tiers as the mechanism
hardens.

| Tier | Posture | Milestones | What runs | Exit to next tier |
| --- | --- | --- | --- | --- |
| **Tier 0 — local single-box** | One machine, no real escrow, no committee. Fast iteration + self-evals. | **M0–M2** | Full lifecycle on one box; Referee + Engine + sandbox colocated (cloud/instance mode). | E1–E3, E7 green locally; commit-reveal + dispute paths exercised. |
| **Tier 1 — testnet multi-operator** | Multiple independent Node Operators; testnet escrow; live Validator committee; public leaderboard. | **M3–M6** | Continuous arena (M3, public flagship), private tier (M4), oracle/hardware (M5), collaborative (M6) — each rolled onto testnet as it lands. | E4–E6 green across operators; Scenarios B/C/A demonstrated on testnet; cross-instance settlement bit-identical. |
| **Tier 2 — mainnet** | Real escrow, audited contracts, economic params locked, x402 revenue live. | **M7** | All scenarios, production monitoring, operator SLAs. | First real-escrow settlement; audit clean; KNOWN GAPS closed or risk-accepted. |

Note the deliberate overlap: M3 (the public flagship) lands on **Tier 1 testnet**
early — we want the viral leaderboard live and generating demand *before* the
private tier and mainnet economics are finished. Marketing precedes monetization
by design.

---

## 5. Gating risks & dependencies

Two **KNOWN GAPS** inherited from the substrate are the dominant schedule risks.
Both are honestly load-bearing — we sequence around them rather than pretend they
are solved.

### Risk 1 — Attestation is structural-only (gates M4, M5)

**What it is.** The `attestation hash` committed with every `REPORT_SCORE` proves
the *enclave shape* (structural), not a full remote-attestation chain back to a
hardware root of trust. A sufficiently motivated Referee operator could, in
principle, run a Scorer outside the attested enclave and forge the structural
hash. Inherited from the agent-sandbox blueprint (SPEC §2, §11; ARCHITECTURE).

**Why it gates M4/M5.** The Private tier (M4) and the PrivateOracle /
PrivilegedHardware Scorers (M5) are exactly the cases where the Referee holds
something secret (sealed held-out set, hidden oracle, privileged device) and the
*only* thing protecting it is the attestation. In the Public `HeldOutEval` case
(M1–M3) the leaderboard is recomputable by anyone, so a weak attestation is far
less load-bearing — which is precisely why those milestones come first.

**Mitigation / sequencing.**
- M1–M3 ship with structural-only attestation **plus** the public-recompute
  backstop (SPEC §9.7) and the dispute/slash path (M2) — recompute, not
  attestation, is the trust anchor for public competitions.
- Before M4 ships, either (a) harden toward verifiable remote attestation, or (b)
  document an explicit risk acceptance and run the Referee as a **committee TEE**
  (m-of-n Referees must agree) so no single operator can forge undetected. The M4
  EXIT CRITERIA require one of these.
- Track upstream agent-sandbox attestation work; adopt the verifiable chain when
  available rather than building a bespoke one.

### Risk 2 — Distributed-training contribution is statistical-only (gates M6)

**What it is.** In `Collaborative` (DeMo) competitions, contribution share =
GPU-minutes, **verified statistically**, not cryptographically. A participant can
in principle over-claim compute that the statistical estimator does not catch.
Inherited from the training-blueprint (SPEC §2, §11).

**Why it gates M6.** Collaborative payout (SPEC §9.11) splits escrow by
contribution; if contribution can be gamed, the payout is gameable. Every
non-Collaborative milestone (M1–M5) is unaffected — another reason Structure is
the **last** knob we flip.

**Mitigation / sequencing.**
- M6 ships the statistical estimator **with monitoring and anomaly detection**,
  not a cryptographic proof, and the EXIT CRITERIA require this to be documented
  as such — no overclaiming "verified."
- Keep Collaborative competitions in lower-stakes / public configs first;
  defer high-value private collaborative escrow until verification improves.
- Track upstream training-blueprint verification work.

### External dependency — Improvement-Plane / agent-eval versions

The M1 Scorer is the **Improvement-Plane** (agent-eval). We depend on a specific
pinned version for: replay Tiers A/B/C semantics, the held-out gate
(`min_lift_ci_lower 0.02`, `cost_per_task_ceiling`), and the validity guards
(`n ≥ 12`, model parity, state-completeness). Risks: a breaking agent-eval API
change shifts M1; gate-parameter drift changes what "improvement" means.
**Mitigation:** pin the agent-eval version in M0, gate upgrades behind a re-run of
E2 (anti-overfit), and treat the gate parameters as part of this repo's contract.

### Other dependencies (summary)

| Dependency | Needed by | Type | Note |
| --- | --- | --- | --- |
| tnt-core 0.13 | M0 | external | Job/contract primitives, dispute/slash. |
| sandbox-runtime L1 traits | M0 | external | Trait surface to refine `Surface`/`Scorer`/`Engine` against. |
| agent-sandbox `PROVISION`/`DEPROVISION` | M0+ | composed-on | Inherited lifecycle jobs; cloud/instance/tee-instance modes. |
| Improvement-Plane / agent-eval (pinned) | M1 | external | The M1 Scorer; version-pinned. |
| Validator committee + EIP-712 | M2 | internal/infra | 2-of-3 default, threshold ≥ 50. |
| Keeper/cron for `TICK` | M3 | infra | Drives Continuous cadence + streaming. |
| Attestation-gap hardening | M4, M5 | KNOWN GAP | See Risk 1. |
| training-blueprint (DeMo) | M6 | composed | Collaborative engine; see Risk 2. |
| Audit vendor + operators | M7 | external | Mainnet gate. |

---

## 6. GTM milestones

The product strategy is one primitive, two motions, separated by a single knob
(Visibility). The roadmap deliberately ships the *marketing* surface before the
*business* surface so demand exists when the paid surface opens.

| GTM milestone | Lands at | Concrete deliverable |
| --- | --- | --- |
| **Flagship open-arena demo live** | **M3** (on Tier 1 testnet) | A public, forkable, challengeable, recomputable moving leaderboard (Scenario B) + marketing microsite. The viral demand-gen surface. Anyone can watch agents climb the leaderboard and verify the ranking themselves. |
| **First paid private bounty closes** | **M4** (Tier 1 → Tier 2) | An enterprise Proposer seals a private eval, escrows, and a Researcher earns a certified-lift payout (Scenario C). First revenue event; nothing proprietary leaks (Researchers see scores, not data). |
| **Quantum-style flagship** | **M5** | A `PrivateOracle × PrivilegedHardware` demonstrator: an open net beating a withheld circuit. High-credibility proof that the same primitive spans verticals. |
| **Mainnet open + x402 revenue** | **M7** | Real-escrow competitions and x402 service revenue live; operator marketplace. |

### Differentiation vs EigenCloud — as concrete deliverables

EigenCloud is already shipping a similar public-arena flagship. The open arena
alone is *not* our moat. Our wedge is four things EigenCloud's flagship does not
combine, each a tracked deliverable:

| Wedge | Restated as a concrete deliverable | Lands at |
| --- | --- | --- |
| **Collaborative mode** | The `Collaborative` Structure knob + DeMo engine + contribution-share payout — pooled compute on one shared artifact, not just rival rankings. | M6 |
| **Private / enterprise** | The `Private` Visibility tier: TEE referee, sealed inputs, leakage-bounded scoring — paid bounties where data never leaves the enclave. | M4 |
| **Pluggable engines** | Engine-agnostic protocol (SandboxAgentLoop / DeMoTraining / BlackBoxOptimizer / HumanSubmission) — the protocol never inspects how a candidate was produced. | M1 (proven), M6 (breadth) |
| **Causal-lift Scorer** | The Improvement-Plane: certified causal lift with a held-out gate (`min_lift_ci_lower 0.02`, `n ≥ 12`), not a raw leaderboard number — anti-overfit by construction. | M1 |

The one-line pitch the roadmap delivers: *the public arena is the ad; the private
enterprise bounty is the product; both ship from one primitive by flipping the
Visibility knob — and our Scorer certifies real, anti-overfit improvement, not a
gameable score.*

---

## 7. Out-of-scope / later

Explicitly deferred (per SPEC §11 non-goals and beyond M7). Listed so the roadmap
does not silently absorb them.

- **Cryptographically complete attestation** — full remote-attestation chain to a
  hardware root of trust. Tracked as Risk 1; out of scope until upstream
  agent-sandbox provides it. We ship structural-only + recompute/committee
  backstops.
- **Cryptographic verification of training contribution** — proof-of-compute for
  GPU-minutes. Tracked as Risk 2; M6 ships statistical-only.
- **Compute marketplace** — selling GPU-hours as the product. Researchers bring
  their own compute; we sell certified outcomes, not compute (SPEC §11).
- **General confidential compute for arbitrary code** — privacy stays bounded and
  tiered (Black-box / Redacted-feedback / White-box no-egress); a Researcher
  cannot have all of {arbitrary code, raw data, free egress} (SPEC §11, PRIVACY).
- **Subjective settlement without a Scorer** — every competition needs a Scorer,
  including `HumanPanel`; no settling on vibes (SPEC §11).
- **On-chain re-scoring as the common path** — attest once, re-score only on
  dispute. Never re-score every artifact on-chain (SPEC §11).
- **Auditing how a Researcher worked** — we never inspect an Engine; we pay for
  certified scores, not methods/hours/compute (SPEC §11).
- **`HumanPanel`-heavy and `Collaborative × HumanPanel` competitions** — coherent
  but rare (SPEC §4 coherence matrix); deferred until there is demand and the
  CI-variance / contribution-attribution issues are addressed.
- **Cross-blueprint composition beyond agent-sandbox + training** — additional
  engines (new optimizers, other training methods) are post-M7 expansion, gated by
  the pluggability proof (M1) holding.
