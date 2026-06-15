//! Economic and payment configuration for an operator deployment.
//!
//! [`EconomicConfig`] is the single place a referee/operator tunes the market's
//! money knobs: the promotion gate defaults, the researcher stake floor, how a
//! competition's fee is split across the operator/referee/validator roles, the
//! default reward shape, and the x402 per-job price weights. Every field has a
//! sane default and an environment-variable override, so a deployment runs with
//! zero configuration and an operator can retune any single knob without touching
//! code. See `OPERATORS.md` for the env-var table and the documented defaults.
//!
//! This config is a *deployment* concern, distinct from the per-competition
//! mechanism in `autoresearch-protocol`/`autoresearch-runtime`: it provides the
//! defaults an operator's binary applies when a competition does not specify its
//! own, and the off-chain pricing the x402 gateway charges per job.

use autoresearch_runtime::Gate;
use std::collections::BTreeMap;

use crate::{
    JOB_CHALLENGE, JOB_COMMIT_CANDIDATE, JOB_CREATE_COMPETITION, JOB_JOIN, JOB_REPORT_SCORE,
    JOB_REVEAL_CANDIDATE, JOB_SETTLE, JOB_TICK,
};

/// Fee split denominator: the three role shares MUST sum to exactly 100.
pub const FEE_SPLIT_TOTAL: u8 = 100;

/// How a competition's protocol fee is divided across the three roles, in whole
/// percent. The shares MUST sum to [`FEE_SPLIT_TOTAL`]; [`FeeSplit::validate`]
/// enforces this and [`FeeSplit::from_env`] fails closed to the default if an
/// override does not sum to 100.
///
/// Default `55 / 30 / 15` mirrors the trading blueprint's role weighting: the
/// operator running the service earns the largest share, the referee doing the
/// held-out scoring earns the next, and validators auditing the result earn the
/// remainder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeeSplit {
    /// Operator share (runs the blueprint service), in whole percent.
    pub operator_pct: u8,
    /// Referee share (runs the held-out scoring), in whole percent.
    pub referee_pct: u8,
    /// Validator share (audits reported scores), in whole percent.
    pub validator_pct: u8,
}

impl Default for FeeSplit {
    fn default() -> Self {
        Self {
            operator_pct: 55,
            referee_pct: 30,
            validator_pct: 15,
        }
    }
}

impl FeeSplit {
    /// Whether the three shares sum to exactly 100%.
    #[must_use]
    pub fn validate(&self) -> bool {
        u16::from(self.operator_pct) + u16::from(self.referee_pct) + u16::from(self.validator_pct)
            == u16::from(FEE_SPLIT_TOTAL)
    }

    /// Load the split from `FEE_SPLIT_OPERATOR` / `FEE_SPLIT_REFEREE` /
    /// `FEE_SPLIT_VALIDATOR`. Fails closed: if any var is unparsable OR the
    /// resulting shares do not sum to 100, the default split is returned. This
    /// guarantees a deployment can never run with a fee split that mints or burns
    /// value relative to the pool.
    ///
    /// A silent fallback hides operator intent — a single mistyped var would
    /// discard the entire override unnoticed. So this also distinguishes "unset"
    /// from "set-but-rejected": whenever any `FEE_SPLIT_*` var is present but the
    /// override is discarded (because a var failed to parse or the shares did not
    /// sum to 100), it logs a hard warning naming the rejected values, so the
    /// operator notices their split was dropped instead of applied.
    #[must_use]
    pub fn from_env() -> Self {
        let default = Self::default();
        // Track presence separately from parse success so a mistyped var (present
        // but unparsable) is surfaced rather than silently treated as "unset".
        let operator = env_u8_opt("FEE_SPLIT_OPERATOR");
        let referee = env_u8_opt("FEE_SPLIT_REFEREE");
        let validator = env_u8_opt("FEE_SPLIT_VALIDATOR");
        let any_present = operator.present || referee.present || validator.present;

        let candidate = Self {
            operator_pct: operator.value.unwrap_or(default.operator_pct),
            referee_pct: referee.value.unwrap_or(default.referee_pct),
            validator_pct: validator.value.unwrap_or(default.validator_pct),
        };
        if candidate.validate() {
            if any_present {
                blueprint_sdk::warn!(
                    operator = candidate.operator_pct,
                    referee = candidate.referee_pct,
                    validator = candidate.validator_pct,
                    "applied operator FEE_SPLIT override"
                );
            }
            return candidate;
        }

        // The override was rejected. Name what was seen so the operator notices
        // their split was discarded (fail-closed to default) instead of running.
        if any_present {
            let unparsable =
                operator.unparsable() || referee.unparsable() || validator.unparsable();
            blueprint_sdk::warn!(
                operator = ?operator.raw,
                referee = ?referee.raw,
                validator = ?validator.raw,
                had_unparsable = unparsable,
                applied_operator = default.operator_pct,
                applied_referee = default.referee_pct,
                applied_validator = default.validator_pct,
                "FEE_SPLIT override rejected (unparsable var or shares did not sum to 100); \
                 falling back to the default split — your configured split was NOT applied"
            );
        }
        default
    }
}

