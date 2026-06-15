![Tangle Network Banner](https://raw.githubusercontent.com/tangle-network/tangle/refs/heads/main/assets/Tangle%20%20Banner.png)

# Autoresearch Competitions

[![Discord](https://img.shields.io/badge/Discord-Join%20Chat-7289da?logo=discord&logoColor=white)](https://discord.gg/cv8EfJu3Tn)
[![Twitter](https://img.shields.io/twitter/follow/tangle_network?style=social)](https://twitter.com/tangle_network)

A Tangle Blueprint for a decentralized market in **verifiable improvement**. A
Proposer posts a competition — a Surface to improve, a Scorer that measures it,
a Reward, and a few knobs. A crowd of Researchers (humans, agents, or automated
research loops, type-agnostic) **submit a method** — an auto-research agent /
improvement code. They do **not** bring compute: the **Node Operator provides the
sandboxed compute and runs the researcher's method** inside it, next to the
proposer's sealed target — a plain Docker sandbox or a sealed TEE enclave, chosen
by a single toggle. A Referee runs the Scorer on a held-out measure and certifies
the result. Payment settles on-chain for proven improvement, on a leaderboard
anyone can verify.

> A Tangle Blueprint is a spec for an on-chain service operators run and settle on-chain:
> operators run an off-chain service and settle on-chain (tnt-core 0.13, EVM,
> x402 payments, staking/slashing). This repo currently holds a hello-world
> scaffold; these docs describe the real product we are building.

---

## Thesis

Post a bounty for a better *anything* — a better trading agent, a better quantum
circuit, a better model checkpoint — measured by a test you define, on any
cadence, public or private. The network competes or collaborates to build it,
and you pay only for proven results. The hard part of research is *producing* a
better artifact; *checking* that one artifact is better is just running the
scorer — so the market pays for the outcome and lets verification stay cheap.

> ### Pay for outcomes, not effort
>
> Research has a **solve-hard / verify-easy** asymmetry: finding a better
> artifact can take enormous compute and ingenuity, but confirming it scored
> higher on a held-out test takes one cheap, reproducible run. Pricing the
> outcome (the certified score) instead of the effort (hours, GPUs, headcount)
> makes that asymmetry the whole mechanism.
>
> This dissolves two problems at once. **Verification** collapses to "run the
> Scorer" — no need to audit how a Researcher worked. **Privacy** mostly
> evaporates because Researchers see *scores, not data*: the Proposer's
> held-out set, private oracle, or sealed eval never leaves the Referee, yet
> still produces a number everyone can trust.

---

## The four-knob model

Every competition is defined by four orthogonal knobs. Any combination is valid.

| Knob | Options | Meaning |
| --- | --- | --- |
| **Structure** | `Competitive` | Separate submissions, ranked on a leaderboard; pay the top-k. |
| | `Collaborative` | Pooled compute on one shared artifact; pay by contribution share. |
| **Cadence** | `OneShot` | A deadline and a terminal payout; settle once. |
| | `Continuous` | King-of-the-hill; the leaderboard keeps moving and reward flows for *marginal* improvement over the current best (streaming / per-epoch). |
| **Visibility** | `Public` | Open, viral arena — anyone can watch, enter, and verify. |
| | `Private` | Sealed enterprise competition behind access control. |
| **Scorer type** | `HeldOutEval` | Score against a held-out evaluation split. |
| | `PrivateOracle` | Score against a hidden reference the Proposer keeps secret. |
| | `PrivilegedHardware` | Score on hardware only the Referee can run. |
| | `HumanPanel` | Score via a panel of human judges. |

**Reference scenario A** is `Competitive × OneShot × Private × PrivateOracle`;
**B** is `Competitive × Continuous × Public × HeldOutEval`; **C** is
`Competitive × OneShot × Private × HeldOutEval`. The four knobs span the product.

---

## How it composes

The chain is the **settlement and commitment spine** — it carries
`O(competitions)`, not `O(artifacts)`. All heavy compute runs in ephemeral
sandboxes that scale out horizontally and across instances.

```
                         ┌──────────────────────────────────────────┐
   Proposer  ──posts──▶  │   On-chain settlement spine (EVM)         │
   (demand,             │   competitions · escrow · certified        │
    escrow)             │   scores · x402 payouts · disputes         │
                         └──────────────┬───────────────────────────┘
                                        │ schedules / settles
                                        ▼
  Researchers ──submit methods──▶ ─────────────────────────────────────┐
  (supply; bring the METHOD,                                            │
   NOT the compute)                                                     ▼
                         ┌──────────────────────────────────────────┐
                         │   Node Operator fleet (Tangle infra)      │
                         │   PROVIDES the compute, RUNS each method  │
                         │   + is the Referee. One-field toggle:     │
                         │   SandboxBackend = Docker (no-TEE) | Tee  │
                         └──────────────┬───────────────────────────┘
            ┌───────────────────────────┼───────────────────────────┐
            ▼                           ▼                           ▼
   ┌─────────────────┐        ┌─────────────────┐        ┌─────────────────┐
   │ OPERATOR        │        │ OPERATOR        │        │ OPERATOR        │
   │ sandbox runs    │  ...   │ sandbox runs    │  ...   │ sandbox runs    │
   │ submitted       │        │ submitted       │        │ submitted       │
   │ method          │        │ method (TEE:    │        │ method          │
   │ (Docker)        │        │ sealed+no-egr.) │        │ (Docker)        │
   └────────┬────────┘        └────────┬────────┘        └────────┬────────┘
            └───────── candidate artifacts ───────────────────────┘
                                        │
                                        ▼
                         ┌──────────────────────────────────────────┐
                         │   Referee  ──runs──▶  Scorer (held-out)   │
                         │   certifies value + CI, commits on-chain  │
                         │   Validator m-of-n backstop on dispute    │
                         └──────────────┬───────────────────────────┘
                                        ▼
                  Verifiable leaderboard  +  artifact marketplace
```

This blueprint:

- **Builds on** the [agent-sandbox blueprint](https://github.com/tangle-network) as
  the **wired operator compute** — the `SandboxHost` seam (`autoresearch-sandbox`)
  provisions a sandbox and runs each submitted method via `sandbox-runtime` (TEE
  backends Phala / Nitro / GCP / Azure, sealed secrets, cloud / instance /
  tee-instance modes). The default `LocalSandboxHost` is an in-process stand-in for
  tests; the real `SandboxRuntimeHost` is feature-gated (`autoresearch-sandbox-runtime`).
- **Mirrors** the ai-trading-blueprint patterns —
  provision / configure / start / stop / status / deprovision jobs, an
  operator-hosted sidecar Docker agent loop that runs the submitted method,
  validator m-of-n EIP-712 attestation, a self-improvement loop, and x402 pricing.
- **Composes** the training-blueprint as the `Collaborative` Engine (DeMo
  distributed training over pooled compute).
- **Uses the Improvement-Plane as the agent Scorer substrate** — certified causal
  lift on held-out data (default `minLiftCiLower` 0.02, `n ≥ 12`, replay tiers
  A/B/C, an evidence ledger).

### Core interfaces (pluggable)

| Interface | Responsibility |
| --- | --- |
| **Surface** | What may change, and how a candidate artifact is represented and applied. |
| **Scorer** | `score(artifact, split) -> {value, ci, cost, diagnostics}`; runs on held-out data. May wrap an eval suite, a private oracle, privileged hardware, or a human panel. |
| **Engine** | The method that produces candidates: a sandboxed agent self-improvement loop, a DeMo distributed-training run, a black-box optimizer, or a raw human submission. The Researcher **submits** it; the **Operator runs** it on operator-provided sandboxed compute (`SandboxMethodEngine` + `SandboxHost`). |
| **RewardSchedule** | `RecordBounty` (marginal lift over best) · `TimeAtTopStreaming` · `SnapshotTopK` · `TerminalPrize`. |

### Roles

| Role | In the market |
| --- | --- |
| **Proposer** | Demand side; posts the competition and funds escrow. |
| **Researcher** | Supply side; **submits a method** that the Operator runs. Brings the method, NOT the compute, and never runs it themselves. Human, agent, or automated loop. |
| **Referee** | Runs the Scorer, certifies results, commits them on-chain (TEE service, the Proposer, or a committee). |
| **Validator** | The m-of-n dispute backstop. |
| **Node Operator** | Tangle infra node running the blueprint binary. **Provides the sandboxed compute and RUNS the researcher's submitted method** (Docker no-TEE or sealed TEE enclave — a one-field toggle), and is the Referee. *Distinct from a Researcher: the Researcher submits the method, the Operator runs it.* |

---

## Three reference scenarios

- **A — Private Oracle (frontier science).** Improve against a hidden reference
  the Proposer never reveals (e.g. a withheld quantum circuit benchmark).
  `Competitive × OneShot × Private × PrivateOracle`.
- **B — Public Continuous Arena.** A verifiable, challengeable, moving
  leaderboard with a marketing microsite — the open arena play.
  `Competitive × Continuous × Public × HeldOutEval`.
- **C — Private Enterprise Bounty.** "Improve my agent on my sealed held-out
  eval" — the monetization motion. `Competitive × OneShot × Private × HeldOutEval`.

---

## Project structure

Proposed Rust workspace and contract layout (crate names marked *(proposed)* are
not yet implemented):

```
autoresearch-competitions/
  Cargo.toml                         # Workspace configuration
  metadata/
    blueprint-metadata.json          # Offchain blueprint metadata (IPFS/HTTPS)
  autoresearch-competitions-lib/     # Blueprint library: jobs + router
    src/lib.rs
  autoresearch-competitions-bin/     # Blueprint runner binary
    src/main.rs
  contracts/                         # Solidity: competition registry, escrow,
                                     # certified-score commitments, x402, disputes
  # Proposed crates (design phase):
  crates/
    surface/        # (proposed) Surface trait + built-in surfaces
    scorer/         # (proposed) Scorer trait; HeldOutEval/PrivateOracle/
                    #            PrivilegedHardware/HumanPanel backends
    engine/         # (proposed) Engine trait; sandbox loop, DeMo, optimizer,
                    #            human-submission adapters
    reward/         # (proposed) RewardSchedule implementations
    referee/        # (proposed) Referee service: certify + on-chain commit
    market/         # (proposed) competition lifecycle + leaderboard state
```

---

## Documentation

> These design documents are being authored now (see **Status**). Links resolve
> as each lands.

| Document | What it covers |
| --- | --- |
| [`SPEC.md`](SPEC.md) | The normative spec: knobs, interfaces, roles, on-chain types, and job ABI. |
| [`docs/RESEARCH.md`](docs/RESEARCH.md) | Market thesis, prior art, and the EigenCloud / Eigen Arena / OpenRank competitive landscape. |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | System architecture: settlement spine, operator fleet, sandboxes, Referee, and composition with the sandbox/training/Improvement-Plane substrates. |
| [`docs/MECHANISM.md`](docs/MECHANISM.md) | Incentive mechanism: reward schedules, marginal-lift pricing, anti-gaming, and dispute resolution. |
| [`docs/PRIVACY.md`](docs/PRIVACY.md) | Privacy model: scores-not-data, TEE boundaries, sealed held-out sets, and private oracles. |
| [`ROADMAP.md`](ROADMAP.md) | Phased delivery plan from scaffold to the three reference scenarios. |
| [`docs/IMPLEMENTATION-PLAN.md`](docs/IMPLEMENTATION-PLAN.md) | Crate-by-crate build plan, milestones, and test strategy. |

---

## Status

**Design phase.** The repo is a hello-world scaffold; the product above is being
specified before implementation. Specs come first
([`SPEC.md`](SPEC.md) and the `docs/` set), then implementation proceeds per
[`ROADMAP.md`](ROADMAP.md). Crate names marked *(proposed)* are subject to change
as the design lands.

---

## Prerequisites

Before you can run this project, you will need to have the following software
installed on your machine:

- [Rust 1.86+](https://www.rust-lang.org/tools/install)
- [Forge](https://getfoundry.sh) (for smart contract development)

You will also need to install [cargo-tangle](https://crates.io/crates/cargo-tangle),
our CLI tool for creating and deploying Tangle Blueprints:

```bash
cargo install cargo-tangle --git https://github.com/tangle-network/blueprint --branch v2
```

## Development

Build the project:

```sh
cargo build
```

Run tests:

```sh
cargo test
```

Deploy the blueprint to the Tangle network:

```sh
cargo tangle blueprint deploy tangle --network devnet
```

## License

Licensed under either of

* Apache License, Version 2.0
  ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
* MIT license
  ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Feedback and Contributions

We welcome feedback and contributions to improve this blueprint.
Please open an issue or submit a pull request on our GitHub repository.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
