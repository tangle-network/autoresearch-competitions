# Deployment & Go-Live Checklist

This is the deploy + go-live path for the autoresearch-competitions blueprint,
in three tiers: **Tier 0 local** → **Tier 1 testnet** → **Tier 2 mainnet**. Each
tier is a strict superset of the verification of the one before it.

> **The mainnet deploy is operator-run and is NOT performed by this repository.**
> Nothing here broadcasts to a live network on your behalf. The `forge script`
> commands below are the path *you* run with *your* keys. CI exercises the deploy
> script only in a non-broadcasting test (`contracts/test/Deploy.t.sol`) and the
> full lifecycle only on a local anvil devnet (`tests/e2e_lifecycle.rs`).

## Artifacts

| Artifact | Path | Role |
| --- | --- | --- |
| Service manager | `contracts/src/CompetitionManager.sol` | On-chain spine: escrow, commit-reveal, settlement. |
| Deploy script | `contracts/script/Deploy.s.sol` | `run()` deploys the manager; `register()` deploys + `createBlueprint` on Tangle. |
| Operator binary | `autoresearch-competitions-bin` → `target/release/autoresearch-competitions` | Runs the `BlueprintRunner` (router + Tangle producer/consumer + cron tick). |
| Metadata | `metadata/blueprint-metadata.json` | Explorer/UI surfaces, job table, pricing display. |
| Lifecycle E2E | `autoresearch-competitions-lib/tests/e2e_lifecycle.rs` | Full deploy→register→request→job→settle roundtrip on local anvil. |

## Tier 0 — Local devnet

Prove the whole thing works on a local anvil before any real chain.

1. **Gates green.** All six must pass:

   ```bash
   cargo build
   cargo test
   cargo clippy --tests --examples -- -D warnings
   cargo fmt --all -- --check
   forge build
   forge test
   ```

2. **Deploy via cargo-tangle (devnet).** `cargo tangle blueprint deploy` spins up a
   local anvil, deploys the Tangle core stack **including the BlueprintServiceManager
   (`CompetitionManager`)**, and registers the blueprint — you do not deploy the
   manager by hand:

   ```bash
   cargo tangle blueprint deploy tangle --network devnet
   ```

   > The `contracts/script/Deploy.s.sol` forge script is an **internal artifact** for
   > direct contract testing (`Deploy.t.sol`) and advanced manual deploys only — it is
   > not the operator path. `cargo tangle blueprint deploy` owns BSM deployment.

3. **Full-lifecycle E2E** against a seeded anvil (boots the tnt-core stack,
   registers the blueprint, requests a service, runs the operator, submits the
   real competition jobs, asserts settlement). Needs Docker + the seeded anvil
   image:

   ```bash
   cargo test -p autoresearch-competitions-lib --test e2e_lifecycle \
     -- --ignored --nocapture
   ```

   This is the centerpiece proof that the blueprint runs as a live Tangle AVS:
   `CREATE_COMPETITION → COMMIT → REVEAL → REPORT_SCORE → SETTLE` all flow as
   on-chain `JobSubmitted` events through `router()` and return the expected
   operator results, with `REPORT_SCORE` evaluating the live promotion gate.

## Tier 1 — Testnet

Everything in Tier 0, plus a real (test) chain and a real operator.

1. **Build the release binary + hash it**, then **deploy + register** with
   `cargo tangle` (this deploys the BSM and registers the blueprint definition +
   its GitHub-release source on Tangle core — no hand-rolled forge broadcast):

   ```bash
   cargo build --release -p autoresearch-competitions-bin
   SHA=$(sha256sum target/release/autoresearch-competitions | cut -d' ' -f1)

   cargo tangle blueprint deploy tangle \
     --network testnet \
     --definition ./blueprint-definition.json \
     --http-rpc-url "$HTTP_RPC_URL" --ws-rpc-url "$WS_RPC_URL" \
     --keystore-path ./keystore \
     --tangle-contract "$TANGLE_CORE" \
     --artifact-source github \
     --artifact-entrypoint ./target/release/autoresearch-competitions \
     --github-owner tangle-network --github-repo autoresearch-competitions \
     --github-tag v0.1.0 \
     --artifact-binary "autoresearch-competitions:x86_64:linux:$SHA"
   # registers the blueprint; note the BLUEPRINT_ID it logs
   ```

   The `--artifact-binary` hash is what the **blueprint-manager** validates before it
   runs your release on each operator (it must match `blueprint-definition.json`).

