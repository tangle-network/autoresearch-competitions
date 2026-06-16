//! Agent self-improvement vertical: the autoresearch market drives the
//! **Improvement-Plane** competition — the flagship recursive-self-improvement (RSI)
//! loop shape of `@tangle-network/agent-eval`.
//!
//! Researchers submit *agent profiles* — the knobs that actually decide how well an
//! agent clears a task suite: how much of the right **skill** coverage it has, the
//! **prompt** quality, the **tool-selection** accuracy, and how well it uses
//! **memory/retrieval**. A real agent-runtime backend would run that profile over a
//! benchmark of tasks and report a pass-rate; the market's Referee re-scores the
//! produced profile on a **held-out** task split, gates it, ranks, and pays.
//! **Delegating the eval never delegates the trust** — a researcher's own dev-suite
//! number is ignored; only the held-out re-score decides payment.
//!
//! # The metric: a modeled task-suite pass-rate in `[0, 1]`
//!
//! [`ImprovementPlaneScorer`] decodes a profile's four knobs into a pass-rate that
//! rises as the profile approaches a good agent, then evaluates it over a modeled
//! number of held-out tasks and reports the empirical pass-rate with a **Wilson**
//! score interval (the correct CI for a binomial proportion — far better than a
//! normal interval near `0`/`1`). `value = +pass_rate`, so HIGHER is better and the
//! orchestrator computes a positive lift for a genuine pass-rate gain.
//!
//! # The generalization gap the held-out gate exploits
//!
//! A profile *over-tuned to the dev task split* — encoded by the `overfit` knob,
//! which a researcher who over-searches the dev signal drives up — passes MORE dev
//! tasks than it deserves and FEWER held-out tasks (classic overfit: it has memorized
//! dev-suite idiosyncrasies rather than learned a generalizing skill). On
//! [`Split::Dev`] the overfit bonus is added; on [`Split::HeldOut`] an overfit
//! PENALTY is applied instead. So a profile that wins the dev signal can still fail
//! the Referee's held-out gate, while a well-generalizing profile clears it.
//!
//! # Honest seam — NOT a real agent eval
//!
//! [`ImprovementPlaneScorer`] is a *deterministic stand-in* for the real
//! agent-runtime RSI loop scored by `@tangle-network/agent-eval`. It does not run an
//! agent, call a model, or execute a task — it is a closed-form model of pass-rate
//! dynamics (with the *shape* of skill coverage, prompt quality, tool accuracy, memory
//! use, and dev-suite overfit), with no `rand`, no clock, and no I/O, so the e2e proof
//! is byte-reproducible in CI. The value it proves is the **market mechanism around a
//! delegated agent eval**: held-out re-scoring of a submitted profile, a Wilson-CI
//! gate refusing plausible-but-overfit profiles, ranking, and conserved payouts. The
//! live backend (Node + `agent-runtime` + `agent-eval` + model credentials) drops in
//! behind the same `Engine`/`Scorer` seams unchanged — the universal
//! [`SupervisorEngine`](autoresearch_supervisor::SupervisorEngine) searches the same
//! `params` encoding this scorer decodes.

use std::future::Future;

use autoresearch_runtime::traits::{Scorer, ScorerError};
use autoresearch_runtime::types::{Measurement, Split};
use autoresearch_supervisor::{ArtifactKind, GenericArtifact};

// --- Profile encoding (the searchable params vector) ------------------------
//
// The `GenericArtifact::params` the universal engine searches map 1:1 onto the four
// agent-profile knobs plus a dev-suite overfit knob. Each is a real number the engine
// perturbs; the scorer squashes it into its natural range. Index order is the public
// contract between the engine's search and this scorer's decode.

/// `params[0]` — skill coverage: how much of the suite's required skills the agent has.
pub const IDX_SKILL: usize = 0;
/// `params[1]` — prompt quality: how well the system/task prompts are written.
pub const IDX_PROMPT: usize = 1;
/// `params[2]` — tool-selection accuracy: picking the right tool for each step.
pub const IDX_TOOL: usize = 2;
/// `params[3]` — memory/retrieval quality: reusing prior context effectively.
pub const IDX_MEMORY: usize = 3;
/// `params[4]` — dev-suite overfit: tuning to dev-split idiosyncrasies. HELPS dev,
/// HURTS held-out. A researcher who over-searches the dev signal drives this up.
pub const IDX_OVERFIT: usize = 4;

