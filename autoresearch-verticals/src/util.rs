//! Shared deterministic helpers used across the verticals.
//!
//! Centralized in the KISS / honesty pass — previously each vertical copy-pasted
//! the same splitmix64 noise fn and the same `max(0, x)` helper. One source of
//! truth means every seeded measurement stays byte-reproducible (no `rand`, no
//! clock, no I/O), which is what lets the e2e tests assert concrete lift numbers.

/// A splitmix64 finalizer mapped to a uniform `f64` in `[-1, 1)`. Deterministic
/// from its input mix word, so callers that feed it a seed-derived mix get a
/// reproducible perturbation — the basis of every vertical's seeded eval noise.
pub(crate) fn jitter(mix: u64) -> f64 {
    let mut z = mix.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let bits = z >> 11; // top 53 bits
    let unit = (bits as f64) / ((1u64 << 53) as f64);
    2.0 * unit - 1.0
}

/// `max(0, x)` — the positive part, for one-sided penalty terms.
pub(crate) fn pos(x: f64) -> f64 {
    x.max(0.0)
}