/// The shape of the default reward schedule an operator advertises. A competition
/// may override its own schedule on-chain; this is the deployment default surfaced
/// to requesters who do not specify one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultRewardShape {
    /// Winner-take-all at the deadline.
    TerminalPrize,
    /// Split across the ranked top-k (the operator advertises a top-3 default).
    SnapshotTopK,
    /// Continuous king-of-the-hill (marginal record bounty).
    RecordBounty,
}

impl DefaultRewardShape {
    fn from_env(default: Self) -> Self {
        match std::env::var("DEFAULT_REWARD_SHAPE").ok().as_deref() {
            Some("terminal_prize") => Self::TerminalPrize,
            Some("snapshot_topk") => Self::SnapshotTopK,
            Some("record_bounty") => Self::RecordBounty,
            _ => default,
        }
    }
}

/// The full economic + payment configuration for a deployment.
#[derive(Clone, Debug, PartialEq)]
pub struct EconomicConfig {
    /// The default promotion gate (`min_lift_ci_lower`, `min_n`, optional cost
    /// ceiling) applied when a competition does not declare its own.
    pub gate: Gate,
    /// The default researcher stake floor in wei (anti-spam / leakage bond).
    pub min_stake_wei: u128,
    /// How a competition fee is split across operator/referee/validator.
    pub fee_split: FeeSplit,
    /// The default reward schedule shape advertised to requesters.
    pub default_reward_shape: DefaultRewardShape,
    /// x402 per-job price *weights*. Each weight is multiplied by
    /// [`EconomicConfig::base_price_wei`] to derive the wei price charged to call
    /// a job. Status-like jobs carry weight 0 (free); economically heavy jobs
    /// (CREATE) carry the largest weight. Keyed by job id; populated for all 8
    /// jobs.
    pub job_price_weights: BTreeMap<u8, u32>,
    /// The wei multiplier applied to each job weight to get its x402 price.
    pub base_price_wei: u128,
}

impl Default for EconomicConfig {
    fn default() -> Self {
        Self {
            gate: Gate::default(),
            // 1e15 wei = 0.001 native by default — a non-zero anti-spam floor that
            // an operator scales to the real scoring cost via STAKE_FLOOR_WEI.
            min_stake_wei: 1_000_000_000_000_000,
            fee_split: FeeSplit::default(),
            default_reward_shape: DefaultRewardShape::SnapshotTopK,
            job_price_weights: default_job_price_weights(),
            base_price_wei: 1_000_000_000_000, // 1e12 wei per weight unit
        }
    }
}