/// Dimension of a well-formed agent profile param vector.
pub const PROFILE_DIM: usize = 5;

// --- Pass-rate dynamics constants -------------------------------------------
//
// A closed-form model of how a profile's knobs convert into a task-suite pass-rate.
// The point is not behavioral fidelity; it is to give the market a real multi-knob
// optimization surface with the *shape* of agent-eval dynamics (each capability lifts
// pass-rate with diminishing returns; overfit helps dev and hurts held-out), so a
// well-generalizing profile wins on held-out and an overfit one is gated.

/// Pass-rate floor of a zero-knob baseline agent (it still guesses some easy tasks).
const BASE_PASS: f64 = 0.20;
/// Headroom each capability knob can contribute toward a perfect agent.
const HEADROOM: f64 = 0.80;

/// Weight of skill coverage in the pass-rate (the dominant capability).
const W_SKILL: f64 = 0.40;
/// Weight of prompt quality.
const W_PROMPT: f64 = 0.25;
/// Weight of tool-selection accuracy.
const W_TOOL: f64 = 0.20;
/// Weight of memory/retrieval quality.
const W_MEMORY: f64 = 0.15;

/// How much dev-suite overfit inflates the DEV pass-rate (memorized idiosyncrasies).
const OVERFIT_DEV_BONUS: f64 = 0.18;
/// How much dev-suite overfit DEPRESSES the HELD-OUT pass-rate (it didn't generalize).
const OVERFIT_HELDOUT_PEN: f64 = 0.30;
/// Baseline generalization gap: held-out tasks are intrinsically a touch harder even
/// for a non-overfit profile (the suite was tuned on dev).
const BASE_GEN_GAP: f64 = 0.02;

/// z for the two-sided 95% Wilson score interval (matches the rest of the repo).
const Z_95: f64 = 1.96;

// --- Deterministic noise ----------------------------------------------------

/// A splitmix64 finalizer mapped to a uniform `f64` in `[0, 1)`. Deterministic from
/// its input mix word — no `rand`, no clock — so each modeled task's pass/fail draw is
/// byte-reproducible, which is what lets the e2e assert concrete lift. Same generator
/// family as `distributed_training::jitter`, returning a `[0,1)` unit instead of
/// `[-1,1)` because here it draws Bernoulli task outcomes.
fn unit01(mix: u64) -> f64 {
    let mut z = mix.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let bits = z >> 11; // top 53 bits
    (bits as f64) / ((1u64 << 53) as f64)
}

/// Logistic squash into `(0, 1)`. The raw search params are unbounded reals; this maps
/// each knob to a capability fraction with smooth diminishing returns, so the engine
/// always has gradient signal and the pass-rate stays a valid probability.
fn squash(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

// --- The agent profile (decoded view of the params) -------------------------

/// A decoded, human-readable agent profile: each capability as a fraction in `[0, 1]`
/// plus the dev-suite overfit fraction. This is the structured form of the four
/// Improvement-Plane knobs (skills / prompt / tools / memory) the live `agent-eval`
/// backend would carry; here it is decoded from [`GenericArtifact::params`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AgentProfile {
    /// Skill-coverage fraction in `[0, 1]`.
    pub skill: f64,
    /// Prompt-quality fraction in `[0, 1]`.
    pub prompt: f64,
    /// Tool-selection-accuracy fraction in `[0, 1]`.
    pub tool: f64,
    /// Memory/retrieval-quality fraction in `[0, 1]`.
    pub memory: f64,
    /// Dev-suite overfit fraction in `[0, 1]` (helps dev, hurts held-out).
    pub overfit: f64,
}

impl AgentProfile {
    /// Decode a profile from a param vector. Missing trailing params decode as `0`
    /// (the baseline value), so a shorter-but-valid vector is still well-defined; the
    /// surface (`GenericSurface`) is what enforces non-empty / all-finite.
    #[must_use]
    pub fn from_params(params: &[f64]) -> Self {
        let raw = |i: usize| params.get(i).copied().unwrap_or(0.0);
        Self {
            skill: squash(raw(IDX_SKILL)),
            prompt: squash(raw(IDX_PROMPT)),
            tool: squash(raw(IDX_TOOL)),
            memory: squash(raw(IDX_MEMORY)),
            overfit: squash(raw(IDX_OVERFIT)),
        }
    }

