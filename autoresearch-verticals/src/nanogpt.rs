//! A **real** auto-research vertical: improving a char-level nanoGPT.
//!
//! The proposer's "held-out test" is nanoGPT's `val.bin`; the [`NanoGptScorer`]
//! shells out to a real training loop (`experiments/nanogpt/nanogpt_eval.py`),
//! trains the candidate config for a fixed compute budget over several seeds, and
//! returns the held-out val loss with a confidence interval. A candidate config that
//! reaches a lower val loss at the same budget is a genuine improvement, so the
//! market's certified lift is `baseline_val_loss − candidate_val_loss`.
//!
//! This is not a synthetic stand-in: it runs Karpathy's nanoGPT model on real data.
//! It needs Python + torch + the prepared `shakespeare_char` data, so the end-to-end
//! competition lives behind an `#[ignore]`d integration test, not the fast gates.
//!
//! Score convention: a [`Measurement`]'s `value` is **negative val loss** (higher is
//! better), so the orchestrator's `lift = candidate.value − baseline.value` equals the
//! reduction in val loss.

use std::future::Future;
use std::path::PathBuf;
use std::process::Command;

use autoresearch_runtime::traits::{
    Engine, EngineContext, EngineError, Scorer, ScorerError, Surface, SurfaceError,
};
use autoresearch_runtime::types::{ArtifactRef, Measurement, Split};

/// The tunable Surface: the hyper-/architecture-parameters a researcher may change.
/// The compute budget (`max_iters`) is fixed by the competition (the [`NanoGptScorer`]),
/// not the config — every candidate is trained at the same budget for a fair comparison.
#[derive(Clone, Debug, PartialEq)]
pub struct NanoGptConfig {
    pub learning_rate: f64,
    pub n_layer: u32,
    pub n_head: u32,
    pub n_embd: u32,
    pub dropout: f64,
    pub weight_decay: f64,
    pub warmup_iters: u32,
    pub block_size: u32,
    pub batch_size: u32,
}

impl NanoGptConfig {
    /// The small baseline config (val loss ~2.41 at a 300-iter budget).
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            learning_rate: 1e-3,
            n_layer: 4,
            n_head: 4,
            n_embd: 128,
            dropout: 0.0,
            weight_decay: 0.1,
            warmup_iters: 20,
            block_size: 64,
            batch_size: 12,
        }
    }

    /// Serialize to the JSON the Python eval wrapper consumes, pinning `seed` and the
    /// competition's `max_iters` budget.
    fn to_eval_json(&self, seed: u32, max_iters: u32) -> String {
        format!(
            concat!(
                r#"{{"learning_rate":{lr},"n_layer":{nl},"n_head":{nh},"n_embd":{ne},"#,
                r#""dropout":{dr},"weight_decay":{wd},"warmup_iters":{wu},"#,
                r#""block_size":{bs},"batch_size":{bz},"max_iters":{mi},"#,
                r#""eval_interval":{ei},"seed":{sd}}}"#
            ),
            lr = self.learning_rate,
            nl = self.n_layer,
            nh = self.n_head,
            ne = self.n_embd,
            dr = self.dropout,
            wd = self.weight_decay,
            wu = self.warmup_iters,
            bs = self.block_size,
            bz = self.batch_size,
            mi = max_iters,
            ei = (max_iters / 3).max(1),
            sd = seed,
        )
    }

    fn short_hash(&self) -> u64 {
        // FNV-1a over the eval JSON (budget-independent fields) for a stable ref.
        let s = self.to_eval_json(0, 0);
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in s.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
        h
    }
}

/// Surface over [`NanoGptConfig`]. Validates parameter bounds before any (expensive)
/// training; full-replacement (a candidate is a whole config), so `apply_delta` returns
/// the delta.
pub struct NanoGptSurface;

impl Surface for NanoGptSurface {
    type Artifact = NanoGptConfig;

    fn id(&self) -> &str {
        "nanogpt-config"
    }

