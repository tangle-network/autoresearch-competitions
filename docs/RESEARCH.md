# Research Foundation — A Market for Verifiable Improvement

> This is the research and prior-art foundation for the `autoresearch-competitions`
> Blueprint. It states the problem, the principle the design rests on, the design
> space, the prior art we build on, and the honest threats to the thesis. The
> protocol mechanics live in [`../SPEC.md`](../SPEC.md), the system shape in
> [`./ARCHITECTURE.md`](./ARCHITECTURE.md), the incentive design in
> [`./MECHANISM.md`](./MECHANISM.md), and the confidentiality model in
> [`./PRIVACY.md`](./PRIVACY.md).

---

## 1. Abstract

Improving an artifact — an AI agent, a model, a trading strategy, an algorithm, a
chip layout, a scientific result — is a search problem. You explore a space of
possible artifacts looking for one that scores higher on some measure you care
about. The fastest way to search a large space is to throw more compute and more
*diverse* search strategies at it than any single team owns. Centralized research
labs are bounded on both: finite GPUs and a finite set of priors about what to try.
The obvious move is to decentralize the search — pay a global pool of researchers
(human, agent, or automated loop) to attack the problem in parallel — but that has
historically been blocked by a single hard question: **how do you pay strangers for
work without trusting them?** This document argues the question has been asked
backwards. The tractable formulation is not "verify the *effort*" (a research swamp
known as proof-of-useful-work) but "verify the *result*." We post a competition —
a Surface to improve, a Scorer that measures improvement on a held-out set, a
Reward, and a few knobs — and pay only for proven, certified lift. Verification
collapses to *run the scorer*, exploiting the same solve-hard / verify-easy
asymmetry that makes NP problems checkable and makes Kaggle work. As a side effect,
most of the privacy problem dissolves: researchers see scores, not data. The claim
of this repo is that a decentralized **market for verifiable improvement**, built on
a Tangle Blueprint, is both buildable today and a
distinct primitive from anything currently shipped.

---

## 2. Problem statement: improvement is search, and the bottleneck is trustless verification

### 2.1 Improvement is search

Take any artifact with a measurable quality. An agent has a task-success rate. A
model has a benchmark score. A trading strategy has a risk-adjusted return. A
quantum circuit has a fidelity. A compiler pass has a speedup. "Make it better"
means: find a point in the artifact's configuration space — its weights, its prompt,
its code, its circuit, its hyperparameters — that scores higher on a measure. That
is search.

Three facts about search drive everything below:

1. **Search rewards parallelism.** Independent searchers covering different regions
   of the space find good points faster than one searcher with the same total
   budget. This is the entire premise of evolutionary methods, ensembles, and
   hyperparameter sweeps.
2. **Search rewards diversity of priors.** The *value* of a searcher is not just
   their compute but their hypothesis distribution — what they think is worth
   trying. A reinforcement-learning PhD, a prompt-engineering hobbyist, and an
   automated architecture-search loop will probe different regions. Homogeneous
   searchers redundantly cover the same ground.
3. **Search is embarrassingly outsourceable *if you can score the output*.** You do
   not need to understand *how* a candidate was produced to know whether it is
   better. You need to measure it.

### 2.2 Centralized search is doubly bounded

A single lab is **compute-limited** (it owns a fixed cluster) and **diversity-limited**
(it hires from a narrow distribution of people and reuses a narrow set of methods).
Both bounds are structural, not budgetary — even a very rich lab cannot buy the tail
of weird, contrarian, or specialized priors held by a global crowd, and it cannot
buy compute that does not exist on its procurement timeline. The crowd, in
aggregate, has more of both.

### 2.3 The bottleneck is trustless verification

So why is research not already a global open market? Because paying strangers
requires verifying that what they did is real and valuable, and naive verification
fails in three ways:

- **Effort is unobservable and unfalsifiable.** A researcher claiming "I ran 10,000
  GPU-hours" can lie, and you cannot cheaply audit the claim. Schemes that try to
  *prove effort* (proof-of-useful-work) have to make the useful work itself the
  thing being verified, which reintroduces the original problem one level down.
- **Results can be overfit, cherry-picked, or stolen.** A researcher can tune to a
  public leaderboard (probe the test set), report their single best of many runs
  (selection bias), or resubmit someone else's artifact.
