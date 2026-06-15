# Mechanism & Incentive Design

> **Status:** design doc for an in-development Tangle Blueprint. Terminology is
> canon from [`SPEC.md`](../SPEC.md); this document is the **authority** on
> rewards, incentives, staking, slashing, and dispute math. Other docs
> ([`docs/ARCHITECTURE.md`](ARCHITECTURE.md),
> [`docs/PRIVACY.md`](PRIVACY.md)) reference these mechanisms but do not
> redefine them. Items marked **(proposed)** are not yet implemented; numbers in
> worked examples are illustrative unless tied to a grounded reference.

This blueprint sells one thing: **verifiable improvement**. A Proposer pays for
a certified score on a held-out measure, not for the effort that produced it.
Every mechanism below exists to make that trade honest — so that the cheapest
way for a Researcher to earn the bounty is to actually improve the artifact, and
the cheapest way for a Referee to earn fees is to certify truthfully.

The whole design leans on research's **solve-hard / verify-easy asymmetry**:
producing a better artifact is expensive and creative; confirming it scored
higher on a held-out test is one cheap, reproducible run. That asymmetry is what
lets us price the outcome and treat verification as a commodity. The mechanism's
job is to keep that asymmetry from being gamed — by overfitting, copying,
colluding, exfiltrating the test, or bribing the Referee.

---

## 1. Design goals & properties wanted

The mechanism is judged against eight properties. Each maps to concrete
mechanisms (sections in parentheses) and to a self-eval gate in
[`SPEC.md` §10](../SPEC.md).

| Property | What it means | Primary mechanism | Eval gate |
| --- | --- | --- | --- |
| **Truthful scoring** | The certified score equals the real held-out performance; no party profits by misreporting. | Referee TEE attestation + m-of-n re-score on dispute (§4, §7) | E7 |
| **Sybil-resistance** | Splitting into many identities yields no advantage over one. | Per-entrant stake; payout curves that don't reward identity count (§3, §5) | E3 |
| **Collusion-resistance** | Rings of Researchers / a Researcher+Referee pair cannot extract more than honest play. | Commit-reveal, held-out secrecy, m-of-n Referee, Shapley-shaped collaborative credit (§6, §8) | E3 |
| **Anti-overfit** | Tuning to visible feedback cannot raise the settlement score without genuine generalization. | Held-out split, walk-forward, rotation, submission rate-limits, `minLiftCiLower` gate (§4) | E2 |
| **Liveness** | Competitions make progress; the leaderboard keeps moving; payouts settle on time. | `TICK` keeper, marginal-lift reward, streaming emissions, rollover (§2, §5) | E1, E4 |
| **Fair payout** | Reward maps to contributed improvement, monotone in lift, no double-pay. | Marginal-over-best `RecordBounty`, top-k curves, contribution shares (§5, §6) | E4 |
| **"Keeps moving"** | Frontier-pushing stays incentivized after the first winner. | Marginal lift over current best with an ε threshold (§5) | E4 |
| **Budget-bounded** | The Proposer can never pay more than escrow; unspent budget is recoverable. | Escrow accounting, `Σ payouts ≤ escrow` invariant, rollover/refund (§2, §5) | E1, E4 |

Two cross-cutting constraints shape everything:

- **The score is a channel.** Every number returned to a Researcher leaks
  information about the held-out set. Privacy mechanisms (rate-limit, CI not raw
  value, rotation, leakage tests) are therefore *also* mechanism-design
  constraints, not just confidentiality features. Detail lives in
  [`docs/PRIVACY.md`](PRIVACY.md); the reward side is here.
- **The chain holds settlement state only.** Footprint is **O(competitions)**,
  not O(artifacts). N candidates produce N off-chain attested scores; only their
  hashes hit the chain. Mechanisms must be expressible as commitments + payouts,
  not as on-chain recomputation.

---

## 2. Bounty escrow & funding

A competition is funded at `CREATE_COMPETITION` (job 0). The Proposer escrows a
**reward pool** into the blueprint contract along with the sealed Scorer ref,
the four knobs, the `RewardSchedule`, and the deadline/policy. Escrow is the
budget ceiling: **`Σ all payouts + Σ all fees ≤ escrow`** is a hard invariant
the contract enforces (acceptance criterion 8 / eval E4).

### Pool shapes by cadence