    fn validate(&self, a: &NanoGptConfig) -> Result<(), SurfaceError> {
        if !(a.learning_rate.is_finite() && a.learning_rate > 0.0 && a.learning_rate <= 1.0) {
            return Err(SurfaceError::Invalid(format!(
                "learning_rate {}",
                a.learning_rate
            )));
        }
        if a.n_layer == 0 || a.n_head == 0 || a.n_embd == 0 {
            return Err(SurfaceError::Invalid(
                "n_layer/n_head/n_embd must be > 0".into(),
            ));
        }
        if !a.n_embd.is_multiple_of(a.n_head) {
            return Err(SurfaceError::Invalid(
                "n_embd must be divisible by n_head".into(),
            ));
        }
        if !(a.dropout.is_finite() && (0.0..1.0).contains(&a.dropout)) {
            return Err(SurfaceError::Invalid(format!("dropout {}", a.dropout)));
        }
        Ok(())
    }

    fn apply_delta(
        &self,
        _base: &NanoGptConfig,
        delta: &NanoGptConfig,
    ) -> Result<NanoGptConfig, SurfaceError> {
        Ok(delta.clone())
    }

    fn to_ref(&self, a: &NanoGptConfig) -> Result<ArtifactRef, SurfaceError> {
        Ok(ArtifactRef(format!("nanogpt:{:016x}", a.short_hash())))
    }
}

/// The referee's measuring instrument: trains a candidate config for `budget_iters` over
/// `seeds` independent runs and reports the held-out val loss (as negative-loss `value`)
/// with a normal-approximation CI across seeds.
pub struct NanoGptScorer {
    python: PathBuf,
    wrapper: PathBuf,
    seeds: u32,
    budget_iters: u32,
}

impl NanoGptScorer {
    /// Resolve from env (`NANOGPT_PYTHON`, `NANOGPT_WRAPPER`) with repo defaults.
    #[must_use]
    pub fn new(seeds: u32, budget_iters: u32) -> Self {
        let python = std::env::var("NANOGPT_PYTHON")
            .unwrap_or_else(|_| shellexpand_home("~/code/nanogpt-venv/bin/python"));
        let wrapper = std::env::var("NANOGPT_WRAPPER")
            .unwrap_or_else(|_| "experiments/nanogpt/nanogpt_eval.py".to_string());
        Self {
            python: PathBuf::from(python),
            wrapper: PathBuf::from(wrapper),
            seeds,
            budget_iters,
        }
    }

    /// Run one training and parse the best val loss + wall-seconds from the wrapper's
    /// final JSON line.
    fn run_one(&self, cfg: &NanoGptConfig, seed: u32) -> Result<(f64, f64), ScorerError> {
        let out = Command::new(&self.python)
            .arg(&self.wrapper)
            .arg(cfg.to_eval_json(seed, self.budget_iters))
            .output()
            .map_err(|e| ScorerError::Io(format!("spawn {}: {e}", self.python.display())))?;
        if !out.status.success() {
            return Err(ScorerError::Unavailable(format!(
                "eval failed: {}",
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .last()
                    .unwrap_or("")
            )));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let last = stdout
            .lines()
            .rev()
            .find(|l| l.contains("\"val_loss\""))
            .ok_or_else(|| ScorerError::Io("no val_loss in eval output".into()))?;
        let val_loss = extract_number(last, "\"val_loss\":")
            .ok_or_else(|| ScorerError::Io(format!("unparseable val_loss: {last}")))?;
        let secs = extract_number(last, "\"seconds\":").unwrap_or(0.0);
        Ok((val_loss, secs))
    }
}

impl Scorer for NanoGptScorer {
    type Artifact = NanoGptConfig;

    fn id(&self) -> &str {
        "nanogpt-held-out-val-loss"
    }