- **Sharing the problem can leak the asset.** To let a stranger work on your data,
  model, or trading edge, you seemingly have to hand it over — which enterprises
  will never do.

The thesis of this Blueprint is that all three failures are artifacts of trying to
verify the *wrong object*. Verify the result on a held-out measure, pay for the
margin of improvement, and never show the researcher the asset, and all three
collapse at once. Sections 3 and 4 make that precise.

---

## 3. The foundational principle: solve-hard / verify-easy

### 3.1 The asymmetry

Many valuable problems are **hard to solve but easy to check**. Factoring a large
number is hard; multiplying the factors back to verify is trivial. Finding a circuit
with higher fidelity is hard; running the fidelity benchmark on a submitted circuit
is cheap. Training an agent that succeeds more often is hard; replaying it on a
held-out task suite and counting successes is mechanical. This is the same
asymmetry that underpins NP, public-key cryptography, and — operationally — every
competition with a hidden test set.

The principle this repo is built on:

> **Pay for the outcome, and verification reduces to running the Scorer once.**

We call markets built this way **result-markets**, in contrast to **effort-markets**.

### 3.2 Result-markets vs effort-markets

| | Effort-market (proof-of-useful-work) | Result-market (pay-for-outcome) |
|---|---|---|
| What you pay for | That work *happened* | That the artifact *is better* |
| What you must verify | The computation was performed honestly and was useful | A score on a held-out measure |
| Verification cost | As hard as the work itself, or harder | One Scorer run + a dispute window |
| Failure modes | Faked work, useless-but-valid work, redundant work | Overfit / cherry-pick / theft — all *mechanically* defendable (held-out rotation, CI bounds, provenance) |
| Privacy | Worker must see the task in detail | Worker sees only the score |
| Status | A known research swamp | Tractable; deployed by Kaggle, Bittensor, EigenCloud |

Effort-markets are a swamp because "useful work was honestly done" has no cheap,
general certificate. The entire proof-of-useful-work literature is the search for
one, and it keeps reducing to either (a) trusted hardware you then have to trust, or
(b) redoing the work to check it. Result-markets sidestep the swamp entirely: we do
not care *how* the lift was produced, only that re-running the Scorer on a measure
the researcher could not see reproduces it.

### 3.3 The cautionary example: distributed training's statistical-verification weakness