    /// The true (noise-free, non-overfit) capability pass-rate this profile earns: the
    /// weighted sum of capability fractions scaled into `[BASE_PASS, BASE_PASS+HEADROOM]`.
    fn capability_pass_rate(&self) -> f64 {
        let cap = W_SKILL * self.skill
            + W_PROMPT * self.prompt
            + W_TOOL * self.tool
            + W_MEMORY * self.memory;
        BASE_PASS + HEADROOM * cap
    }

    /// The expected pass-rate on a split, with the dev/held-out overfit asymmetry
    /// applied, clamped to a valid probability `[0, 1]`. This is the per-task success
    /// probability the Wilson evaluation draws against.
    ///
    /// - [`Split::Dev`]: overfit ADDS to the pass-rate (memorized dev idiosyncrasies).
    /// - [`Split::HeldOut`]: a baseline gap plus an overfit PENALTY are SUBTRACTED.
    fn expected_pass_rate(&self, split: Split) -> f64 {
        let base = self.capability_pass_rate();
        let adjusted = match split {
            Split::Dev => base + OVERFIT_DEV_BONUS * self.overfit,
            Split::HeldOut => base - BASE_GEN_GAP - OVERFIT_HELDOUT_PEN * self.overfit,
        };
        adjusted.clamp(0.0, 1.0)
    }
}

// --- The scorer (the Referee's held-out task-suite evaluation) --------------

/// Re-scores a submitted agent profile by running it over a modeled task suite and
/// reporting the empirical pass-rate with a **Wilson** 95% score interval. `value` is
/// the pass-rate itself (`+pass_rate`, higher better) so the orchestrator computes a
/// positive lift for a genuine improvement over the baseline profile.
///
/// On [`Split::Dev`] the overfit bonus is applied; on [`Split::HeldOut`] the overfit
/// penalty is applied — so an over-tuned profile can look good on the dev signal a
/// researcher sees and still fail the Referee's held-out gate. Held-out tasks are also
/// a DIFFERENT modeled set (a different deterministic seed stream), so the gate is
/// genuinely out-of-sample, not a relabeling of the same draws.
#[derive(Clone, Copy, Debug)]
pub struct ImprovementPlaneScorer {
    /// Number of modeled tasks in the suite (the binomial `n`). Should be at least the
    /// gate's `min_n` (12) for the held-out result to be admissible.
    n_tasks: u32,
}

impl ImprovementPlaneScorer {
    /// `n_tasks` is the modeled suite size; pass at least the gate's `min_n` (12).
    #[must_use]
    pub fn new(n_tasks: u32) -> Self {
        Self { n_tasks }
    }

    /// The modeled number of tasks in the suite.
    #[must_use]
    pub fn n_tasks(&self) -> u32 {
        self.n_tasks
    }

    /// Synchronous scoring core, exposed so non-async callers (unit tests, a
    /// continuous leaderboard) can re-score a profile without driving the future.
    ///
    /// Draws one Bernoulli outcome per modeled task against the split's expected
    /// pass-rate, using a deterministic per-(profile, split, task) seed, then reports
    /// the empirical pass-rate with a Wilson 95% interval. The held-out split uses a
    /// disjoint seed word so it is a genuinely different task set.
    #[must_use]
    pub fn measure(&self, artifact: &GenericArtifact, split: Split) -> Measurement {
        let profile = AgentProfile::from_params(&artifact.params);
        let p = profile.expected_pass_rate(split);

        // A profile fingerprint so different profiles draw different task outcomes
        // (FNV-1a over the param bits), keeping the evaluation a deterministic function
        // of the submission rather than of any external state.
        let mut fp: u64 = 0xcbf2_9ce4_8422_2325;
        for q in &artifact.params {
            for b in q.to_bits().to_le_bytes() {
                fp ^= u64::from(b);
                fp = fp.wrapping_mul(0x0000_0100_0000_01B3);
            }
        }
        // Disjoint task universes per split: the held-out suite is a different set of
        // tasks, not the dev tasks re-labeled.
        let split_word: u64 = match split {
            Split::Dev => 0x0000_0000_0000_D0D0,
            Split::HeldOut => 0x0000_0000_0000_4E1D,
        };

        let n = self.n_tasks.max(1);
        let mut passes: u32 = 0;
        for task in 0..n {
            let mix = fp ^ split_word ^ u64::from(task).wrapping_mul(0x9E37_79B1_8472_4D3F);
            if unit01(mix) < p {
                passes += 1;
            }
        }

        wilson_measurement(passes, n)
    }
}