2. **Register as an operator** (preregistration payload + on-chain registration):
   see [`OPERATORS.md`](../OPERATORS.md). Advertise capacity, API endpoint, and
   supported scorer kinds.

3. **Request a service**, capture its `SERVICE_ID`, and start the operator binary
   with that id and your economic configuration.

4. **Smoke a single competition end-to-end** on testnet: post a small bounty,
   submit a candidate (commit→reveal), report a score that clears the gate, and
   settle. Verify on-chain with `cast` (see `OPERATORS.md` → Monitor).

## Tier 2 — Mainnet (operator-run)

Everything in Tier 1, on mainnet, with the security/readiness checklist below
signed off. **This step is performed by the operator, not by CI or this repo.**

```bash
# Operator runs this with mainnet keys + the mainnet Tangle core address.
# Same cargo-tangle flow as Tier 1, pointed at mainnet (deploys the BSM + registers).
SHA=$(sha256sum target/release/autoresearch-competitions | cut -d' ' -f1)
cargo tangle blueprint deploy tangle \
  --network mainnet \
  --definition ./blueprint-definition.json \
  --http-rpc-url "$MAINNET_HTTP_RPC" --ws-rpc-url "$MAINNET_WS_RPC" \
  --keystore-path ./keystore \
  --tangle-contract "$MAINNET_TANGLE_CORE" \
  --artifact-source github \
  --artifact-entrypoint ./target/release/autoresearch-competitions \
  --github-owner tangle-network --github-repo autoresearch-competitions \
  --github-tag "$RELEASE_TAG" \
  --artifact-binary "autoresearch-competitions:x86_64:linux:$SHA"
```

Operators then run the manager (`cargo tangle blueprint run`, see
[`OPERATORS.md`](../OPERATORS.md)) against mainnet to serve the live service.

## Security / readiness checklist

Before Tier 2, confirm each of these:

- [ ] All six fast gates green on the exact commit being deployed.
- [ ] Full-lifecycle E2E (`--ignored`) green locally.
- [ ] Deploy script reviewed; `TANGLE_CORE` and `PRIVATE_KEY` are the intended
      mainnet values (not the anvil defaults baked into the script).
- [ ] Fee split sums to 100% (enforced + unit-tested; verify any operator
      override).
- [ ] Stake floor (`STAKE_FLOOR_WEI`) and gate defaults (`GATE_*`) sized to your
      real scoring cost, not the development defaults.
- [ ] Cron schedule (`WORKFLOW_CRON_SCHEDULE`) appropriate for your deadline /
      epoch cadence; confirm `TICK` is firing in logs.
- [ ] Operator runs under a supervisor that restarts on crash, with `SERVICE_ID`
      pinned.
- [ ] `metadata/blueprint-metadata.json` reflects the deployed job set + pricing.

### Known gaps — pre-mainnet items

Two substrate-level limitations are inherited and **must be acknowledged before a
mainnet launch that handles real value** (full discussion in `docs/MECHANISM.md`
§"Known gaps" and `docs/RESEARCH.md`):

1. **Attestation is structural-only.** The TEE attestation hash proves enclave
   *shape*, not a full remote-attestation chain (inherited from agent-sandbox). A
   compromised-but-correctly-shaped enclave could in principle certify falsely;
   the m-of-n re-score is the backstop, but it is reactive (dispute-triggered),
   not preventive. Do not rely on attestation alone for high-value private-oracle
   or privileged-hardware competitions until a hardware quote-signature path
   lands.

2. **Collaborative-verification baseline.** The collaborative contribution
   estimator (Shapley-style attribution) is approximate and a coordinated ring
   submitting plausible-but-useless contributions is not provably caught. This is
   materially *improved* by the held-out gating now in place — payout still
   requires clearing the promotion gate on a held-out test, so useless
   contributions cannot extract reward by claiming credit alone — but the
   attribution *split* among genuine contributors remains a known approximation.
   Prefer Competitive structure (held-out scored) over Collaborative for the
   highest-value pools until the estimator's error is quantified.