| Cadence | Pool shape | Drain mechanic |
| --- | --- | --- |
| `OneShot` | Fixed prize pool. | Settled once at the deadline via `FINALIZE`; remainder (if the gate isn't cleared) rolls back to the Proposer. |
| `Continuous` | Streaming pool with a **burn rate cap**. | Drained per-epoch via `TICK`→`SETTLE`; runs until exhausted or the Proposer ends it. |

For `Continuous`, the Proposer commits a **per-epoch cap** so the pool cannot be
drained faster than intended. With `RecordBounty`, the natural cap is
`reward_per_unit_lift × max_expected_lift_per_epoch`; with
`TimeAtTopStreaming`, it is `rate_per_epoch`. The contract tracks
`remaining_escrow` and refuses any `SETTLE` that would overspend.

### Rollover / refund if no one clears the gate

The most important funding rule: **the Proposer pays only for certified
improvement.** If no candidate clears the gate (§4), no reward is paid.

- **OneShot.** If the top candidate's `ci.lower` does not clear `minLiftCiLower`
  over the baseline, the full pool (minus any Referee scoring fees actually
  incurred) refunds to the Proposer. A Proposer is never forced to pay for a
  field that failed to improve their metric.
- **Continuous.** Each epoch where the record does not move pays zero from the
  reward pool (Referee scoring fees may still accrue — see §9). Unspent reward
  rolls forward to the next epoch. When the Proposer ends the competition,
  `remaining_escrow` refunds.

**Worked example (OneShot refund).** Pool = 10,000 USDC. Baseline held-out
score = 0.700. `minLiftCiLower` = 0.02. Best candidate at deadline scores
`value = 0.715, ci = (0.705, 0.725)`. Lift on the CI lower bound is
`0.705 − 0.700 = 0.005 < 0.02`. **Gate not cleared.** Payout = 0; 10,000 USDC
(minus incurred scoring fees) refunds. The Proposer got a free, certified
"nobody beat your baseline by a statistically real margin" — which is itself a
valuable result.

This is the single most trust-building property for the demand side: **a bounty
that fails to attract a real improvement costs only scoring fees, not the
prize.**

---

## 3. Researcher staking

At `JOIN` (job 1) a Researcher posts **stake** before they may submit. Stake is
the load-bearing primitive for spam- and sybil-resistance and is the collateral
that slashing acts on.

### Purpose

1. **Sybil / spam cost.** Each identity costs `stake` to register. Flooding a
   competition with junk candidates or fake identities now has a per-identity
   capital cost, not a free-rider cost. Combined with payout curves that don't
   reward identity count (§5), splitting into N sybils is strictly worse than
   one honest entrant.
2. **Slashable collateral.** Stake is what a `CHALLENGE` slashes when a
   Researcher is caught: plagiarism, non-reproducible lift, exfiltration
   attempt, or collusion (§7). Without stake there is nothing to slash and
   cheating is costless.
3. **Skin in the game for feedback access.** In `Private` / `Redacted-feedback`
   competitions, the dev-split feedback a Researcher receives is itself a leaked
   channel. Stake makes over-querying-to-probe expensive: probe attempts are
   rate-limited (PRIVACY) and over-querying is slashable against stake.

### Sizing considerations

There is no single right stake. It is set per-competition by the Proposer (or a
network default) against these forces:

| Force | Pushes stake… | Why |
| --- | --- | --- |
| Reward pool size | up | A bigger prize attracts more spam and more incentive to cheat; stake should scale with what's at risk. |
| Scoring cost per candidate | up | Each submission consumes a Referee scoring run (tokens / GPU-min / QPU-sec). Stake should at least cover the marginal scoring cost a Researcher imposes, or honest Researchers subsidize spammers. |
| Desired entrant breadth | down | High stake excludes capital-poor but capable Researchers (a solo agent operator). Public arenas that want virality keep stake low. |
| Private-tier leakage risk | up | The more a single query leaks about a sealed held-out set, the more a probe attempt must cost to deter. |

**Heuristic (proposed):**
`stake ≥ max(k · scoring_cost_per_candidate, leakage_deposit)` where `k` covers
a few wasted scoring runs and `leakage_deposit` is the privacy-tier-specific
over-query bond. For a public arena where scoring is one cheap eval run, stake
can be small (cover ~3–5 scoring runs). For a private enterprise oracle where
each query leaks signal, `leakage_deposit` dominates.

### Reputation accrual

Stake is the entry cost; **reputation** is the durable asset. (proposed)

- A Researcher accrues reputation from **certified, undisputed, reproducible
  lifts** — weighted by lift magnitude and by surviving the challenge window
  un-slashed.
- Reputation is **non-transferable** (so it can't be bought, defeating the
  sybil-resistance it provides) and **slashable** alongside stake on a upheld
  challenge.
- Uses: (a) **reduced stake** for high-reputation Researchers (lowers their
  capital cost, rewarding a track record); (b) **priority / tie-breaking** in
  `SnapshotTopK` ties; (c) a public **leaderboard credential** that feeds the
  marketplace (§10) — a Researcher's history of certified lift is sellable
  signal. Reputation never substitutes for held-out verification: a reputable
  Researcher's score is still certified on held-out like everyone's.

---

## 4. The Scorer as referee — the gate and the anti-overfit toolkit

The `Scorer` (`score(artifact, split) -> {value, ci, cost, diagnostics, n}`) is
the thing being paid against. It runs inside the Referee's TEE on the **held-out
split** for settlement. This section is the authority on **the gate** — the
condition a candidate must clear to be paid.

### Held-out vs dev split

| Split | Who sees it | Purpose |
| --- | --- | --- |
| **Dev** | Researcher (scores + redacted diagnostics) | Feedback to steer the Engine. May be probed within rate limits. |
| **Held-out** | Referee only, never exposed | The secret measure settlement is computed against. Paying against held-out is what stops overfitting-to-the-test. |

A Researcher with unlimited dev-split access still cannot raise their *held-out*
settlement score without genuine generalization (acceptance criterion 2, eval
E2). This is the structural anti-overfit guarantee; the toolkit below hardens
it against statistical probing.

### The gate

A candidate is paid only if it clears **all three** conditions. These mirror the
Improvement-Plane held-out gate.

1. **`minLiftCiLower` — statistically real lift.** The improvement is measured
   on the **lower bound of the confidence interval**, not the point estimate:
   `ci.lower − baseline ≥ minLiftCiLower` (default `0.02` = 2 percentage points).
   Using the CI lower bound is the anti-overfit lever: a lucky high point
   estimate with a wide CI does **not** clear the gate. A real, reproducible
   lift has a tight CI whose lower bound clears the threshold.
2. **`costPerTaskCeiling` — no win by burning resources.** The candidate's
   `cost` (tokens / GPU-min / QPU-sec / panel-cost per task) must be
   `≤ costPerTaskCeiling`. This stops "improvements" that buy accuracy with
   10× spend, which the Proposer doesn't actually want.
3. **Guardrail / no-regression metrics.** Beyond the headline metric, named
   guardrails must not regress (mirrors the trading blueprint's
   "promote a variant only if metric +10% **and no regression**"). A latency
   win that tanks correctness, or an accuracy win that breaks a safety check,
   fails the gate. Guardrails are declared in the Scorer config; each has a
   `max_regression` tolerance.

Only candidates clearing all three enter ranking / marginal-lift accounting.
Everything else is recorded (for the leaderboard / marketplace) but pays zero.

### Anti-overfit toolkit

The held-out split is the defense; these keep it from being statistically
reverse-engineered through repeated scoring.

| Tool | What it does | Grounded in |
| --- | --- | --- |
| **Private held-out** | Settlement split never leaves the Referee; Researchers see only their own scores. | SPEC §2; Kaggle private test set. |
| **Walk-forward** | For time-series / sequential surfaces, evaluate on data strictly after the training window, advancing the window each epoch — so you can't tune to a fixed test. | trading blueprint walk-forward holdout. |
| **Held-out rotation** | Periodically rotate the held-out subset so a Researcher who slowly fits to it via score feedback loses that fit when it rotates. | PRIVACY rotation; Improvement-Plane. |
| **Submission rate-limits** | Cap scoring queries per Researcher per epoch. Each query leaks ≤ one CI's worth of signal; capping queries caps total leakage and probing. | Kaggle submission limits; PRIVACY. |
| **CI not raw value** | Return `(value, ci)`, not raw per-example outputs. The Researcher learns "how good," not "on which examples," collapsing the leakage channel. | SPEC `Score`; PRIVACY. |
| **`n ≥ 12` validity guard** | A score on fewer than 12 samples is not eligible to settle — too noisy to gate on. | Improvement-Plane. |

Together these turn "fit the test set" into a bounded, expensive, and
ultimately self-defeating strategy. Quantitative leakage bounds (how many
queries reconstruct how much of the set) are owned by
[`docs/PRIVACY.md`](PRIVACY.md); the rate-limit *as a reward-eligibility
constraint* is owned here.

---

## 5. RewardSchedule designs in depth

Four schedules convert certified scores into payouts. **RewardSchedule must
match Cadence** (SPEC §4 rule): `Continuous` → `RecordBounty` or
`TimeAtTopStreaming`; `OneShot` → `SnapshotTopK` or `TerminalPrize`. The
contract rejects incoherent pairs at `CREATE_COMPETITION` (acceptance criterion
10, eval E1).

### 5.1 `RecordBounty` — marginal lift over the current best

**Cadence:** `Continuous`. **The core "keeps moving" mechanism.**

Reward is paid for the **lift over the current record**, not absolute score:

```
payout(epoch) = reward_per_unit_lift × max(0, new_record − prior_record)
  paid only if  ci.lower − prior_record ≥ min_lift_ci_lower   (the ε threshold)
```

**Why marginal, not absolute.** This is the single most important insight in
continuous-mode design. If you paid absolute score, you pay **twice for the same
gain**: the first Researcher to reach 0.80 gets paid for 0.80, and so does the
next one who also reaches 0.80, even though they added nothing. Worse, after the
first winner hits a high score, there is no incentive to push further — matching
pays the same as the first winner did. Paying **marginal lift over best** means:

- You pay for each unit of improvement **exactly once** — to whoever first
  achieved it (budget-bounded; `Σ marginal payouts ≤ escrow`, eval E4).
- **Matching the record pays zero.** Beating it by a hair pays for the hair.
- The frontier-pushing incentive **never dies**: there is always unclaimed
  reward above the current record, so the leaderboard keeps moving.

**The ε threshold (`min_lift_ci_lower`).** Without a threshold you'd pay for
infinitesimal, noise-driven "improvements" and bleed the pool to statistical
flukes. Requiring `ci.lower − prior_record ≥ ε` (default 0.02) means a move only
pays if it is **statistically real** — the CI lower bound, not the point
estimate, must clear the prior record by ε. This also prevents a
"salami-slicing" attack where a Researcher submits a real improvement in many
tiny noise-sized steps to multiply threshold-clearing events: each step must
clear ε on the CI lower bound, so noise-sized steps don't pay.

**Worked example.** `reward_per_unit_lift` = 50,000 USDC per 1.00 of metric
(i.e. 500 USDC per 0.01). `min_lift_ci_lower` = 0.02. Baseline record = 0.700.

| Epoch | Top candidate `value` | `ci` | `ci.lower − prior_record` | Clears ε? | New record | Payout |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 0.735 | (0.724, 0.746) | 0.024 | ✅ | 0.735 | `50,000 × 0.035` = **1,750** |
| 2 | 0.740 | (0.731, 0.749) | −0.004 | ❌ | 0.735 | **0** (CI lower below record) |
| 3 | 0.770 | (0.762, 0.778) | 0.027 | ✅ | 0.770 | `50,000 × 0.035` = **1,750** |
| 4 | 0.772 | (0.760, 0.784) | −0.010 | ❌ | 0.770 | **0** (noisy; doesn't clear) |
| 5 | 0.800 | (0.792, 0.808) | 0.022 | ✅ | 0.800 | `50,000 × 0.030` = **1,500** |

Total paid for moving the record 0.700→0.800 (0.100 of lift) = **5,000 USDC** =
`50,000 × 0.100`. **The total paid equals reward-per-unit × total lift, no
matter how many Researchers or epochs it took** — each unit of frontier was
bought exactly once. Epochs 2 and 4 paid zero: a higher point estimate with a CI
lower bound below the record is not a certified improvement.

### 5.2 `TimeAtTopStreaming` — emissions per epoch held at #1

**Cadence:** `Continuous`. Rewards **holding** the frontier, not just reaching
it. Inspired by Bittensor-style continuous emissions by measured standing.

```
payout(researcher, epoch) = rate_per_epoch    if researcher is #1 at epoch close
                          = 0                  otherwise
```

**Dethrone mechanics.** Each epoch closes via `TICK`. The Referee's certified
scores rank the leaderboard. Whoever holds rank #1 at epoch close earns
`rate_per_epoch`. To **dethrone** the incumbent, a challenger must post a
certified score that clears the incumbent's by the same ε discipline
(`ci.lower > incumbent_value + ε_dethrone`) — otherwise a noisy tie doesn't flip
the crown, preventing crown-flapping between statistically-tied candidates.

**Worked example.** `rate_per_epoch` = 200 USDC. Epoch = 1 hour. Researcher A
takes #1 at epoch 3 and holds it through epoch 9 (7 epochs), then B dethrones
and holds epochs 10–12 (3 epochs).

| Researcher | Epochs at #1 | Payout |
| --- | --- | --- |
| A | 3–9 (7) | `200 × 7` = **1,400** |
| B | 10–12 (3) | `200 × 3` = **600** |

`RecordBounty` and `TimeAtTopStreaming` **compose**: a Proposer can pay a
one-time marginal-lift bounty for *moving* the record **and** a streaming rate
for *holding* it — rewarding both the breakthrough and its defense, while
keeping `Σ ≤ escrow`.

### 5.3 `SnapshotTopK` — ranked top-k payout at the deadline

**Cadence:** `OneShot`. At the deadline, rank all gate-clearing candidates and
pay the top `k` by a declared weight vector (`weights`, summing to the pool).

**Concrete top-5 curve.** A common shape is a decaying curve that rewards the
winner heavily but keeps places 2–5 meaningful (so strong-but-second efforts
still earn, sustaining a deep field). Pool = 10,000 USDC, `k = 5`:

| Rank | Weight (bps) | Payout (10,000 pool) |
| --- | --- | --- |
| 1 | 4,000 | 4,000 |
| 2 | 2,500 | 2,500 |
| 3 | 1,500 | 1,500 |
| 4 | 1,200 | 1,200 |
| 5 | 800 | 800 |
| **Σ** | **10,000** | **10,000** |

**Gate interaction.** Only candidates that clear the gate (§4) are rankable. If
fewer than `k` clear, the unfilled places' weight **rolls back to the Proposer**
(refund) rather than inflating lower ranks — the Proposer pays for `k` real
improvements or fewer, never for padding. **Tie-break:** earlier reveal time,
then higher reputation (§3), then lower cost.

**Worked example (short field).** With the curve above, suppose only 3
candidates clear the gate. Ranks 1–3 pay 4,000 / 2,500 / 1,500 = 8,000;
**2,000 (ranks 4–5 weight) refunds to the Proposer.**

### 5.4 `TerminalPrize` — single winner-take-all

**Cadence:** `OneShot`. One prize to the single top gate-clearing candidate.
Simplest schedule; right for withheld-oracle problems (Scenario A) where there
is one correct frontier and second place adds little. If no candidate clears the
gate, the full prize refunds.

**Worked example.** Prize = 25,000 USDC. Best revealed circuit beats the
withheld reference by a certified margin clearing the gate → winner takes 25,000;
all others (who staked and lost) earn 0 but keep their stake (losing honestly is
not slashable). If no circuit beat the reference, 25,000 refunds.

### 5.5 `{Structure × Cadence} → valid RewardSchedules`

| Structure × Cadence | `RecordBounty` | `TimeAtTopStreaming` | `SnapshotTopK` | `TerminalPrize` | `ContributionShare` |
| --- | :---: | :---: | :---: | :---: | :---: |
| Competitive × OneShot | ❌ | ❌ | ✅ | ✅ | — |
| Competitive × Continuous | ✅ | ✅ | ❌ | ❌ | — |
| Collaborative × Continuous | — | — | — | — | ✅ (per-epoch) |
| Collaborative × OneShot | — | — | — | — | ⚠️ (single terminal split; loses per-epoch accounting) |

❌ = nonsensical (SPEC §4 coherence matrix): marginal-over-best and time-at-top
need a clock that runs across epochs; OneShot has none. `Collaborative` uses
`ContributionShare` (§6), not the ranked schedules, because there is one shared
artifact, not rival ones to rank. `Collaborative × OneShot` is allowed but
⚠️: a single terminal split of a pooled artifact loses the natural per-epoch
contribution accounting that makes Collaborative work — `Continuous` is the
grain.

---

## 6. Collaborative-mode contribution attribution

In `Collaborative` mode, Researchers (typically GPU pools) **pool compute on one
shared artifact** — e.g. a model checkpoint trained by many contributors via the
training-blueprint DeMo engine. There are no rival artifacts to rank; payout is
split by **contribution share** (`ContributionShare`, `Σ share_bps = 10,000`).
The question is: **how do you measure contribution honestly?**

### 6.1 Baseline: GPU-minutes (and its honest weakness)

The training blueprint's baseline measures contribution as **GPU-minutes** and
pays proportionally. Verification is **statistical only** (TOPLOC
state-transition hash + gradient-norm outlier detection); there is **no
auto-slash, no data-hash / base-model enforcement**.

**This is gameable, and we say so plainly** (a known gap, SPEC §11):

- **Collusion / fake work.** A ring can report GPU-minutes for low-value or
  redundant work and split the pool. Statistical checks catch gross outliers,
  not a coordinated ring submitting plausible-looking-but-useless gradients.
- **GPU-minutes ≠ improvement.** Burning compute is rewarded even if it doesn't
  move the shared artifact's held-out score. This violates the project thesis
  — **pay for outcome, not effort** — at exactly the mode where effort is the
  unit. Collaborative mode must improve on this.

### 6.2 Improvements (proposed)

The fix is to drag Collaborative back toward **outcome-priced** payout — measure
contribution by *effect on the held-out score*, not raw compute. Four layers,
composable:

1. **Held-out-eval-gated payout (proposed).** Tie the *pool* to certified
   held-out improvement of the shared artifact (the same gate as §4: the
   checkpoint must clear `minLiftCiLower` to pay). GPU-minutes then split a pool
   that **only exists because the artifact actually improved** — effort is
   rewarded only when it produced outcome. This alone removes the "burn compute
   for pay even if the model didn't improve" failure.

2. **Shapley-shaped marginal-contribution credit (proposed).** Instead of raw
   GPU-minutes, weight each contributor by their **marginal effect** on the
   held-out score: approximate the Shapley value — the average improvement the
   shared artifact loses when that contributor's updates are ablated, over
   sampled coalitions / checkpoints. A contributor whose gradients don't move
   held-out gets near-zero credit even if they burned many GPU-minutes. This
   directly prices the *useful* part of the contribution. Exact Shapley is
   exponential; use a sampled / checkpoint-difference estimator (proposed) with
   a stated approximation error.

   **Worked example (proposed).** Three pools report GPU-minutes 100 / 100 / 100
   (naive split: 3,333 bps each). Ablation estimates of held-out lift
   contribution: A = +0.040, B = +0.038, C = +0.002 (C ran redundant / dead
   gradients). Shapley-shaped shares ≈ 0.040 / 0.080 : 0.038 / 0.080 :
   0.002 / 0.080 = **5,000 / 4,750 / 250 bps**. C burned equal compute but earns
   250 bps, not 3,333 — effort that didn't move the outcome isn't paid for it.

3. **Validator spot-checks (proposed).** Beyond statistical outlier detection,
   the m-of-n Validator committee **re-runs a sampled subset** of claimed
   updates (recompute a contributor's reported gradient from the committed data
   + base-model hash) and checks it matches. This is the missing **data-hash /
   base-model enforcement**: contributors commit to `(data_hash, base_model_hash)`
   and a spot-check that fails is slashable. Spot-checking a sample makes
   fabricating a ring's worth of fake work expected-costly.

4. **Stake-weighted SLA (proposed).** GPU pools post stake and a delivery SLA
   (committed minutes / availability). Falling short of the SLA, or failing a
   spot-check, slashes stake. This deters the "report minutes, deliver junk"
   attack the baseline can't auto-punish.

**Honest status:** layers 1 is straightforward; 2–4 are (proposed) and the
Collaborative cryptographic-contribution problem is **not fully solved** (SPEC
§11). The improvement over the baseline is moving from "pay for reported
GPU-minutes, verify statistically, never slash" to "pay for held-out-gated,
Shapley-shaped, spot-checked, slashable contribution." See §11.

---

## 7. Dispute & slashing

Disputes are the backstop that makes certified scores trustworthy without
re-scoring everything on-chain. We **attest once and re-score only on dispute**
(SPEC §11 non-goal: re-scoring everything on-chain). A `CHALLENGE` (job 6) is the
only path that activates Validators.

### 7.1 CHALLENGE economics

Anyone may challenge a certified score by posting **challenger stake**. The
challenge triggers an m-of-n Validator **re-score** (§7.2). The economics must
make honest challenging profitable and frivolous challenging costly:

```
if re-score upholds the challenge (certified score was wrong):
    faulty party (Referee or Researcher) is slashed
    challenger receives:  challenger_stake refunded  +  reward (a share of the slashed stake)
if re-score rejects the challenge (certified score stands):
    challenger_stake is slashed (split to Validators + the wrongly-accused party)
```

**Sizing the challenger stake.** It must be (a) high enough that spamming
challenges to grief the system is unprofitable, (b) low enough that a Researcher
who spots a real miscertification will actually challenge. Set
`challenger_stake ≈ Validator re-score cost × safety_factor`, so the challenger
at least covers the work they force the committee to do if they're wrong. The
**reward if upheld** should exceed `challenger_stake` (so challenging a real
fault is +EV), funded from the slashed party's stake.

**Worked example.** Referee certifies a candidate at 0.78; the true held-out
score is 0.70 (miscertification). A Researcher challenges with 500 USDC stake.
m-of-n re-score returns 0.70 → challenge **upheld**. Referee's stake (say 5,000)
is slashed: challenger gets 500 (refund) + 1,500 (reward) = 2,000; Validators
split the remaining 3,500. The Referee loses 5,000 for one bad certification —
making honest scoring the dominant strategy. A frivolous challenger who was
wrong would instead forfeit their 500.

### 7.2 m-of-n re-score

Re-scoring uses the same Validator backstop as the trading blueprint: a default
**2-of-3** committee, **EIP-712**-signed outcomes, aggregate score threshold
**≥ 50** (mirroring SPEC §2 / the trading blueprint). The committee
independently re-runs the Scorer on the held-out split inside their own TEEs and
signs the result. A score that disagrees with the certified value **beyond a
declared tolerance** flips the dispute. Tolerance accounts for legitimate CI
noise: disagreement must exceed the CI width, not just differ in the last
decimal (eval E7 honors tolerance).

### 7.3 Slash conditions catalog

| Condition | Who is slashed | How detected | Notes |
| --- | --- | --- | --- |
| **Fabricated / incorrect score** | Referee | m-of-n re-score disagrees beyond tolerance | The core Referee-honesty slash. |
| **Exfiltration attempt** | Researcher | Over-query past rate-limit; leakage test trips (PRIVACY) | Treats probing the held-out set as an attack on the channel. |
| **Plagiarism / copy** | Researcher | Reveal matches a prior commit by another Researcher (commit-reveal) | An artifact revealed by A can't be committed by B (acceptance criterion 3). |
| **Non-reproducible lift** | Researcher | Re-score can't reproduce the certified lift (state-incomplete replay) | Lift must replay; a one-off lucky run that doesn't reproduce is slashed. |
| **Collusion** | Researcher(s) and/or Referee | Spot-check / cross-check reveals coordinated fake work or a Researcher-Referee ring | Hardest to detect; (proposed) spot-checks + m-of-n independence. |
| **SLA / spot-check failure (collaborative)** | GPU-pool Researcher | Validator spot-check of claimed updates fails (§6.2.3) | Enforces data-hash / base-model commitment. |

**Honest losing is never slashable.** A Researcher who stakes, submits a real
attempt, and loses keeps their stake. Slashing is for *cheating*, not for
*losing* — otherwise the field collapses and the arena dies.

---

## 8. Anti-cheat catalog

Every attack we can name, with the mechanism that stops it. This is the
adversarial spine of the design.

| Attack | What the attacker does | Mechanism that stops it | Eval |
| --- | --- | --- | --- |
| **Held-out leakage** | Probe the sealed test via repeated scoring to reconstruct it, then fit. | Held-out never exposed; submission rate-limits; CI not raw; rotation; leakage test → slash (§4, PRIVACY). | E5 |
| **Overfitting (dev split)** | Tune to visible feedback so the point estimate looks great. | Settlement on held-out only; gate uses **CI lower bound** so wide-CI flukes don't pay; walk-forward; `n ≥ 12` (§4). | E2 |
| **Plagiarism / copy** | Copy a rival's revealed artifact and resubmit as your own. | **Commit-reveal**: B can't commit a hash for an artifact A already committed; reveal-mismatch reverts; copy earns zero + slash (§7). | E3 |
| **Sybil** | Split into many identities to multiply payout or probe more. | Per-identity **stake**; payout curves indifferent to identity count; per-identity rate-limits (§3, §5). | E3 |
| **Collusion (collaborative)** | A ring reports plausible fake GPU-work and splits the pool. | Held-out-gated pool + Shapley-shaped credit (dead gradients earn ~0) + Validator spot-checks + stake-weighted SLA (§6). | E3 |
| **Scorer gaming / Goodhart** | Optimize the measured metric while degrading the real goal. | **Multi-metric guardrails** with no-regression tolerances + **hidden held-out** + **human spot-check** on `HumanPanel` surfaces (§4). | E2 |
| **Reward hacking** | Win by burning resources / salami-slicing noise into many ε-clearing steps. | `costPerTaskCeiling` (no win by spend); ε on **CI lower bound** so noise-sized steps don't clear (§4, §5.1). | E4 |
| **Referee bribery / miscertification** | Bribe or run a dishonest Referee to certify false scores. | **TEE** attestation (enclave-isolated scoring) + **m-of-n re-score** on dispute (independent committees) + Referee stake slashed on upheld challenge (§7). | E7 |
| **Crown-flapping** | Flip #1 back and forth between statistically-tied candidates to farm streaming reward. | Dethrone requires clearing the incumbent by ε on the CI lower bound; ties don't flip the crown (§5.2). | E4 |

**Goodhart deserves a note.** No single metric is safe to optimize against
without proxy-gaming. The defense is layered: (a) **guardrails** turn "the
metric" into "the metric *and* these don't regress"; (b) the **hidden held-out**
means the Researcher can't even see the exact thing they're being graded on; (c)
for subjective surfaces, a **`HumanPanel`** spot-check catches metric-gaming the
automated Scorer misses. We never settle on a single number a clever optimizer
can saturate.

---

## 9. Fee model

Payments run over **x402** (HTTP-native stablecoin payments) integrated through
the agent-sandbox substrate this blueprint builds on (see
[`docs/ARCHITECTURE.md`](ARCHITECTURE.md)). Three revenue events:

1. **Proposer escrow** (`CREATE_COMPETITION`): funds the reward pool. Not a fee
   — it's the prize, refundable per §2.
2. **Scoring fee** (per `REPORT_SCORE`): the Referee charges for each certified
   score (covers the held-out scoring run's real cost: tokens / GPU-min /
   QPU-sec / panel-cost). Charged via x402 at submission. This is what a
   Researcher's stake must cover a few of (§3).
3. **Protocol service revenue** (per competition / per epoch): the network's cut
   for running the blueprint service plane, split across the parties below.

### Revenue split

The trading blueprint's `FeeDistributor` splits **70% operator / 30%
validators**. This blueprint **extends** it to add the Referee — the scarce
trusted resource that runs the held-out scoring — as a first-class earner.

| Party | Share (proposed) | Rationale |
| --- | --- | --- |
| **Node Operator** | 55% | Runs the blueprint service plane: blueprint binary + sandboxes hosting Engines and Referee scoring. The infrastructure cost center. |
| **Referee** | 30% | Runs the held-out Scorer in TEE and certifies — the scarce trusted resource; the verify side of solve-hard/verify-easy. Earns per certified score; **slashed on upheld miscertification** (§7). |
| **Validator** | 15% | m-of-n dispute backstop. Earns the base share for standing ready + the bulk of slashed stake when activated on `CHALLENGE`. |

The 55/30/15 split (proposed) generalizes trading's 70/30: it carves the
Referee's 30% out of what would otherwise be operator+validator revenue,
reflecting that **certification is the value-add unique to this blueprint**.
Where Referee = Proposer or Referee = a committee (SPEC §2), that 30% accrues to
whoever plays the role. Disputes redistribute: an upheld challenge moves slashed
stake to challenger + Validators, independent of this base split.

---

## 10. Marketplace flywheel

Competitions don't just settle a prize — they **manufacture certified-artifact
inventory**. Every competition, across its life, produces scored artifacts:
winners, near-misses, and losers, each with a Referee-attested score on some
distribution. That inventory seeds a **certified-artifact marketplace**.

### How competitions seed the marketplace

- A `Competitive` competition produces a ranked field of artifacts, each with a
  certified `{value, ci, cost}` and an attestation hash. **Winning *and* losing
  artifacts can be listed and sold** — a candidate that placed 4th on
  Proposer X's metric may be exactly what Buyer Y needs on *their*
  distribution.
- A `Continuous` arena produces a *stream* of record-holders over time — a
  history of certified frontiers, each sellable.
- The leaderboard + reputation (§3) is the **provenance layer**: a buyer sees
  the artifact's certified track record, not a vendor's claim.

### Pricing certified lift on the buyer's distribution

The unit of value is **certified lift**, but lift is **distribution-specific**.
An artifact certified at +0.10 on Proposer X's held-out set is not guaranteed to
lift Buyer Y's distribution by +0.10. So the marketplace prices lift **on the
buyer's distribution**: the buyer supplies (or the seller is re-scored against) a
held-out sample of the buyer's data, the same Referee machinery produces a
certified lift on *that*, and price is a function of *that* certified number —
not the original competition's. This reuses the entire §4 gate + §7 dispute
machinery: a marketplace sale is just a competition-of-one against the buyer's
held-out set.

### Licensing / consent / provenance (required)

A sale is not valid without:

- **Consent / licensing.** The Researcher who produced the artifact must have
  listed it for sale under a stated license; the Proposer whose competition
  produced it must have permitted resale (a competition declares at creation
  whether artifacts may be marketplace-listed and on what license terms).
- **Provenance.** The artifact carries its certified history: which
  competition(s), what certified scores, the attestation hashes, the producing
  Researcher's reputation. Buyers price against verifiable provenance, not
  claims.
- **No leakage of the original held-out set.** A sale re-scores on the *buyer's*
  data; it never reveals the original Proposer's sealed held-out set (PRIVACY).

The flywheel: competitions attract Researchers → Researchers produce certified
artifacts → artifacts become sellable inventory → marketplace revenue attracts
more Researchers and Proposers → more competitions. The mechanism that makes a
score trustworthy in a competition (held-out + attestation + dispute) is the
*same* mechanism that makes a score trustworthy in a sale — so the marketplace
inherits the trust model for free.

---

## 11. Open mechanism-design problems

Stated honestly. These are unsolved or only partially addressed; do not market
them as done (SPEC §11 non-goals).

1. **Collaborative contribution verification is statistical, not
   cryptographic.** GPU-minutes verification today is TOPLOC hash + gradient
   outlier detection — no auto-slash, no data-hash / base-model enforcement. The
   §6.2 layers (held-out-gating, Shapley-shaped credit, spot-checks, SLA) are
   (proposed) and the Shapley estimator's approximation error is unquantified. A
   coordinated ring submitting plausible-but-useless gradients is not provably
   caught.

2. **Attestation is structural-only.** The TEE attestation hash proves enclave
   *shape*, not a full remote-attestation chain (inherited gap from
   agent-sandbox). A compromised-but-correctly-shaped enclave could in principle
   certify falsely; m-of-n re-score is the backstop, but it is reactive
   (dispute-triggered), not preventive.

3. **Leakage bound under adaptive querying is hard to make tight.** Rate-limits
   + CI + rotation bound leakage, but a sophisticated adaptive querier
   coordinating across sybils (each within its own rate-limit) may extract more
   than the per-identity bound suggests. The composition of per-identity bounds
   into a global bound is not fully worked out (owned by PRIVACY; flagged here
   as a *reward-eligibility* risk).

4. **Shapley approximation vs. gameability.** Sampled / checkpoint-difference
   Shapley estimators are themselves gameable: a contributor could time
   contributions to maximize *measured* marginal credit (e.g. submit right
   before a checkpoint) without maximizing true contribution. Order-/timing-
   robust estimators are an open problem.

5. **Referee centralization in private competitions.** When Referee = Proposer
   (an enterprise scoring its own private eval), the dispute backstop is weaker
   — challengers can't independently re-score a truly private held-out set
   without the Proposer exposing it. The trust model degrades to "trust the
   Proposer's TEE attestation," which circles back to problem 2.

6. **Optimal stake / `min_lift_ci_lower` / `rate_per_epoch` are
   hand-tuned.** We give heuristics (§3, §5), not a mechanism that *learns* the
   spam/leakage/liveness-optimal parameters per competition. Auto-calibrating
   these from observed entrant behavior is future work.

7. **Cross-competition collusion and reputation gaming.** A ring could farm
   reputation in low-stakes competitions to earn reduced stake / tie-break
   priority, then exploit it. Reputation is non-transferable and slashable, but
   the cross-competition equilibrium isn't analyzed.

---

*Cross-references: [`SPEC.md`](../SPEC.md) (canon terminology, jobs, lifecycle,
acceptance criteria, self-eval gates), [`docs/ARCHITECTURE.md`](ARCHITECTURE.md)
(TEE/sandbox substrate, x402 wiring, job→contract→runtime), and
[`docs/PRIVACY.md`](PRIVACY.md) (visibility tiers, formal leakage bounds, the
score-as-channel analysis).*