/// A Wilson 95% score interval for a binomial proportion `passes / n`, packaged as a
/// [`Measurement`] with `value = pass_rate`. The Wilson interval is the right CI for a
/// proportion — it stays inside `[0, 1]` and is well-behaved near the extremes, unlike
/// a naive normal interval. The `value` is the empirical pass-rate (the point
/// estimate the orchestrator ranks on); `ci_lower`/`ci_upper` are the Wilson bounds,
/// which the promotion gate keys off. By construction `ci_lower <= value <= ci_upper`
/// is NOT guaranteed for Wilson (the interval is centered on a shrunk estimate), so we
/// widen the bounds to contain the point estimate, preserving the runtime invariant
/// the gate relies on without distorting the interval width.
#[must_use]
fn wilson_measurement(passes: u32, n: u32) -> Measurement {
    let n_f = f64::from(n);
    let p_hat = f64::from(passes) / n_f;
    let z = Z_95;
    let z2 = z * z;
    let denom = 1.0 + z2 / n_f;
    let center = (p_hat + z2 / (2.0 * n_f)) / denom;
    let margin = (z / denom) * ((p_hat * (1.0 - p_hat) / n_f) + z2 / (4.0 * n_f * n_f)).sqrt();
    let lo = (center - margin).max(0.0);
    let hi = (center + margin).min(1.0);
    Measurement {
        value: p_hat,
        // Keep the ci_lower <= value <= ci_upper invariant the lift/gate code asserts.
        ci_lower: lo.min(p_hat),
        ci_upper: hi.max(p_hat),
        n,
        cost: n_f,
    }
}

impl Scorer for ImprovementPlaneScorer {
    type Artifact = GenericArtifact;

    fn id(&self) -> &str {
        "improvement-plane-heldout"
    }

