# Generalization: a generic engine for the search-loop class

How the blueprint avoids needing a bespoke `Engine` for every algorithmic-
advancement domain — and what that does (and does **not**) replace.

## The seam

The market is built on three pluggable interfaces (`autoresearch-runtime::traits`):

- **`Surface`** — what may change / how a candidate is represented.
- **`Scorer`** — how "better" is measured on a held-out split (this is the gate's input).
- **`Engine`** — the *producer*: how a candidate artifact is generated.

A "vertical" is one `(Surface, Scorer, Engine)` triple. The protocol underneath
(gate, reward, stake, dispute, slash, privacy tiers, continuous/collaborative,
marketplace) is finished and domain-agnostic.

## The change: a generic Engine for the search-loop class

A new domain *can* mean a new `Engine` — but only when its producer is not a
search loop (see the boundary below). For the broad class where "improve X" *is*
"iteratively propose-and-evaluate against a scorer," the `autoresearch-generic-engine`
crate provides one shared `Engine`:

- **`GenericEngine`** improves *any* `GenericArtifact` against *any* `Scorer` by
  running a long-horizon propose → score → keep-better loop. It is **domain-blind**:
  it searches the artifact's numeric encoding; only the `Scorer` knows the domain.
- **`SubprocessEngine`** (feature `subprocess-backend`) is a generic external-process
  backend: it shells out to a caller-supplied driver binary with a JSON manifest and
  parses the returned artifact content. Same `Engine` trait, so it is a one-line swap
  for the deterministic stand-in. This crate does not ship a driver; it is a seam for
  plugging in a real solver/prover/agent loop when one is available.

**Adding a search-loop domain is: write a `Scorer`.** Domains whose producer is
*not* a propose-and-evaluate search (a multi-node training run, a hidden-oracle
query) still get their own `Engine` — see the boundary below.

Proven in `tests/e2e_generalization.rs` — the *same* `GenericEngine` improves five
different domains, held-out:

| Domain | held-out value before → after |
| --- | --- |
| program superoptimization | +0.0014 → +0.6841 |
| combinatorial solver | +0.5599 → +0.8965 |
| theorem proving | +90.80 → +97.35 |
| agent self-improvement | +0.4375 → +0.6250 |
| forecasting | −0.8399 → −0.2334 |

Each is just a `Scorer<Artifact = GenericArtifact>` (in `autoresearch-verticals`:
`program_superopt`, `combinatorial_solver`, `theorem_proving`, `agent_improvement`,
`forecasting`) plus the **one** shared engine.

## What this does NOT do

**It does not delete the specialized engines, and it does not replace them.** Two
kinds of engine coexist:

1. **Specialized engines** — for producers that are *not* a search loop:
   - `DistributedTrainingEngine` — improvement *is* a multi-node training run on an
     external GPU cluster; the "search" is distributed gradient descent.
   - `BlackBoxOptimizerEngine` — queries a hidden reference oracle (private/quantum).
   - `FixedConfigEngine` — passthrough of a fixed submission (e.g. nanoGPT).
2. **The generic engine** (`GenericEngine`) — for the broad class where "improve
   X" *is* "iteratively propose-and-evaluate against a scorer."

They **compose** rather than compete: `GenericEngine` can search over (say)
training recipes and dispatch each candidate to `DistributedTrainingEngine` to
evaluate — outer search, inner producer.

The existing `GenericArtifact`-free verticals (`config_opt`, `nanogpt`,
`distributed_training`, the four `ScorerKind` scorers) keep their own artifact types
and engines and continue to work. Migrating them onto `GenericArtifact` +
`GenericEngine` is optional cleanup, not a requirement.

## How to add a new vertical

1. Implement `Scorer<Artifact = GenericArtifact>` for your domain (dev + held-out
   splits, a CI). Encode the domain candidate in `GenericArtifact::params` for the
   deterministic stand-in; the real artifact lives in `content` for an external
   backend plugged into `SubprocessEngine`.
2. (Optional) a domain `Surface` if `GenericSurface` is too permissive.
3. Drive it with `GenericEngine::new(researcher, start, dev_scorer, seed)` — no
   new engine. Add an e2e proving the market certifies a gate-clearing lift.

## Honest boundary

The `GenericEngine` stand-in is a *search over a numeric encoding*, and each
domain scorer is a *deterministic model* of its metric — enough to prove the market +
engine generality in CI, not a live solver/prover/forecaster. The real artifacts
(actual code, proofs, prompts, models) would be produced by an external backend
plugged into `SubprocessEngine` or a domain-specific engine. The market never trusts
what a backend returns: the Referee re-scores on held-out before any payout.