impl EconomicConfig {
    /// Load the full config from the environment, falling back to defaults for any
    /// unset or unparsable knob. Pure (no side effects); reads only `std::env`.
    ///
    /// Per-field fail-closed loading guarantees the *returned* config is always
    /// internally runnable: an unparsable knob falls back to its default, the fee
    /// split fails closed (see [`FeeSplit::from_env`]), and a gate override that
    /// would be nonsensical (e.g. `GATE_MIN_N=0`) falls back to the default gate
    /// rather than producing a powerless gate. Deployment-level rejection of a
    /// misconfigured operator override is a *separate* concern: call
    /// [`EconomicConfig::validate`] (which `main` does) to hard-error on it.
    #[must_use]
    pub fn from_env() -> Self {
        let default = Self::default();

        // Load the gate, then fail closed to the default gate if the override is
        // nonsensical. Without this floor a `GATE_MIN_N=0` override would yield a
        // gate that accepts a "win" computed from zero paired episodes (`clears`
        // only checks `lift.n < min_n`, so `n=0 >= 0` passes), removing the
        // statistical-power floor the whole promotion mechanism rests on — the
        // exact condition `CompetitionSpec::validate` rejects.
        let loaded_gate = Gate {
            min_lift_ci_lower: env_f64("GATE_MIN_LIFT_CI_LOWER", default.gate.min_lift_ci_lower),
            cost_per_task_ceiling: std::env::var("GATE_COST_PER_TASK_CEILING")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .or(default.gate.cost_per_task_ceiling),
            min_n: env_u32("GATE_MIN_N", default.gate.min_n),
        };
        let gate = if validate_gate(&loaded_gate).is_ok() {
            loaded_gate
        } else {
            blueprint_sdk::warn!(
                min_n = loaded_gate.min_n,
                min_lift_ci_lower = loaded_gate.min_lift_ci_lower,
                "GATE_* override produced a nonsensical gate; falling back to the default gate \
                 — your configured gate was NOT applied"
            );
            default.gate
        };

        // Fail closed on a zero/garbage stake floor or base price so the returned
        // config never disables the anti-spam bond or x402 revenue silently. The
        // strict deployment check is `validate`; this keeps the loaded value sane.
        let min_stake_wei = match env_u128("STAKE_FLOOR_WEI", default.min_stake_wei) {
            0 => {
                blueprint_sdk::warn!(
                    "STAKE_FLOOR_WEI=0 removes the anti-spam bond; falling back to the default \
                     floor — set it deliberately and run with validation disabled if you truly \
                     want no bond"
                );
                default.min_stake_wei
            }
            v => v,
        };
        let base_price_wei = match env_u128("X402_BASE_PRICE_WEI", default.base_price_wei) {
            0 => {
                blueprint_sdk::warn!(
                    "X402_BASE_PRICE_WEI=0 makes every job free and disables x402 revenue; \
                     falling back to the default base price"
                );
                default.base_price_wei
            }
            v => v,
        };

        Self {
            gate,
            min_stake_wei,
            fee_split: FeeSplit::from_env(),
            default_reward_shape: DefaultRewardShape::from_env(default.default_reward_shape),
            job_price_weights: default_job_price_weights(),
            base_price_wei,
        }
    }

    /// Strict deployment validation: reject any economic state that is nonsensical
    /// even though the individual fields parsed. This is the gate that prevents an
    /// operator's env overrides from producing a market that cannot pay correctly.
    ///
    /// Mirrors `CompetitionSpec::validate` for the gate (`min_n` must be positive)
    /// and adds the deployment-specific money invariants (sane stake floor, fee
    /// split sums to 100, a finite x402 base price). `from_env` already fails
    /// closed per-field, so this will pass for a config it produced; the value is
    /// in catching a config built any other way and in being the explicit,
    /// fail-loud contract `main` enforces before serving.
    pub fn validate(&self) -> Result<(), String> {
        validate_gate(&self.gate)?;
        if self.min_stake_wei == 0 {
            return Err("min_stake_wei must be non-zero (anti-spam / leakage bond)".into());
        }
        if self.base_price_wei == 0 {
            return Err("base_price_wei must be non-zero (x402 service revenue)".into());
        }
        if !self.fee_split.validate() {
            return Err("fee split shares must sum to exactly 100".into());
        }
        Ok(())
    }

    /// The x402 price (in wei) to call `job_id`, i.e. `weight * base_price_wei`.
    /// Returns `0` for free (status-like) jobs and for unknown ids.
    #[must_use]
    pub fn job_price_wei(&self, job_id: u8) -> u128 {
        let weight = self.job_price_weights.get(&job_id).copied().unwrap_or(0);
        self.base_price_wei.saturating_mul(u128::from(weight))
    }
}

/// Validate a [`Gate`] with the same rule the on-chain mechanism enforces in
/// `CompetitionSpec::validate`, plus finiteness guards on the float knobs. A gate
/// that fails this can pass `Gate::clears` for a zero-episode or `NaN`-bounded
/// "win", so it must never be loaded or deployed.
fn validate_gate(gate: &Gate) -> Result<(), String> {
    if gate.min_n == 0 {
        return Err("gate min_n must be positive".into());
    }
    if !gate.min_lift_ci_lower.is_finite() || gate.min_lift_ci_lower < 0.0 {
        return Err("gate min_lift_ci_lower must be finite and non-negative".into());
    }
    if let Some(ceiling) = gate.cost_per_task_ceiling
        && !ceiling.is_finite()
    {
        return Err("gate cost_per_task_ceiling must be finite when set".into());
    }
    Ok(())
}