    fn score(
        &self,
        artifact: &Self::Artifact,
        split: Split,
    ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send {
        let m = self.measure(artifact, split);
        std::future::ready(Ok(m))
    }
}

// --- Profile builders -------------------------------------------------------

/// The baseline agent profile: a zero-knob agent (every capability and overfit at the
/// search origin). Its `params` are all-zero, which the surface accepts and the scorer
/// decodes to a `squash(0) = 0.5` fraction per knob — a mediocre starting agent a real
/// improvement must beat on held-out. Mirrors `GenericArtifact::baseline`.
#[must_use]
pub fn baseline_profile() -> GenericArtifact {
    GenericArtifact::baseline(
        ArtifactKind::AgentProfile,
        PROFILE_DIM,
        "baseline agent profile (zero-knob starting point)",
    )
}

/// Build a profile artifact directly from the five knob params (skill, prompt, tool,
/// memory, overfit) in raw (pre-squash) units. Exposed so the e2e can construct named
/// researcher start points without reaching into the index constants.
#[must_use]
pub fn profile_from_knobs(
    skill: f64,
    prompt: f64,
    tool: f64,
    memory: f64,
    overfit: f64,
    content: impl Into<String>,
) -> GenericArtifact {
    GenericArtifact::new(
        ArtifactKind::AgentProfile,
        vec![skill, prompt, tool, memory, overfit],
        content,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn knobs(skill: f64, prompt: f64, tool: f64, memory: f64, overfit: f64) -> GenericArtifact {
        profile_from_knobs(skill, prompt, tool, memory, overfit, "t")
    }

    #[test]
    fn pass_rate_rises_with_capability() {
        // A strong-capability profile passes more tasks than the zero-knob baseline,
        // on BOTH splits, when neither is overfit.
        let scorer = ImprovementPlaneScorer::new(200);
        let base = baseline_profile();
        let strong = knobs(3.0, 3.0, 3.0, 3.0, 0.0);
        for split in [Split::Dev, Split::HeldOut] {
            let bv = scorer.measure(&base, split).value;
            let sv = scorer.measure(&strong, split).value;
            assert!(
                sv > bv + 0.1,
                "capability must lift pass-rate on {split:?}: {bv} -> {sv}"
            );
        }
    }

    #[test]
    fn overfit_helps_dev_but_hurts_heldout() {
        // The generalization gap the gate exploits: an overfit profile looks BETTER on
        // dev than on held-out. Same capability, only the overfit knob differs.
        let scorer = ImprovementPlaneScorer::new(400);
        let overfit = knobs(2.0, 2.0, 2.0, 2.0, 4.0);
        let dev = scorer.measure(&overfit, Split::Dev).value;
        let held = scorer.measure(&overfit, Split::HeldOut).value;
        assert!(
            dev > held + 0.05,
            "overfit profile must score worse on held-out than dev: dev={dev} held={held}"
        );
    }

    #[test]
    fn generalizing_beats_overfit_on_heldout() {
        // A well-generalizing profile (high capability, no overfit) beats an
        // equal-effort overfit profile on the held-out split that decides payment.
        let scorer = ImprovementPlaneScorer::new(400);
        let general = knobs(3.0, 3.0, 2.5, 2.0, 0.0);
        let overfit = knobs(1.0, 1.0, 0.5, 0.5, 5.0);
        let g = scorer.measure(&general, Split::HeldOut).value;
        let o = scorer.measure(&overfit, Split::HeldOut).value;
        assert!(
            g > o,
            "generalizing profile must win on held-out: general={g} overfit={o}"
        );
    }

    #[test]
    fn dev_and_heldout_are_disjoint_task_sets() {
        // Even with overfit=0 the two splits draw different task outcomes (disjoint
        // seed universes), so the held-out gate is genuinely out-of-sample. A profile
        // pinned to a clearly-passing rate should still differ shard-by-shard.
        let scorer = ImprovementPlaneScorer::new(64);
        let p = knobs(0.5, 0.5, 0.5, 0.5, 0.0);
        let dev = scorer.measure(&p, Split::Dev);
        let held = scorer.measure(&p, Split::HeldOut);
        // Different task universes => generally different empirical pass counts.
        assert_ne!(
            dev.value, held.value,
            "dev and held-out must be different task draws"
        );
    }

    #[test]
    fn measurement_is_deterministic() {
        let scorer = ImprovementPlaneScorer::new(128);
        let p = knobs(1.5, 1.0, 0.8, 0.4, 1.0);
        let a = scorer.measure(&p, Split::HeldOut);
        let b = scorer.measure(&p, Split::HeldOut);
        assert_eq!(a.value, b.value);
        assert_eq!(a.ci_lower, b.ci_lower);
        assert_eq!(a.ci_upper, b.ci_upper);
    }

    #[test]
    fn wilson_interval_brackets_value_and_stays_in_unit() {
        // The runtime gate asserts ci_lower <= value <= ci_upper and a proportion must
        // stay in [0, 1]. Check across the full pass-count range, including 0 and n.
        let n = 20;
        for passes in 0..=n {
            let m = wilson_measurement(passes, n);
            assert!(m.value >= 0.0 && m.value <= 1.0);
            assert!(m.ci_lower >= 0.0 && m.ci_upper <= 1.0);
            assert!(
                m.ci_lower <= m.value && m.value <= m.ci_upper,
                "interval must bracket the point estimate at passes={passes}"
            );
            assert_eq!(m.n, n);
        }
    }

    #[test]
    fn higher_n_tightens_the_interval() {
        // More tasks => a tighter Wilson interval at the same proportion (real power).
        let narrow = wilson_measurement(150, 300);
        let wide = wilson_measurement(5, 10); // same 0.5 proportion, fewer tasks
        let w_narrow = narrow.ci_upper - narrow.ci_lower;
        let w_wide = wide.ci_upper - wide.ci_lower;
        assert!(
            w_narrow < w_wide,
            "more tasks must tighten the CI: {w_narrow} vs {w_wide}"
        );
    }

    #[tokio::test]
    async fn score_future_matches_measure() {
        let scorer = ImprovementPlaneScorer::new(32);
        let p = knobs(1.0, 1.0, 1.0, 1.0, 0.0);
        let via_future = scorer.score(&p, Split::Dev).await.unwrap();
        let via_sync = scorer.measure(&p, Split::Dev);
        assert_eq!(via_future.value, via_sync.value);
    }
}
