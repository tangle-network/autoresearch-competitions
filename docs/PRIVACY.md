# Autoresearch Competitions — Privacy & TEE Threat Model

> **Status:** design doc for an in-development Tangle Blueprint built **on** the
> agent-sandbox blueprint. Mechanisms marked **(proposed)** are not yet
> implemented. This document is written by a security engineer's hand: its
> credibility comes from **not overclaiming**. Where a guarantee is real today
> we say so; where it is structural-only or aspirational we say *that* instead.
> See [`SPEC.md`](../SPEC.md) for canon terminology,
> [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) for the sandbox/TEE substrate and the
> attestation gap (§7 there), and [`docs/MECHANISM.md`](MECHANISM.md) for the
> staking/slashing that backs every control here.

---

## 1. Framing — the default mode is privacy-easy

The single most important thing to understand about this system: **in the
default competitive mode there is almost nothing to protect.**

Recall the thesis (SPEC §1): the protocol prices the **outcome**, not the
**effort**. A Researcher produces a candidate artifact with *their own* compute
on *their own* machine, submits it under commit-reveal, and the Referee returns
a **score**. The Researcher never holds the Proposer's held-out data, private
oracle, or sealed eval. There is no plaintext on the Researcher's side to
exfiltrate, because the data never crosses to the Researcher's side at all.

> **Pay-for-outcome makes privacy mostly evaporate.** Researchers see *scores,
> not data*. The held-out set never leaves the Referee.

So this document is **not** a general "how to do confidential compute" manual.
It exists for three narrower reasons:

1. **White-box modes.** Some competitions get a far stronger research signal if
   the Researcher's code can *touch* the data (inspect failures, train on raw
   examples). The moment that happens, the easy story breaks and we need real
   controls. Most of this doc is about that case.
2. **The residual.** Even in the easy default, the *score itself* is a channel
   that leaks a little about the held-out set (§8). That leak is bounded, not
   zero. Honesty requires characterizing it.
3. **The honest gaps.** The TEE attestation we inherit from agent-sandbox is
   **structural-only** today (§12). We must be precise about what "attested"
   does and does not currently mean, so nobody builds a Private Enterprise
   Bounty on a guarantee that isn't there yet.

If you only read one section, read §3 (the key correction) and §4 (the hard
rule). Everything else is consequence.

---

## 2. Threat model — parties × what each can observe

A privacy claim is meaningless without naming **who** the adversary is and
**what** they can see. The five parties are the SPEC roles (§2 there). The
**real adversary for the Proposer's data is the Researcher** — the party whose
code may legitimately need to touch data — **not** the host. People reach for
TEEs to defend against the host operator; that is the wrong threat to optimize
for here (see §3).

### Trust boundaries

```
  ┌─ Proposer's data domain ─────────────────────────────────────┐
  │  held-out split · private oracle · sealed eval                │
  │                                                               │
  │   ════════ B1: Referee boundary (data never crosses out) ═══  │
  │                                                               │
  │   Referee TEE  ── runs Scorer on held-out ──▶ {value, ci}     │
  │        │                                                       │
  └────────┼───────────────────────────────────────────────────── ┘
           │  ════════ B2: score channel (leaks a little; §8) ════
           ▼
     Researcher  ── owns Engine + candidate artifact ──┐
        │                                               │
        │  ════ B3: in white-box, data crosses INTO ════│
        │        the Researcher's code (§3)             │
        ▼                                               ▼
   Node Operator (host) ── runs the sandbox plane, not the research
        │
        │  ════ B4: host boundary — TEE defends THIS one ════
```

- **B1 — Referee boundary.** In every mode, the held-out data is *meant* to stay
  inside the Referee. This is the boundary the whole design protects.
- **B2 — Score channel.** Whatever feedback reaches the Researcher (scores,
  diagnostics) crosses here. Non-zero by construction (§8).
- **B3 — Data-into-Researcher-code.** Only exists in white-box modes. This is
  the dangerous one and the reason §4 exists.
- **B4 — Host boundary.** The boundary a TEE actually defends. Defends Referee
  *and* Researcher enclave contents **from the Node Operator** — *not* the
  Proposer's data from the Researcher.

### Who can observe what (default Black-box / Redacted-feedback mode)