/// The default per-job x402 price weights. CREATE is heaviest (it escrows a pool
/// and opens market state); REPORT_SCORE and SETTLE are medium (they run/realize
/// the held-out scoring and payout); the commit/reveal/join/challenge submission
/// jobs are light; TICK is free (operator-internal cron upkeep, never billed to a
/// requester).
fn default_job_price_weights() -> BTreeMap<u8, u32> {
    BTreeMap::from([
        (JOB_CREATE_COMPETITION, 100),
        (JOB_JOIN, 5),
        (JOB_COMMIT_CANDIDATE, 5),
        (JOB_REVEAL_CANDIDATE, 5),
        (JOB_REPORT_SCORE, 50),
        (JOB_SETTLE, 50),
        (JOB_CHALLENGE, 20),
        (JOB_TICK, 0),
    ])
}

// --- env helpers -----------------------------------------------------------

/// The result of reading a single env var as a `u8`, preserving the distinction
/// between "unset" and "set-but-unparsable" so callers can fail loud on a typo
/// instead of silently substituting a default.
struct EnvU8 {
    /// Whether the variable was present in the environment at all.
    present: bool,
    /// The raw string as read (for diagnostics); `None` if unset.
    raw: Option<String>,
    /// The parsed value; `None` if unset or unparsable.
    value: Option<u8>,
}

impl EnvU8 {
    /// True iff the variable was present but failed to parse as a `u8`.
    fn unparsable(&self) -> bool {
        self.present && self.value.is_none()
    }
}