The cleanest illustration of *why* effort-verification is hard sits inside one of
our own composed engines. The [`training-blueprint`](./ARCHITECTURE.md#collaborative-engine)
runs **DeMo** (Decoupled Momentum): nodes train a shared model, decouple momentum,
DCT-transform and **top-0.1% sparsify** their updates, and gossip the compressed
gradients over libp2p. Contribution is measured in **GPU-minutes**. The problem:
*how do you know a node actually did the GPU-minutes it claims, on the real data,
honestly?*

The state of the art is **statistical verification** — TOPLOC-style state hashes
(commit to intermediate activations/state so you can spot-check that the claimed
computation is consistent) plus gradient-norm sanity checks. This catches lazy
or broken nodes. It does **not** catch *collusion*: a ring of nodes that agree on
fabricated-but-internally-consistent updates can pass state-hash and norm checks
while contributing nothing real. We flag this as a **KNOWN GAP** (see
[`./MECHANISM.md`](./MECHANISM.md) on collaborative credit). It is exactly the
proof-of-useful-work swamp: the verification is *statistical and gameable*, because
the object being verified (honest, useful effort) has no cheap exact certificate.

### 3.4 The clean alternative: competitive result-scoring

Now contrast the **Competitive** structure on a **HeldOutEval** Scorer. A researcher
submits an artifact. The Referee runs the Scorer on a held-out split the researcher
never saw and returns `{value, ci, cost, diagnostics}`. Either the score on hidden
data went up by more than the confidence interval allows by chance, or it did not.
There is no "did they really do the work" question — the work *is* the artifact, and
the artifact *is* re-scored. Collusion buys you nothing, because the held-out score
does not care who you colluded with. This is why the competitive, result-scored
modes are the load-bearing core of the design and the collaborative effort-scored
mode is the part with an open research gap.

The lesson generalizes: **wherever we can score the result on held-out data, do that;
wherever we are forced to score effort, treat the verification as statistical, weak,
and a research frontier — not a solved problem.**

---

## 4. Pay-for-outcome dissolves most of the privacy problem

A second, less obvious consequence of result-markets: they make confidentiality
mostly free.

In an effort-market the worker must *see the task* to do it — your data, your model
internals, your trading signal. In a result-market the worker needs only:

1. A **Surface** describing the shape of the problem (what an artifact looks like,
   what the I/O contract is) — not the private asset itself.
2. A **score** for each candidate they submit.

The private asset — the held-out evaluation set, the proprietary oracle, the
enterprise data, the hidden benchmark — lives entirely Referee-side and is run
*against* the researcher's artifact, never shipped *to* the researcher. The
researcher learns "your candidate scored 0.83," nothing more. This is precisely the
**Kaggle hidden-test-set** insight, generalized and made the default: contestants
optimize against a measure they cannot inspect.

So the bulk of the privacy problem evaporates by construction. What remains is the
**residual leakage** through the score channel itself:

- **Probing / membership inference**: enough scored queries can reconstruct
  information about the held-out set (the reason Kaggle rate-limits submissions).
- **Covert channels**: a Scorer that returns rich `diagnostics` can leak more than
  intended.
- **Score-as-oracle**: a determined adversary treats the Scorer as a black-box
  function to invert.

These are real but *bounded* and *engineerable* — submission budgets, rate limits,
held-out rotation, CI-only feedback, differential-privacy noise on scores, and TEE
isolation of the Scorer. The full treatment, including the threat model and the
quantitative leakage bounds, is in [`./PRIVACY.md`](./PRIVACY.md). The point here is
architectural: **outcome-pricing turns privacy from a wall into a leak-budget.**

---

## 5. The four-knob taxonomy as a research framing

Result-markets are not one mechanism; they are a family. We claim the family is
spanned by four **orthogonal** knobs. Orthogonal means any combination is coherent —
you can turn each independently and get a valid, distinct market. (Mechanics in
[`./MECHANISM.md`](./MECHANISM.md); types in [`../SPEC.md`](../SPEC.md).)

| Knob | Values | What it controls |
|---|---|---|
| **Structure** | `Competitive` \| `Collaborative` | Do researchers race for the same prize, or pool work toward one shared artifact? |
| **Cadence** | `OneShot` \| `Continuous` | A single deadline-and-payout, or a king-of-the-hill stream paying the **marginal** lift over the current best? |
| **Visibility** | `Public` \| `Private` | Is the competition an open arena, or a closed enterprise bounty? |
| **Scorer type** | `HeldOutEval` \| `PrivateOracle` \| `PrivilegedHardware` \| `HumanPanel` | How is "better" measured and certified? |

The research value of this framing is that it **places real systems as points in a
shared space**, which both clarifies what each proves and shows the regions nobody
occupies yet.

| System | Structure | Cadence | Visibility | Scorer | Note |
|---|---|---|---|---|---|
| **Kaggle** | Competitive | OneShot (mostly) | Public | HeldOutEval (hidden test set) | The canonical held-out result-market; rate-limits to bound probing. |
| **Bittensor** | Competitive | Continuous | Public | varies by subnet | Continuous emissions by *measured contribution*; proves the marginal-reward / king-of-the-hill cadence at scale. |
| **Eigen Arena** | Competitive | Continuous | Public | PrivateOracle (verifiable trading P&L) | Verifiable AI trading competition (Recall, Dec 2025); proves a verifiable continuous arena on EigenCloud. |
| **Eigen quantum flagship** | Competitive | OneShot→Continuous | Public | PrivateOracle (withheld circuit benchmark) | Open crowd beat Google's *withheld* benchmark; proves open+agentic+verifiable beats closed on frontier science. |
| **Distributed training (DeMo)** | **Collaborative** | Continuous | Public | effort (GPU-minutes, statistical) | The one collaborative point; also the one with weak (gameable) verification — see §3.3. |
| **Enterprise bounty (this repo)** | Competitive | OneShot or Continuous | **Private** | HeldOutEval / PrivateOracle | The under-occupied region: flip Visibility to private and the arena becomes a business. |

Two observations fall straight out of the table:

- **The `Collaborative` column is nearly empty**, and the one entry has the weakest
  verification. Collaborative frontier-improvement with sound credit assignment is an
  open frontier (and our `training-blueprint` engine, §6.5).
- **The `Private` row is nearly empty among shipped *open* systems**, because open
  arenas are marketing. Private is where enterprises pay. Same primitive, flip one
  knob (§9 of this repo's thesis; see [`./MECHANISM.md`](./MECHANISM.md)).

This is the differentiation thesis in one picture: existing systems cluster in the
`Competitive / Public` quadrant; the business and the frontier-science upside both
live in the quadrants they have left open.

---

## 6. Prior art & landscape

For each system: **what it proves**, **what it lacks**, **what we borrow**.

### 6.1 ML competitions / Kaggle

- **Proves**: A hidden test set with a private leaderboard prevents overfitting to
  the measure, and submission rate-limits prevent probing the held-out data. This is
  the original, battle-tested result-market: contestants are paid (in prizes / rank)
  for a score on data they cannot see.
- **Lacks**: Centralized, off-chain, no programmable settlement, no continuous
  marginal reward, no privacy guarantee beyond "trust Kaggle to hold the test set,"
  no composition with arbitrary compute engines.
- **We borrow**: The held-out-measure-as-Scorer pattern, the private leaderboard,
  and rate-limiting as the canonical anti-probing defense. The `HeldOutEval` Scorer
  type *is* Kaggle's mechanism, made on-chain, programmable, and private-capable.

### 6.2 Bittensor — continuous incentive

- **Proves**: A decentralized network can pay a continuous emission stream to
  participants ranked by *measured contribution*, sustaining a live king-of-the-hill
  market rather than a one-shot prize. Demonstrates the `Continuous` cadence at
  network scale with crypto-economic settlement.
- **Lacks**: Per-subnet scoring quality varies widely and is often gameable; the
  "measured contribution" is frequently an effort/quality proxy, not a held-out
  result, which reopens the verification swamp. No general held-out-gate discipline.
- **We borrow**: The continuous-emission / marginal-reward shape for our
  `TimeAtTopStreaming` and continuous cadence — but bound to a *result* Scorer with a
  held-out gate, not an effort proxy.

### 6.3 EigenCloud / Eigen Arena / OpenRank

EigenCloud is the closest existing system and our primary comparison. It is roughly:

- **EigenAI** — verifiable inference (you can check a model produced a claimed
  output).
- **EigenCompute** — `Docker → TEE`, i.e. run a container inside a trusted execution
  environment and get attestation.
- **Eigen Arena** — a verifiable AI trading competition (with Recall, Dec 2025): a
  `Competitive / Continuous / Public / PrivateOracle` result-market.
- **OpenRank** — verifiable, challengeable, **forkable** leaderboards.

- **Proves**: That a verifiable, on-chain result-market with TEE-backed scoring is
  real and shippable today, and that *challengeable, forkable* leaderboards are a
  desirable primitive (a researcher can dispute a rank and fork the ranking
  function).
- **Lacks** (our differentiation):
  1. **No collaborative frontier-training mode** — Eigen is competitive-only; pooled
     improvement toward one shared artifact is absent.
  2. **No private/enterprise posture as the product** — the open arena is the
     marketing surface; we treat the *same* primitive with `Visibility = Private` as
     the business (open arena is *our* marketing too, but not the revenue).
  3. **No pluggable engines** — Eigen's competitions are bespoke; we make *every
     Tangle Blueprint* bounty-able by plugging it in as an Engine.
  4. **A single benchmark number, not a rigorous causal-lift Scorer** — we score
     *certified causal improvement* (CI lower bound over current best, cost ceiling,
     held-out gate), not one leaderboard scalar.
- **We borrow**: TEE-backed verifiable scoring (via the agent-sandbox substrate,
  §6.6), the challengeable-leaderboard / dispute primitive (our Validator m-of-n),
  and the overall "make it verifiable and on-chain" stance.

### 6.4 The Eigen quantum flagship + Google's withheld-circuit benchmark

The single most important external proof point for the *thesis* (not the
mechanics):

- **What happened**: Google held back a quantum circuit benchmark — one tied to
  breaking Bitcoin's ECC signatures — as a withheld, internal result. An open
  EigenCloud network of anonymous contributors, named researchers, and AI agents
  rebuilt the withheld benchmark *in the open* and pushed it from **37% → 39.9%**,
  past Google's withheld number, in roughly **72 hours**. Separately,
  **Trail of Bits** beat Google's ZK proof-of-quantum-cryptanalysis by improving the
  artifact.
- **Proves**: The headline claim of this entire repo — *"frontier science can move
  faster when problems are made verifiable, open, and agentic."* A closed lab's
  withheld result was matched and beaten by an open, type-agnostic crowd in days,
  precisely because the problem had a verifiable Scorer. This is the existence proof
  that the `PrivateOracle` competition over a frontier scientific surface works, and
  it is our must-pass **Scenario A**.
- **Lacks**: It was a one-off marketing event, not a reusable, programmable,
  privately-deployable primitive with on-chain settlement and a general Scorer
  interface.
- **We borrow**: The scenario itself (a private quantum oracle as Scorer), the
  "open + agentic + verifiable" framing, and the lesson that the Scorer's existence
  is what unlocks the crowd.

### 6.5 Decentralized / distributed training (DeMo)

- **Proves**: Frontier-scale model training can be *decentralized* — nodes over the
  open internet, communicating compressed gradients (DeMo: decoupled momentum, DCT,
  top-0.1% sparsification, libp2p gossip), can co-train one model. This is the live
  `Collaborative` engine.
- **Lacks**: Sound verification of contribution. As detailed in §3.3, contribution =
  GPU-minutes verified only statistically (TOPLOC-style state hashes + gradient-norm
  checks), which is **gameable by collusion**. Causal credit assignment among
  collaborators is unsolved.
- **We borrow**: The whole engine, as our `Collaborative` mode — while being honest
  (here and in [`./MECHANISM.md`](./MECHANISM.md)) that its verification is the
  weakest link in the design and an open research problem (§8).

### 6.6 Agent self-improvement & the Improvement-Plane

The substrate for *agent*-improvement Scorers is the Tangle Intelligence
**Improvement-Plane**:

- **Primitives**: `AgentProfile`; an agent-eval harness; a **replay engine** with
  **Tier A** (full re-execution), **Tier B** (tool-mocked replay), and **Tier C**
  (observational — *never promotes*, only flags); a **held-out gate**
  (`minLiftCiLower` default 0.02 = 2 percentage points, plus a `costPerTaskCeiling`);
  an **evidence ledger** of `{kind, delta, ci, n, confounded}`; and **R2
  calibration** — replay reproduces live lift *only* under model parity + a
  state-complete snapshot + `n ≥ 12`.
- **Proves**: That "this agent is genuinely better" can be turned into a *certified,
  held-out, confidence-bounded, cost-aware* claim rather than a vibe — exactly the
  rigorous causal-lift Scorer we contrast against Eigen's single number (§6.3).
- **Lacks**: It is a substrate, not a market — no proposer/reward/settlement layer,
  and its replay tiers inherit confounding risk (Tier C never promotes for good
  reason).
- **We borrow**: This *is* our `HeldOutEval` Scorer for the agent-improvement
  surface — the held-out gate, the evidence ledger, the replay tiers, and the R2
  calibration discipline carry over directly (see [`./ARCHITECTURE.md`](./ARCHITECTURE.md)).

### 6.7 MEV / decentralized compute markets

- **Proves**: Large, adversarial, crypto-economic markets for *computational
  outcomes* clear in production. MEV (maximal extractable value) auctions are
  result-markets: searchers compete to produce the most valuable block-ordering and
  are paid for the *outcome* (the bundle's value), not the effort of finding it.
  Decentralized compute markets (rented GPUs, etc.) prove the supply side scales.
- **Lacks**: MEV's "result" is trivially verifiable on-chain (the bundle either pays
  or it does not), so it dodges the held-out / privacy problems we must solve; raw
  compute markets sell *effort* (GPU-hours) and inherit the effort-verification
  swamp.
- **We borrow**: The auction-as-result-market mental model, the searcher archetype
  (independent, self-funded, diverse), and the crypto-economic settlement and
  dispute patterns — applied to *scored improvement* rather than block value.

---

## 7. Why now

Four enabling conditions converged, and none held five years ago:

1. **Compute abundance and rentability.** Search is parallel; the supply of rentable
   GPU/TPU and the existence of decentralized compute markets means the crowd can
   actually *bring* compute to a problem, not just opinions.
2. **Agentic researchers.** The "Researcher" role is now genuinely type-agnostic. An
   automated research loop or an LLM agent can autonomously propose, build, and
   submit candidate artifacts. The quantum flagship (§6.4) already had AI agents as
   first-class contributors. The supply of searchers is no longer bounded by the
   supply of humans.
3. **Verifiable-compute primitives.** Verifiable inference (EigenAI), state-hash
   commitments (TOPLOC), ZK proofs, and on-chain settlement (Tangle Blueprint, x402
   payments) make "run the Scorer and certify the result" enforceable rather than
   trust-me.
4. **TEE availability.** Trusted execution environments (Phala, AWS Nitro, GCP
   Confidential, Azure Confidential) are commodity, so the Scorer and the private
   asset can run isolated from both the researcher and the operator — the
   architectural basis for the `Private` Visibility knob and the `PrivilegedHardware`
   Scorer. **KNOWN GAP**: in the agent-sandbox substrate today, TEE attestation is
   *structural only* — we check the enclave shape but do **not** yet verify a
   hardware quote signature or pin measurements. Hardware-anchored attestation is on
   the roadmap, not shipped (see [`./PRIVACY.md`](./PRIVACY.md)).

The combination — abundant compute + agentic searchers + verifiable settlement +
commodity TEEs — is what makes a decentralized result-market buildable *now* rather
than aspirational.

---

## 8. Open research questions

These are genuinely open; we are not claiming solutions. They are ordered roughly by
how load-bearing they are for the thesis.

1. **Causal credit assignment in collaborative mode (Shapley-shaped).** When N nodes
   co-produce one improved artifact, who gets paid what? GPU-minutes is gameable
   (§3.3). A Shapley-value-style attribution (each contributor's marginal effect on
   the held-out score across coalitions) is the principled answer but is
   exponential to compute exactly and itself attackable by collusion. *Can we
   approximate held-out marginal contribution cheaply and collusion-resistantly?*

2. **Anti-overfit at scale.** A finite held-out set is a finite resource: enough
   submissions and the crowd overfits it. **Held-out rotation** (rotate which slice
   is hidden), **walk-forward** evaluation (always test on the future relative to the
   artifact), and submission budgets are the levers. *What rotation schedule and
   budget keep the certified lift honest as submission volume grows?*

3. **Covert-channel leakage bounds.** The score channel leaks (§4). *What is the
   provable information-leakage per scored query as a function of CI width,
   diagnostic richness, and rate limit, and what noise/budget makes it acceptable?*
   (Quantitative target in [`./PRIVACY.md`](./PRIVACY.md).)

4. **Collusion resistance.** Beyond collaborative credit: colluding researchers can
   coordinate submissions to probe a held-out set faster, split rewards to defeat
   rate limits, or (in effort mode) fabricate consistent updates. *What mechanisms —
   identity costs, stake, randomized held-out assignment — bound collusion's payoff?*

5. **Scorer gaming / Goodhart.** "When a measure becomes a target, it ceases to be a
   good measure." A Scorer is a proxy for true value; researchers optimize the proxy.
   *How do we design Scorers (and multi-Scorer ensembles, adversarial held-outs)
   whose optimum coincides with real-world value, and detect when the gap opens?*

6. **Cross-distribution generalization of certified artifacts.** An artifact
   certified to lift on held-out distribution D may not lift on the buyer's live
   distribution D′ (the R2-calibration problem from §6.6: replay reproduces live lift
   only under parity). *How do we certify, and price, generalization — not just
   in-distribution held-out lift?*

7. **Pricing certified lift in a marketplace.** Given a certified +X% improvement
   with confidence interval and cost, *what is it worth?* Pricing the marginal lift
   (for `Continuous` cadence), setting reward schedules that clear the market without
   overpaying, and avoiding both starvation and runaway emission are open mechanism-
   design questions (see [`./MECHANISM.md`](./MECHANISM.md)).

---

## 9. Threats to the thesis (honest)

The strongest counterarguments, stated as plainly as we can, with our current best
response — and where we have no good response, we say so.

### 9.1 "The Scorer is a single point of failure — and it is gameable."

If the Scorer is a bad proxy, the whole market optimizes the wrong thing (Goodhart,
§8.5), and if the Referee running it is compromised, certified results are fake. This
is the deepest threat: **the entire design inherits the quality and integrity of the
Scorer.** Our responses — m-of-n Validators for dispute, held-out rotation,
multi-Scorer ensembles, TEE isolation — *mitigate* but do not *eliminate* it. A
genuinely bad Surface/Scorer pairing produces certified garbage. We accept this and
push the burden onto Scorer design as a first-class, reviewable artifact.

### 9.2 "Held-out verification only works where a held-out Scorer exists."

Many valuable improvements have *no* cheap held-out measure — open-ended research,
taste, novel theorem-proving, anything where "better" is not a number you can compute
on hidden data. For those, we fall back to `HumanPanel` Scorers, which reintroduce
subjectivity, cost, and trust, partially surrendering the solve-hard/verify-easy
advantage. **The thesis is strongest exactly where a held-out Scorer is cheap and
honest, and weakest where it is not.** We do not claim to have decentralized all of
research — only the verifiable-result-shaped part of it. That part is large, but it
is not all of it.

### 9.3 "Collaborative mode's verification is gameable, and collaborative is half the pitch."

§3.3 and §6.5 are honest that DeMo-style contribution is verified only statistically
and is collusion-breakable. If collaborative frontier-training is a headline
differentiator (it is — §5), and its verification is the weakest part of the system,
then a core selling point rests on an unsolved problem. **Counter-counter**: the
*competitive, result-scored* core does not depend on this and is sound on its own; we
should not over-sell collaborative until the credit-assignment question (§8.1) has a
defensible answer. We mark it `(proposed)` accordingly.

### 9.4 "Privacy leaks compound; a determined adversary inverts the Scorer."

§4 reduces privacy to a leak-budget, but budgets are spent by patient adversaries:
membership inference, score-oracle inversion, and covert channels can, over many
queries, reconstruct the protected asset. If the residual leakage is larger than we
think, the `Private` enterprise pitch — the *business* — is undermined. Our response
is rate limits + DP noise + rotation + TEE isolation, with quantitative bounds owed
in [`./PRIVACY.md`](./PRIVACY.md). Until those bounds are *proven*, the enterprise
privacy claim is **a hypothesis, not a guarantee.**

### 9.5 "TEE attestation isn't real yet."

The `Private` and `PrivilegedHardware` knobs lean on TEE isolation, but our current
substrate does **structural-only** attestation — no hardware quote signature
verification, no measurement pinning (§7, KNOWN GAP). Until hardware-anchored
attestation ships, a malicious operator could in principle subvert the enclave. We
must not market enclave-grade confidentiality as delivered while it is structural. We
state this gap openly rather than paper over it.

### 9.6 "The market won't have two sides."

A marketplace needs both Proposers (paying for improvement) and Researchers
(supplying it). If either side is thin — no liquidity of bounties, or no crowd of
capable searchers — the flywheel never spins. Open arenas (marketing) exist to seed
the Researcher side; private bounties (revenue) seed the Proposer side. **The
existence of Kaggle, Bittensor, and the quantum flagship is evidence the Researcher
side shows up when problems are verifiable and rewarded** — but two-sided cold-start
remains a real go-to-market risk, not a solved one.

### 9.7 "EigenCloud is ahead and well-funded."

Our four differentiators (§6.3) — collaborative mode, private-as-the-business,
pluggable engines, causal-lift Scorer — are real, but Eigen could add any of them.
Our durable edge is *composition*: built on the agent-sandbox blueprint, mirroring
ai-trading-blueprint, composing training-blueprint, sitting in the Tangle Blueprint
ecosystem where **every blueprint becomes bounty-able**. That is a moat of breadth,
not of any single feature — and breadth moats are slower but harder to copy. Whether
it is *enough* is the open commercial bet.

---

## 10. Where this goes next

The three must-pass scenarios that exercise the thesis end to end:

- **Scenario A — Private Oracle (quantum):** a `PrivateOracle` Scorer over a frontier
  scientific surface, mirroring §6.4.
- **Scenario B — Public Continuous Arena (Eigen):** a `Competitive / Continuous /
  Public` arena, going head-to-head with Eigen Arena on its own ground.
- **Scenario C — Private Enterprise Bounty:** the same primitive with `Visibility =
  Private` — the monetization path.

Each is specified against the interfaces (`Surface`, `Scorer`, `Engine`,
`RewardSchedule`) in [`../SPEC.md`](../SPEC.md), shaped in
[`./ARCHITECTURE.md`](./ARCHITECTURE.md), incentivized in
[`./MECHANISM.md`](./MECHANISM.md), and bounded in [`./PRIVACY.md`](./PRIVACY.md).
