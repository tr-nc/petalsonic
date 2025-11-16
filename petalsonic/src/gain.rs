//! Utilities for converting between decibels and linear gain.
//!
//! These helpers are used throughout the engine for both HRTF gain compensation
//! and per-source volume handling.

/// Smallest linear gain we clamp to when converting to dB, to avoid `-inf`.
pub const MIN_LINEAR_GAIN: f32 = 1e-6; // ~ -120 dB

/// Convert decibels to linear gain.
///
/// - `0.0` dB -> `1.0` (unity gain)
/// - `-6.0` dB ~ `0.501`
/// - `6.0` dB ~ `1.995`
#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

/// Convert linear gain to decibels relative to unity.
///
/// - `1.0` -> `0.0` dB
/// - `0.5` ~ `-6.02` dB
///
/// Values `<= 0` are clamped to `MIN_LINEAR_GAIN` to avoid negative infinity.
#[inline]
pub fn linear_to_db(linear: f32) -> f32 {
    let l = linear.max(MIN_LINEAR_GAIN);
    20.0 * l.log10()
}