fn env_u8_opt(key: &str) -> EnvU8 {
    match std::env::var(key) {
        Ok(raw) => {
            let value = raw.trim().parse::<u8>().ok();
            EnvU8 {
                present: true,
                raw: Some(raw),
                value,
            }
        }
        Err(_) => EnvU8 {
            present: false,
            raw: None,
            value: None,
        },
    }
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_u128(key: &str, default: u128) -> u128 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_fee_split_sums_to_100() {
        assert!(FeeSplit::default().validate());
        let s = FeeSplit::default();
        assert_eq!(
            u16::from(s.operator_pct) + u16::from(s.referee_pct) + u16::from(s.validator_pct),
            100
        );
    }

    #[test]
    fn fee_split_rejecting_non_100_sum() {
        let bad = FeeSplit {
            operator_pct: 50,
            referee_pct: 30,
            validator_pct: 15, // sums to 95
        };
        assert!(!bad.validate());
    }

    #[test]
    fn default_config_fee_split_is_valid() {
        assert!(EconomicConfig::default().fee_split.validate());
    }

    #[test]
    fn pricing_weights_exist_for_all_eight_jobs() {
        let cfg = EconomicConfig::default();
        let jobs = [
            JOB_CREATE_COMPETITION,
            JOB_JOIN,
            JOB_COMMIT_CANDIDATE,
            JOB_REVEAL_CANDIDATE,
            JOB_REPORT_SCORE,
            JOB_SETTLE,
            JOB_CHALLENGE,
            JOB_TICK,
        ];
        for job in jobs {
            assert!(
                cfg.job_price_weights.contains_key(&job),
                "missing price weight for job {job}"
            );
        }
        assert_eq!(cfg.job_price_weights.len(), 8, "exactly 8 jobs priced");
    }

    #[test]
    fn pricing_ordering_create_heaviest_tick_free() {
        let cfg = EconomicConfig::default();
        let create = cfg.job_price_wei(JOB_CREATE_COMPETITION);
        let report = cfg.job_price_wei(JOB_REPORT_SCORE);
        let join = cfg.job_price_wei(JOB_JOIN);
        let tick = cfg.job_price_wei(JOB_TICK);
        assert!(create > report, "CREATE heavier than REPORT_SCORE");
        assert!(report > join, "REPORT_SCORE heavier than JOIN");
        assert_eq!(tick, 0, "TICK is free (operator cron upkeep)");
    }

    #[test]
    fn default_gate_matches_runtime_defaults() {
        let cfg = EconomicConfig::default();
        assert_eq!(cfg.gate.min_lift_ci_lower, 0.02);
        assert_eq!(cfg.gate.min_n, 12);
    }

    #[test]
    fn default_config_validates() {
        assert!(EconomicConfig::default().validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_min_n() {
        let mut cfg = EconomicConfig::default();
        cfg.gate.min_n = 0;
        // Same rule CompetitionSpec::validate enforces.
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("min_n"), "got: {err}");
    }

    #[test]
    fn validate_rejects_non_finite_or_negative_min_lift() {
        for bad in [f64::NAN, f64::INFINITY, -0.01] {
            let mut cfg = EconomicConfig::default();
            cfg.gate.min_lift_ci_lower = bad;
            assert!(
                cfg.validate().is_err(),
                "min_lift_ci_lower={bad} must be rejected"
            );
        }
    }

    #[test]
    fn validate_rejects_non_finite_cost_ceiling() {
        let mut cfg = EconomicConfig::default();
        cfg.gate.cost_per_task_ceiling = Some(f64::NAN);
        assert!(cfg.validate().is_err());
        cfg.gate.cost_per_task_ceiling = Some(f64::INFINITY);
        assert!(cfg.validate().is_err());
        // A finite ceiling is fine.
        cfg.gate.cost_per_task_ceiling = Some(2.5);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_stake_floor() {
        let cfg = EconomicConfig {
            min_stake_wei: 0,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_base_price() {
        let cfg = EconomicConfig {
            base_price_wei: 0,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    // --- env-override tests ---------------------------------------------------
    //
    // `std::env` is process-global, so these mutate-then-read tests are serialized
    // behind a single mutex to keep them from racing each other (nextest runs
    // tests in parallel by default). Each restores the keys it touched.

    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<const N: usize>(vars: [(&str, &str); N], f: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| ((*k).to_string(), std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            // SAFETY: serialized by ENV_LOCK; no other thread reads env concurrently
            // within this crate's tests.
            unsafe { std::env::set_var(k, v) };
        }
        f();
        for (k, v) in prev {
            match v {
                Some(v) => unsafe { std::env::set_var(&k, v) },
                None => unsafe { std::env::remove_var(&k) },
            }
        }
    }

    #[test]
    fn gate_min_n_zero_does_not_yield_a_runnable_gate() {
        with_env([("GATE_MIN_N", "0")], || {
            let cfg = EconomicConfig::from_env();
            // from_env must fail closed to a powered gate, never min_n=0.
            assert_ne!(
                cfg.gate.min_n, 0,
                "GATE_MIN_N=0 must not load a powerless gate"
            );
            assert_eq!(cfg.gate.min_n, Gate::default().min_n);
            // And the loaded config must validate (the zero never reaches deployment).
            assert!(cfg.validate().is_ok());
        });
    }

    #[test]
    fn stake_floor_zero_falls_back_to_default() {
        with_env([("STAKE_FLOOR_WEI", "0")], || {
            let cfg = EconomicConfig::from_env();
            assert_ne!(
                cfg.min_stake_wei, 0,
                "STAKE_FLOOR_WEI=0 must not disable the bond"
            );
            assert!(cfg.validate().is_ok());
        });
    }

    #[test]
    fn x402_base_price_zero_falls_back_to_default() {
        with_env([("X402_BASE_PRICE_WEI", "0")], || {
            let cfg = EconomicConfig::from_env();
            assert_ne!(
                cfg.base_price_wei, 0,
                "X402_BASE_PRICE_WEI=0 must not make jobs free"
            );
            // CREATE (weight 100) is again priced.
            assert!(cfg.job_price_wei(JOB_CREATE_COMPETITION) > 0);
            assert!(cfg.validate().is_ok());
        });
    }

    #[test]
    fn fee_split_partial_override_with_unparsable_var_falls_back_to_default() {
        // Operator intends 70/25/15 but mistypes referee as 256 (out of u8 range).
        // The whole override must be discarded back to the default split, and the
        // resulting split must still sum to 100 (never mint/burn value).
        with_env(
            [
                ("FEE_SPLIT_OPERATOR", "70"),
                ("FEE_SPLIT_REFEREE", "256"),
                ("FEE_SPLIT_VALIDATOR", ""),
            ],
            || {
                let split = FeeSplit::from_env();
                assert_eq!(split, FeeSplit::default());
                assert!(split.validate());
            },
        );
    }

    #[test]
    fn fee_split_override_that_does_not_sum_to_100_falls_back() {
        with_env(
            [
                ("FEE_SPLIT_OPERATOR", "70"),
                ("FEE_SPLIT_REFEREE", "20"),
                ("FEE_SPLIT_VALIDATOR", "5"),
            ],
            || {
                // 95 != 100 -> fail closed.
                let split = FeeSplit::from_env();
                assert_eq!(split, FeeSplit::default());
            },
        );
    }

    #[test]
    fn fee_split_valid_override_is_applied() {
        with_env(
            [
                ("FEE_SPLIT_OPERATOR", "60"),
                ("FEE_SPLIT_REFEREE", "25"),
                ("FEE_SPLIT_VALIDATOR", "15"),
            ],
            || {
                let split = FeeSplit::from_env();
                assert_eq!(split.operator_pct, 60);
                assert_eq!(split.referee_pct, 25);
                assert_eq!(split.validator_pct, 15);
                assert!(split.validate());
            },
        );
    }
}
