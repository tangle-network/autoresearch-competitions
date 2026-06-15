# Running an Autoresearch Competitions Operator

You operate the **Referee/settlement node** for a decentralized market for
*verifiable improvement*: requesters post a bounty for a better artifact, scored
on a held-out test, and your node adjudicates submissions and drives settlement.
This is the operator runbook — prerequisites, build, register, configure, run,
monitor, and the economic model.

The on-chain spine is `CompetitionManager` (a `BlueprintServiceManagerBase`
subclass). The Rust operator binary (`autoresearch-competitions`) runs the
`BlueprintRunner`: it listens for `JobSubmitted` events, routes them to the eight
job handlers, and submits results. A cron producer fires the `TICK` job on a
schedule so deadlines and continuous epochs advance without an external poker.

> The real mainnet/testnet deploy is an **operator-run step**. Nothing in this
> repository deploys to a live network for you; the commands below are the path
> you run with your own keys. See [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md) for
> the tiered go-live checklist.

## Requirements

- Linux x86_64 host (the blueprint binary target is `Amd64`/`Linux`).
- A Rust toolchain pinned to the repo's `rust-toolchain.toml` (stable `1.91`).
- [Foundry](https://book.getfoundry.sh/) (`forge`, `cast`, `anvil`) `1.7+` for
  contract deploy + local devnet.
- An EVM key with gas on your target chain, and TNT stake on the Tangle
  restaking layer to register as an operator.
- Docker only if you run the full-lifecycle E2E (`tests/e2e_lifecycle.rs`), which
  boots a seeded anvil in a container.

## Build

```bash
cargo build --release
# operator binary: ./target/release/autoresearch-competitions
forge build   # CompetitionManager + Deploy script
```

Run the gates before shipping anything:

```bash
cargo test
cargo clippy --tests --examples -- -D warnings
cargo fmt --all -- --check
forge test
```

## Register

Registration is a two-part flow. First, the **blueprint** is registered on Tangle
core (once, by whoever owns the blueprint — see `docs/DEPLOYMENT.md`). Then each
**operator** registers against it.

The binary supports the SDK preregistration flow. In registration mode it writes
a TLV payload describing your referee capabilities (capacity, API endpoint,
supported scorer kinds) and exits before connecting to the network:

```bash
# The runner sets registration mode; the binary writes the payload and exits.
OPERATOR_MAX_CAPACITY=8 \
OPERATOR_API_ENDPOINT="https://referee.example.com" \
SUPPORTED_SCORERS="held_out_eval,private_oracle" \
./target/release/autoresearch-competitions
```

The same capabilities are also threaded into the on-chain `TangleConfig`
registration inputs when `OPERATOR_MAX_CAPACITY` is set during a normal run, so a
Dynamic-membership service learns your capacity when you join.

## Configure (environment variables)

Everything is environment-driven with sane defaults; a node runs with zero
configuration. Put overrides in a `.env` file (loaded on startup) or the systemd
unit environment.

| Variable | Default | Meaning |
| --- | --- | --- |
| `SERVICE_ID` | _(required)_ | The Tangle service id this operator serves. The binary refuses to start without it. |
| `WORKFLOW_CRON_SCHEDULE` | `0 * * * * *` | 6-field cron (with seconds) for the `TICK` job. Default: once a minute. |
| `OPERATOR_MAX_CAPACITY` | `8` | Concurrent competitions you will adjudicate. Advertised on registration. |
| `OPERATOR_API_ENDPOINT` | _(empty)_ | Public URL of your off-chain referee/scoring service. |
| `SUPPORTED_SCORERS` | _(empty)_ | Comma-separated scorer kinds you support (`held_out_eval,private_oracle,...`). |
| `GATE_MIN_LIFT_CI_LOWER` | `0.02` | Default promotion gate: minimum lower CI bound of the lift (2pp). |
| `GATE_MIN_N` | `12` | Default promotion gate: minimum paired episodes for sufficient power. |
| `GATE_COST_PER_TASK_CEILING` | _(none)_ | Optional per-task cost ceiling for the gate. Unset = uncapped. |
| `STAKE_FLOOR_WEI` | `1000000000000000` | Default researcher stake floor (anti-spam / leakage bond), in wei. |
| `FEE_SPLIT_OPERATOR` | `55` | Operator share of the fee, whole percent. |
| `FEE_SPLIT_REFEREE` | `30` | Referee share of the fee, whole percent. |
| `FEE_SPLIT_VALIDATOR` | `15` | Validator share of the fee, whole percent. |
| `DEFAULT_REWARD_SHAPE` | `snapshot_topk` | Default reward schedule shape (`terminal_prize`, `snapshot_topk`, `record_bounty`). |
| `X402_BASE_PRICE_WEI` | `1000000000000` | Wei multiplier applied to each job's price weight for x402 pricing. |
| `RUST_LOG` | _(unset)_ | Standard `tracing` env filter, e.g. `info,autoresearch_competitions=debug`. |

**The binary validates your economic config and refuses to start on a nonsensical
one.** On boot it loads every knob and then runs `EconomicConfig::validate`. The
binary hard-errors (does not start) if:

- the promotion gate has `GATE_MIN_N=0` (a zero-power gate would accept a "win"
  computed from zero episodes — the same rule the on-chain `CompetitionSpec`
  rejects), or `GATE_MIN_LIFT_CI_LOWER` is negative / non-finite, or
  `GATE_COST_PER_TASK_CEILING` is non-finite;
- `STAKE_FLOOR_WEI=0` (this would remove the anti-spam / leakage bond);
- `X402_BASE_PRICE_WEI=0` (this would make every job free and disable x402 revenue);
- the fee split does not sum to exactly 100.

**Fee split fails closed.** The three shares MUST sum to exactly 100. If your
overrides do not sum to 100 — or any `FEE_SPLIT_*` var is set but unparsable (e.g.
`256`, which is out of range) — the binary rejects the whole override and logs a
**warning naming the rejected values**, then falls back to the `55/30/15` default.
It will never run with a split that mints or burns value relative to the pool.
Watch the startup logs: a single mistyped `FEE_SPLIT_*` var discards your entire
split, and the warning is how you notice it was dropped instead of applied.

**Gate / stake / price overrides fail closed too.** A `GATE_*`, `STAKE_FLOOR_WEI`,
or `X402_BASE_PRICE_WEI` override that would be nonsensical falls back to its
default with a warning during load; `validate` is the hard floor that stops the
binary if a misconfiguration somehow survives loading. These are enforced by
`EconomicConfig::validate` / `FeeSplit::validate` and unit-tested.

## Run

Operators run the **blueprint-manager** via `cargo tangle blueprint run` — **not**
the operator binary directly. The manager watches Tangle for your service's
`JobSubmitted` events, fetches the operator binary from the registered source
(the GitHub release, hash-validated against `blueprint-definition.json`),
supervises it, and submits results back on-chain.

```bash
cargo tangle blueprint run \
  --protocol tangle \
  --http-rpc-url "$HTTP_RPC_URL" \
  --ws-rpc-url "$WS_RPC_URL" \
  --keystore-path ./keystore \
  --settings-file ./settings.env
```

`settings.env` carries `BLUEPRINT_ID`, `SERVICE_ID`, the Tangle core address, your
operator key, and the economic/runtime variables documented above. Run the manager
under systemd (or your supervisor of choice) so it restarts on crash.

For **local development** you may run the operator binary directly, bypassing the
manager — it connects to Tangle, resolves `SERVICE_ID`, and serves jobs itself:

```bash
SERVICE_ID=<your-service-id> \
WORKFLOW_CRON_SCHEDULE="0 * * * * *" \
RUST_LOG=info \
./target/release/autoresearch-competitions
```

On startup the binary logs the resolved economic configuration (gate, stake
floor, fee split, reward shape) and the cron schedule, then connects to Tangle
and begins serving jobs.

## Monitor

- **Logs** are structured `tracing` output; raise verbosity with `RUST_LOG`.
- **Cron ticks** log `Tick cron scheduled: "<schedule>"` at boot and the `TICK`
  handler runs on every fire — a missing tick log means deadlines/epochs are not
  advancing.
- **On-chain state** is the source of truth. Inspect a competition with `cast`:

  ```bash
  cast call <COMPETITION_MANAGER> \
    "competitions(uint64)(address,uint256,uint256,address,uint64,uint256,bool,bool)" \
    <COMPETITION_ID> --rpc-url <RPC_URL>
  ```

  The booleans are `exists` and `settled`; the `uint256`s are the original pool,
  the remaining escrow, and the stake floor.

## The economic model

The market splits a competition's protocol fee across three roles — **operator**
(runs the blueprint service), **referee** (runs the held-out scoring), and
**validator** (audits reported scores). The default split is **55 / 30 / 15**,
mirroring the trading blueprint's role weighting: the largest share to the
operator carrying the service, the next to the referee doing the work that makes
the result trustworthy, the remainder to validators who keep it honest. The
shares are configurable but always sum to 100%.

Per-job **x402** pricing charges requesters by job weight × `X402_BASE_PRICE_WEI`:

| Job | Weight | Rationale |
| --- | --- | --- |
| `create_competition` (0) | 100 | Escrows a pool and opens market state — heaviest. |
| `report_score` (4) | 50 | Runs/realizes the held-out scoring — medium. |
| `settle` (5) | 50 | Ranks and pays out — medium. |
| `challenge` (6) | 20 | Staked dispute. |
| `join` / `commit` / `reveal` (1/2/3) | 5 | Light submission jobs. |
| `tick` (7) | 0 | Operator cron upkeep — never billed to a requester. |

Researchers post a slashable **stake** (floor `STAKE_FLOOR_WEI`) to submit; honest
losing is never slashable, but a researcher with an unresolved challenge against
them cannot withdraw their bond. The **promotion gate** (`GATE_*`) is the quality
bar a candidate must clear to be eligible for payout: the *lower* bound of the
lift's confidence interval must be at least `GATE_MIN_LIFT_CI_LOWER` (default
0.02) with at least `GATE_MIN_N` paired episodes (default 12). This is what makes
the market pay for *verifiable* improvement rather than a lucky point estimate.