| Party | Held-out data | Researcher's method/artifact | Score + diagnostics | Other Researchers' artifacts |
| --- | --- | --- | --- | --- |
| **Node Operator** (host) | ❌ (inside Referee enclave) | ❌ (inside Researcher's own enclave / off-host) | ❌ (in transit it is sealed) | ❌ |
| **Researcher** | ❌ (never crosses B1) | ✅ (it's theirs) | ✅ own only, redacted | ❌ (commit-reveal; §10) |
| **Referee** | ✅ (runs the Scorer on it) | ✅ revealed artifact (to score it) | ✅ produces it | ✅ (scores all) |
| **Other Researchers** | ❌ | ❌ | ❌ (only public leaderboard if `Public`) | ❌ before reveal; ✅ after, if `Public` |
| **Proposer** | ✅ (owns it) | ✅ revealed winner under license | ✅ | ✅ |

### Who can observe what (White-box no-egress mode)

The one row that changes — and it changes everything:

| Party | Held-out / raw data |
| --- | --- |
| **Researcher (their code, running in the no-egress enclave)** | ✅ **plaintext, by design** — but cannot exfiltrate it because egress is dropped (§4, §5) |

The Researcher's *code* sees the data; the *Researcher the person* never
receives it, because the enclave that holds the plaintext has no network path
out and only an output-gated channel (§5). That distinction is the entire
white-box safety argument.

---

## 3. The key correction — a TEE defends against the HOST, not the RESEARCHER

This is the most common and most expensive misconception in the space, so we
state it bluntly.

> **A TEE (Trusted Execution Environment) protects the contents of an enclave
> from the operator of the machine it runs on. It does NOT, by itself, protect
> the Proposer's data from the Researcher.**

Why "run it all in a TEE" does **not** give the Proposer confidentiality against
the Researcher:

**The white-box example.** A Proposer wants a stronger research signal, so they
let the Researcher's own code run *inside* an enclave that also holds the raw
data — the standard "confidential compute" pitch. The TEE faithfully hides that
enclave's contents from the **Node Operator**. Good. But the **Researcher wrote
the code in the enclave**, and that code is holding the plaintext **by design**.
If that same code is also allowed to open a socket, it emails the dataset out.
The TEE did its job perfectly — it protected the enclave from the host — and the
Proposer's data still walked out the front door, because the threat was never
the host.

The host boundary (B4) and the data-into-code boundary (B3) are **different
boundaries**. A TEE is a tool for B4. It does nothing for B3. Confusing them is
how you ship a system that *attests beautifully* and *leaks completely.*

The corollary, which §4 makes precise: protecting the Proposer's data from the
Researcher in white-box mode is **not** an attestation problem. It is an
**egress and information-flow** problem.

---

## 4. The hard rule — pick at most two of three

Every privacy tier in this system is one application of a single rule.

> **A Researcher's code cannot simultaneously have all three of:**
> **{ arbitrary code, raw private-data access, free egress }.**
> **Any two are safe. All three is exfiltration-by-design.**

This is not a policy choice you can engineer around with a cleverer enclave; it
is arithmetic. Arbitrary code with raw data and an open socket *is* a data-export
program. The only question a competition designer answers is **which one to
drop** — and that choice *is* the privacy tier.

| If you drop… | …you keep | The resulting tier | What the Researcher gives up |
| --- | --- | --- | --- |
| **raw data access** | arbitrary code + free egress | **Black-box** (§5.1) | sees only scores; never touches data |
| **free egress** | arbitrary code + raw data access | **White-box no-egress** (§5.3) | runs anything on plaintext, but in a sealed no-network enclave; output is gated |
| **arbitrary code** | raw data access + (brokered) egress | **Attested-harness** (§5.4) | ships strategies/configs into a *measured* harness whose egress policy is attested, not arbitrary code |

**Redacted-feedback** (§5.2, the default when any feedback is needed) is a
refinement of Black-box: it keeps "no raw data access" but widens the feedback
from a bare score to PII-stripped aggregate diagnostics and synthetic/redacted
failure exemplars. It still drops raw data access; it just leaks a controlled
bit more through B2.

---

## 5. The privacy tiers

Each tier states: **what the Researcher gets**, **privacy strength**,
**research-power cost**, and **when to use it**.

### 5.1 Black-box (drop raw data access)

- **What the Researcher gets:** a **score** and nothing else — Kaggle-style.
  Submit artifact → receive `{value, ci}` on the held-out (or dev) split.
- **Privacy strength:** **maximum.** There is no plaintext on the Researcher's
  side, so there is nothing to exfiltrate. The only channel is the score (§8).
- **Research-power cost:** **highest.** A bare scalar is the weakest possible
  steering signal; the Researcher is optimizing nearly blind.
- **When to use:** any time the score alone is enough to drive an Engine —
  notably **Scenario A** (Private Oracle), where the Researcher *cannot* see the
  oracle anyway, so black-box is privacy-for-free.

### 5.2 Redacted-feedback — **the default when feedback is needed**

- **What the Researcher gets:** scores **plus** PII-stripped **aggregate
  diagnostics** (e.g. "23% of failures are on long inputs"; per-bucket error
  rates) and **synthetic or redacted failure exemplars** — examples shaped like
  the real failures but with the proprietary content removed or replaced.
- **Privacy strength:** **high, but no longer trivially zero.** Aggregate
  diagnostics and redacted exemplars are a deliberately widened B2 channel; the
  redaction pipeline is now part of the trusted base and must itself be reviewed
  (a leaky "synthetic" exemplar that echoes a real record is a real leak).
- **Research-power cost:** **moderate.** Far better steering than a bare score;
  the Researcher learns *where* they fail without learning *what the data is*.
- **When to use:** the **default** for **Scenario C** (Private Enterprise
  Bounty) and most `Private × HeldOutEval` competitions — whenever a bare score
  is too weak but raw access is unacceptable.

### 5.3 White-box no-egress (drop free egress)

- **What the Researcher gets:** their **arbitrary code runs on the raw data**,
  inside a **no-network enclave**. Output is **gated**: only the declared result
  (a score, a model artifact subject to the checks in §9) leaves; raw data and
  free-form output do not.
- **Privacy strength:** **strong but covert-channel-bounded.** Dropping egress
  removes the exfiltration path the §4 rule forbids. What remains are *covert*
  channels — the gated output and the score still carry some information, and
  timing/size side-channels exist. The guarantee is "leakage bounded to the
  gated output + covert channels," **not** "zero leakage."
- **Research-power cost:** **lowest** — this is the strongest research signal,
  because the Engine can do anything a local data-science loop could.
- **When to use:** when the research problem genuinely requires touching raw
  examples (deep failure analysis, training on the data) **and** the Proposer
  will accept a covert-channel bound rather than a black-box guarantee. Requires
  output-gating discipline and the attestation of §7.

### 5.4 Attested-harness (drop arbitrary code)

- **What the Researcher gets:** the data, plus egress — but they **cannot run
  arbitrary code**. They ship **strategies, configs, or weights** into a
  **measured, attested harness** the Proposer controls, whose **egress policy is
  itself part of the attested measurement.** The harness, not the Researcher,
  decides what leaves.
- **Privacy strength:** **strong, and the cleanest story for "data + feedback"**
  — because the thing touching the data is a known, measured program (you can
  pin its measurement; §7), not the Researcher's black box.
- **Research-power cost:** **moderate to high**, depending on how expressive the
  harness's strategy/config surface is. You trade Turing-completeness for
  inspectability.
- **When to use:** when the Researcher needs data-conditioned behavior **and**
  legitimate egress (e.g. an agent loop that calls an LLM, §6), and you can
  express the research as configuration of a fixed harness rather than as
  arbitrary code. The natural pairing with **brokered egress** (§6).

### Tier comparison

| Tier | Drops | Researcher gets | Privacy | Research power |
| --- | --- | --- | --- | --- |
| Black-box | raw data | score only | max | lowest |
| Redacted-feedback *(default w/ feedback)* | raw data | score + redacted diagnostics/exemplars | high | moderate |
| White-box no-egress | free egress | arbitrary code on plaintext, gated output | strong (covert-bounded) | highest |
| Attested-harness | arbitrary code | configs/strategies in a measured harness | strong | moderate–high |

---

## 6. Brokered egress — for agent loops that legitimately need tools

Some Engines are agent loops that *must* call an LLM or a tool mid-run. That is
a legitimate need for egress, which collides with white-box no-egress. The
resolution is **brokered egress**, not free egress.

> **Allow only a fixed allowlist of destinations, reached through a
> Referee-controlled proxy — never arbitrary sockets.**

Design:

- The enclave has **no direct network**. Its only outbound path is a
  **referee-controlled proxy** that enforces a **fixed allowlist** (e.g. "the
  competition's designated model endpoint, and nothing else").
- **(Proposed)** the proxy is the **Tangle router running in its own TEE**, so
  the broker itself is attested and the Researcher cannot tamper with the
  allowlist.
- Every allowed call is **logged, rate-limited, and content-monitored.**

What this buys: it shrinks the exfiltration surface from "any socket" down to
**covert channels through the model prompts themselves** — a Researcher could
try to encode stolen data into the text it sends to the allowlisted LLM. That
residual is real, but it is **monitored and rate-limited**: prompt content is
inspected, call volume is capped, and anomalies are flagged for slashing (§8).
You have converted an open door into a watched, narrow, throttled hallway.

Brokered egress is the mechanism that makes **Attested-harness** (§5.4) viable
for agent workloads, and that makes a *no-egress* enclave usable by agents that
would otherwise be unable to run at all.

---

## 7. Attest both sides — host hardware + harness image

A complete white-box / attested-harness story requires **two** attestations,
proving **two different things**. Neither alone is sufficient.

| Attestation | What it proves | Defends boundary |
| --- | --- | --- |
| **Host hardware attestation** (the TEE quote) | a genuine, unmodified enclave of the expected measurement is running on genuine TEE hardware | **B4** — protects enclave contents from the **Node Operator** |
| **Researcher harness-image measurement** | the code inside the enclave is the *agreed* harness that obeys the egress/output-gating policy — not a modified image that opens a socket | **B3** — proves the data-touching code behaves |

The first answers "is this a real enclave the host can't peek into?" The second
answers "is the program inside it the one that *won't* exfiltrate?" The §3
correction is exactly why you need both: the host attestation is silent about
whether the Researcher's code obeys the egress policy, and *that* — not the host
— is the Proposer's actual threat.

> **Attest both sides or you have attested nothing that matters for the
> Proposer's data.** Host-only attestation defends against the wrong adversary.

**Today, both are subject to the §12 gap:** the agent-sandbox attestation is
structural-only, so neither the host quote signature nor the harness-image
measurement is *cryptographically verified or pinned* yet. We describe the
intended design here and mark the implementation state honestly in §12.

---

## 8. The score is a channel — bounded, not zero

The uncomfortable truth, stated plainly:

> **You cannot reach zero leakage if the Researcher receives ANY feedback. The
> score itself leaks a little about the held-out set.**

Every time a Researcher submits and gets back a number, they learn *something*
about the secret split — that is what makes the number useful, and it is
inseparable from its usefulness. A patient adversary submitting many crafted
artifacts can, in principle, triangulate the held-out set from its scores alone.

So the goal is **not** zero leakage. The goal is leakage that is **bounded,
monitored, and economically irrational** — it costs more in stake and effort to
extract the data than the data is worth. The controls (each cross-referenced to
MECHANISM, which prices them):

- **Rate-limit submissions.** Cap scoring queries per Researcher per epoch. Each
  query leaks at most one CI's worth of signal; capping queries caps total
  leakage (MECHANISM §"Submission rate-limits"). Over-querying is **slashable.**
- **Return CI, not raw values.** Hand back `{value, ci}`, never per-example
  outputs. The Researcher learns *how good*, not *on which examples* — which
  collapses the most direct reconstruction channel (SPEC `Score`; MECHANISM).
- **Rotate held-out splits.** Periodically rotate the secret subset so a
  Researcher slowly fitting to it via score feedback loses that fit on rotation
  (MECHANISM §"Held-out rotation").
- **Run leakage tests.** Actively measure how much a repeated-scoring adversary
  can recover, and gate against a declared bound (SPEC AC §5; eval **E5**). For
  weight artifacts specifically, run **membership-inference** tests (§9).
- **Stake and slash on caught probing.** A `leakage_deposit` priced into stake
  makes a detected reconstruction campaign lose money (MECHANISM
  §"Stake sizing": `stake ≥ max(k · scoring_cost, leakage_deposit)`).

The honest framing for a Proposer: **a Private competition does not promise your
held-out set is unreconstructable in the information-theoretic sense.** It
promises the leak is rate-limited, CI-blurred, rotated out from under a slow
attacker, leakage-tested, and economically irrational to pursue. For most
enterprise tasks that is the right and sufficient bar; for a Proposer who needs
*provable* zero leakage, the answer is Black-box with a single terminal score,
not Continuous feedback.

---

## 9. Per-artifact-type risk — a prompt is not a model weight

The exfiltration risk of an artifact is not uniform. It scales with how much
**hidden state** the artifact can carry **back out** past the score channel —
and weights are in a different risk class entirely.

| Artifact type | Size | Inspectable? | Can it memorize/hide data? | Mitigation |
| --- | --- | --- | --- | --- |
| **Text prompt / skill** | tiny | ✅ fully | ✗ effectively no | direct review; nothing special needed |
| **Algorithm source / config** | small–medium | ✅ readable | ✗ (code, not a data store) | review; pin to attested-harness surface (§5.4) |
| **Model weights** | large | ✗ opaque | ✅ **yes — can memorize training data** | **DP training + membership-inference gating** (below) |

The **weight-memorization problem** is the sharp one. In any mode where a
Researcher trains a model **on** the Proposer's data and then *ships the weights
out* (e.g. White-box no-egress whose gated output is a model, or a Collaborative
training run), the weights can **memorize and smuggle** raw records — a large,
opaque artifact is a covert channel with gigabytes of capacity. The score
channel's rate-limits do nothing here, because the leak rides out *inside the
deliverable itself.*

Mitigations for weight artifacts:

- **Differential-privacy training (DP).** Require the training procedure to be
  differentially private so individual records provably cannot be reconstructed
  from the weights, at a quantified privacy budget. **(Proposed)** — this
  constrains the harness, so it pairs with Attested-harness (§5.4).
- **Membership-inference gating.** Before a weight artifact is released, run a
  **membership-inference attack** against it — test whether an adversary can
  tell which records were in the training set — and **reject** artifacts that
  fail the bound (SPEC AC §5; eval **E5**). Memorization that beats the gate is
  treated as a leakage attempt and is slashable.

The takeaway: **a competition whose deliverable is model weights cannot rely on
egress controls alone.** It needs DP + membership-inference, or it needs to not
ship the weights (score-only / Black-box).

---

## 10. Anti-plagiarism — commit-reveal (orthogonal to data privacy)

**Commit-reveal** (SPEC §2, jobs 2–3) protects a **different** thing from the
tiers above, and it is worth keeping the two cleanly separated:

- The privacy tiers protect the **Proposer's data** from the **Researcher**.
- Commit-reveal protects the **Researcher's submission** from **other
  Researchers** — anti-plagiarism.

A Researcher first commits a hash of their artifact, then reveals the artifact
itself. Because the hash is fixed first, a rival who sees the reveal cannot
re-submit a copy for credit (the eval **E3** path): reveals that don't match a
prior commit are rejected, and a copy committed *after* the original's reveal is
too late.

This is **orthogonal to data privacy.** It would be needed even if the held-out
set were fully public. We mention it here only to prevent the common conflation:
commit-reveal is not a data-confidentiality mechanism, and the privacy tiers are
not anti-copy mechanisms. A complete Private competition uses both.

---

## 11. Symmetric privacy — protecting the Researcher's method is the EASY direction

There are two directions of privacy in this market, and they are **not** equally
hard:

| Direction | Protect… from… | Difficulty |
| --- | --- | --- |
| **Hard** | the **Proposer's data** | the **Researcher** | requires the whole §4–§9 apparatus |
| **Easy** | the **Researcher's method** | the **Proposer and rivals** | fully achievable today |

The easy direction is **fully achievable** with mechanisms we already have:

- The Researcher's **Engine is never disclosed** — the protocol pays for
  artifacts, not methods, and explicitly never inspects an Engine (SPEC §11
  Non-goals).
- The submitted artifact is revealed only to the **Referee** (in a sealed
  scoring path), not to rivals; in `Private` mode it is access-controlled.
- **Commit-reveal** (§10) stops rivals from copying it.
- The winning artifact reaches the Proposer **under license**, on the Proposer's
  terms — disclosure is a commercial decision, not an automatic leak.

So when someone says "but won't the Researcher's secret sauce leak?" — that is
the **solved** direction. The genuinely hard, only-partially-solved direction is
the one this whole document is about: keeping the **Proposer's data** away from
the **Researcher**.

---

## 12. Honest limitations & roadmap — what we can and cannot claim today

This is the section the document exists to be honest about.

### The structural-only attestation gap (inherited from agent-sandbox)

> **Today, "attestation submitted" does NOT mean "attestation valid."**

The agent-sandbox blueprint we build on collects attestation reports and
validates their **shape** — it checks the structure of the attestation JSON
returned by the L1 runtime. It does **not** yet perform the cryptographic
verification that would make an attestation *trustworthy*. Specifically **not
implemented** (per ARCHITECTURE §7):

- **Hardware quote signature verification** — DCAP / KDS (Intel TDX), NSM (AWS
  Nitro), or the equivalent vendor root-of-trust check that proves the quote
  came from genuine TEE silicon.
- **Measurement pinning** — asserting the enclave measurement matches an
  expected, pinned value, so a *modified* image is rejected.

Consequently, **today the attestation hash proves that *an* enclave of the right
*shape* ran — not that it was genuine, unmodified hardware running the *expected*
code.** A malicious host who forged a structurally-correct attestation report
would currently pass. We do not pretend otherwise.

### What closing the gap requires

1. **Quote-signature verification** against each TEE vendor's root of trust:
   DCAP for Phala/TDX, NSM attestation for AWS Nitro, the GCP/Azure equivalents.
2. **On-chain measurement pinning** — pin expected enclave measurements per
   Scorer image and per Researcher harness image (§7), and reject mismatches.
3. **Nonce binding** — bind a fresh challenge nonce into each quote so a captured
   attestation cannot be replayed.
4. **Binding measurement → committed `attestHash`** so a dispute can prove the
   score came from the *expected* code on *genuine* hardware.
5. **Client-side verification** — let a Proposer (or a Validator on `CHALLENGE`)
   independently verify the quote rather than trusting the operator's say-so.

### What we CAN truthfully claim today

- The **default Black-box / Redacted-feedback** privacy story does **not depend
  on TEE attestation at all** — it holds because the data never crosses B1 to
  the Researcher (§1). This is the strongest and most honest claim we make, and
  it covers most real competitions.
- Commit-reveal anti-copy (§10) is real and does not depend on the gap.
- The score-channel controls (§8) — rate-limit, CI-only, rotation, leakage
  tests, slashing — are mechanism-level and do not depend on the gap.

### What we CANNOT truthfully claim today

- We **cannot** claim a white-box or attested-harness competition is
  cryptographically protected against a **malicious Node Operator**, because the
  host quote is not yet verified (B4 is structural-only).
- We **cannot** claim the harness running on the data is provably the agreed
  egress-respecting image, because harness-image measurement pinning is not yet
  implemented (B3 is structural-only).
- We **cannot** offer DP-trained / membership-inference-gated weight release
  (§9) as a shipped feature — it is **(proposed)**.

**Bottom line for a Proposer choosing today:** if your competition can run in
Black-box or Redacted-feedback mode, your data is protected by the *structure of
the market*, and the attestation gap does not touch you. If your competition
*requires* white-box access to raw data, you are currently relying on
structural-only attestation — treat the host as *not yet cryptographically
verified* and gate that risk operationally (trusted operator set, off-chain
agreement) until the §12 roadmap lands.

---

## 13. Which scenario uses which tier

Mapping the three SPEC reference scenarios (§8 there) to the tiers above:

| Scenario | Knobs | Tier | Why | Attestation reliance |
| --- | --- | --- | --- | --- |
| **A — Private Oracle** (quantum withheld circuit) | `Competitive × OneShot × Private × PrivateOracle` | **Black-box** (effectively free) | The Researcher *never sees the circuit/oracle* — only the score. Privacy is a non-problem; it falls out of the Scorer design (§5.1). | **none needed** for data privacy |
| **B — Public Continuous Arena** (Eigen-style) | `Competitive × Continuous × Public × HeldOutEval` | **(public — little to protect)** | Usually public data and a public leaderboard. The only residual is the score channel (§8); rotation + CI + rate-limit suffice. | only for verifiable-recompute integrity, not confidentiality |
| **C — Private Enterprise Bounty** | `Competitive × Continuous × Private × HeldOutEval` | **Redacted-feedback** (default), escalating to **White-box no-egress** or **Attested-harness** only if the signal demands raw access | This is the **case where the tiers actually matter.** Default to redacted feedback (§5.2); go white-box only with brokered egress (§6), both-sides attestation (§7), and the §12 caveats made explicit to the Proposer. | **material** — and currently structural-only (§12); gate accordingly |

The pattern across all three: **the easy scenarios (A, B) get privacy for free
or near-free; only the private enterprise case (C) pays the full cost of this
document.** That is the honest shape of the problem — and the reason §1 insists
the default mode is privacy-easy.