    fn score(
        &self,
        artifact: &NanoGptConfig,
        _split: Split,
    ) -> impl Future<Output = Result<Measurement, ScorerError>> + Send {
        // Run all seeds synchronously, then resolve. The future is `Send` and runs to
        // completion in one poll — fine for the (sequential, `#[ignore]`d) competition.
        let result = (|| {
            let mut losses = Vec::with_capacity(self.seeds as usize);
            let mut cost = 0.0;
            for seed in 0..self.seeds {
                let (loss, secs) = self.run_one(artifact, seed)?;
                losses.push(loss);
                cost += secs;
            }
            Ok(measurement_from_losses(&losses, cost))
        })();
        std::future::ready(result)
    }
}

/// `value = -mean_val_loss` (higher is better); CI is the normal-approx interval on the
/// mean val loss, mapped through the negation.
fn measurement_from_losses(losses: &[f64], cost: f64) -> Measurement {
    let n = losses.len().max(1);
    let mean = losses.iter().sum::<f64>() / n as f64;
    let var = if n > 1 {
        losses.iter().map(|l| (l - mean).powi(2)).sum::<f64>() / (n - 1) as f64
    } else {
        0.0
    };
    let se = (var / n as f64).sqrt();
    let h = 1.96 * se;
    Measurement {
        value: -mean,
        ci_lower: -(mean + h),
        ci_upper: -(mean - h),
        n: n as u32,
        cost,
    }
}

/// An engine that submits a fixed researcher hypothesis (one config). A production
/// researcher runs a full search loop in the operator sandbox; here each engine emits
/// its best config so the validation focuses on the market scoring real training.
pub struct FixedConfigEngine {
    id: String,
    config: NanoGptConfig,
}

impl FixedConfigEngine {
    #[must_use]
    pub fn new(id: impl Into<String>, config: NanoGptConfig) -> Self {
        Self {
            id: id.into(),
            config,
        }
    }
}

impl Engine for FixedConfigEngine {
    type Artifact = NanoGptConfig;

    fn id(&self) -> &str {
        &self.id
    }

    fn produce(
        &self,
        _ctx: &EngineContext,
    ) -> impl Future<Output = Result<NanoGptConfig, EngineError>> + Send {
        std::future::ready(Ok(self.config.clone()))
    }
}

fn extract_number(line: &str, key: &str) -> Option<f64> {
    let rest = line.split(key).nth(1)?;
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim().parse::<f64>().ok()
}

fn shellexpand_home(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => p.to_string(),
        },
        None => p.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_config_is_valid_and_serializes() {
        let s = NanoGptSurface;
        let b = NanoGptConfig::baseline();
        assert!(s.validate(&b).is_ok());
        let json = b.to_eval_json(7, 300);
        assert!(json.contains("\"seed\":7"));
        assert!(json.contains("\"max_iters\":300"));
        assert!(json.contains("\"learning_rate\":0.001"));
    }

    #[test]
    fn surface_rejects_bad_configs() {
        let s = NanoGptSurface;
        let mut c = NanoGptConfig::baseline();
        c.n_embd = 130; // not divisible by n_head=4
        assert!(s.validate(&c).is_err());
        c = NanoGptConfig::baseline();
        c.learning_rate = 0.0;
        assert!(s.validate(&c).is_err());
    }

    #[test]
    fn measurement_maps_loss_to_negative_value_with_ci() {
        // Lower loss => higher value; a tighter spread => tighter CI.
        let m = measurement_from_losses(&[2.40, 2.42, 2.41, 2.39], 10.0);
        assert!((m.value + 2.405).abs() < 1e-6); // value == -mean
        assert!(m.ci_lower < m.value && m.value < m.ci_upper);
        assert_eq!(m.n, 4);
    }

    #[test]
    fn extract_number_parses_wrapper_json() {
        let line = r#"{"val_loss": 2.2464, "final_val_loss": 2.25, "seconds": 8.66}"#;
        assert!((extract_number(line, "\"val_loss\":").unwrap() - 2.2464).abs() < 1e-9);
        assert!((extract_number(line, "\"seconds\":").unwrap() - 8.66).abs() < 1e-9);
    }
}
