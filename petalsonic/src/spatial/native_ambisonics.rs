use crate::error::{PetalSonicError, Result};
use crate::math::Vec3;
use crate::spatial::native_hrtf::{NativeHrtfRenderMetrics, NativeHrtfTable};
use std::f32::consts::PI;
use std::sync::Arc;
use std::time::Instant;

/// Native Ambisonics order used by re-flora for Steam Audio parity experiments.
pub const DEFAULT_NATIVE_AMBISONICS_ORDER: u32 = 2;
const MAX_NATIVE_AMBISONICS_ORDER: u32 = 2;
const MAX_NATIVE_AMBISONICS_CHANNELS: usize = 9;

/// Returns the number of ACN channels for an Ambisonics order.
pub fn native_ambisonics_channel_count(order: u32) -> Result<usize> {
    if order > MAX_NATIVE_AMBISONICS_ORDER {
        return Err(PetalSonicError::Configuration(format!(
            "native Ambisonics currently supports order 0..={MAX_NATIVE_AMBISONICS_ORDER}, got {order}"
        )));
    }

    Ok(((order + 1) * (order + 1)) as usize)
}

/// Compute real ACN/N3D spherical-harmonic coefficients up to order 2.
///
/// The listener-local direction convention is the same as native HRTF:
/// `x=right`, `y=up`, `z=front`. Coefficients are returned in ACN order:
/// 0, then 1:-1/0/1, then 2:-2/-1/0/1/2.
fn native_ambisonics_coefficients(
    order: u32,
    direction: Vec3,
) -> Result<[f32; MAX_NATIVE_AMBISONICS_CHANNELS]> {
    native_ambisonics_channel_count(order)?;

    let direction = normalize_or_front(direction);
    let x = direction.x;
    let y = direction.y;
    let z = direction.z;

    let mut coeffs = [0.0; MAX_NATIVE_AMBISONICS_CHANNELS];
    coeffs[0] = 0.282_094_8; // sqrt(1 / 4pi)

    if order >= 1 {
        coeffs[1] = 0.488_602_52 * y;
        coeffs[2] = 0.488_602_52 * z;
        coeffs[3] = 0.488_602_52 * x;
    }

    if order >= 2 {
        coeffs[4] = 1.092_548_5 * x * y;
        coeffs[5] = 1.092_548_5 * y * z;
        coeffs[6] = 0.315_391_57 * (3.0 * z * z - 1.0);
        coeffs[7] = 1.092_548_5 * x * z;
        coeffs[8] = 0.546_274_24 * (x * x - y * y);
    }

    Ok(coeffs)
}

/// Native Ambisonics encoder for mono point sources.
#[derive(Debug, Clone)]
pub struct NativeAmbisonicsEncoder {
    order: u32,
    channel_count: usize,
}

impl NativeAmbisonicsEncoder {
    pub fn new(order: u32) -> Result<Self> {
        let channel_count = native_ambisonics_channel_count(order)?;
        Ok(Self {
            order,
            channel_count,
        })
    }

    pub fn order(&self) -> u32 {
        self.order
    }

    pub fn channel_count(&self) -> usize {
        self.channel_count
    }

    /// Encode `input` and accumulate it into a planar Ambisonics output buffer.
    pub fn encode_source_accumulate(
        &self,
        direction: Vec3,
        input: &[f32],
        output_planar: &mut [f32],
    ) -> Result<()> {
        let frames = input.len();
        let expected_len = frames * self.channel_count;
        if output_planar.len() < expected_len {
            return Err(PetalSonicError::Configuration(format!(
                "native Ambisonics output buffer too small: need {expected_len} samples, got {}",
                output_planar.len()
            )));
        }

        let coeffs = native_ambisonics_coefficients(self.order, direction)?;
        for channel in 0..self.channel_count {
            let coeff = coeffs[channel];
            let channel_offset = channel * frames;
            for (frame_index, input_sample) in input.iter().copied().enumerate() {
                output_planar[channel_offset + frame_index] += input_sample * coeff;
            }
        }

        Ok(())
    }
}

/// Delay-line state for native Ambisonics binaural decoding.
#[derive(Debug, Clone)]
pub struct NativeAmbisonicsBinauralState {
    delay_lines: Vec<f32>,
    write_index: usize,
}

impl NativeAmbisonicsBinauralState {
    fn new(channel_count: usize, taps: usize) -> Self {
        Self {
            delay_lines: vec![0.0; channel_count * taps],
            write_index: 0,
        }
    }

    pub fn reset(&mut self) {
        self.delay_lines.fill(0.0);
        self.write_index = 0;
    }
}

/// Native Ambisonics binaural decoder derived from the native HRTF table.
#[derive(Debug, Clone)]
pub struct NativeAmbisonicsBinauralDecoder {
    order: u32,
    channel_count: usize,
    taps: usize,
    left_filters: Vec<f32>,
    right_filters: Vec<f32>,
}

impl NativeAmbisonicsBinauralDecoder {
    pub fn new(table: Arc<NativeHrtfTable>, order: u32) -> Result<Self> {
        let channel_count = native_ambisonics_channel_count(order)?;
        let taps = table.taps();
        let mut left_filters = vec![0.0; channel_count * taps];
        let mut right_filters = vec![0.0; channel_count * taps];
        let direction_count = table.direction_count();
        if direction_count == 0 {
            return Err(PetalSonicError::Configuration(
                "native Ambisonics decoder requires at least one HRTF direction".to_string(),
            ));
        }

        // Approximate the spherical integral that maps an Ambisonics field to binaural HRIRs.
        // This assumes the HRTF measurements are reasonably distributed over the sphere.
        let weight = 4.0 * PI / direction_count as f32;
        for index in 0..direction_count {
            let entry = table.direction(index).ok_or_else(|| {
                PetalSonicError::Configuration(format!(
                    "native HRTF direction index {index} disappeared during decoder build"
                ))
            })?;
            let coeffs = native_ambisonics_coefficients(order, entry.direction)?;

            for channel in 0..channel_count {
                let scaled_coeff = coeffs[channel] * weight;
                let filter_offset = channel * taps;
                for tap in 0..taps {
                    left_filters[filter_offset + tap] += entry.left[tap] * scaled_coeff;
                    right_filters[filter_offset + tap] += entry.right[tap] * scaled_coeff;
                }
            }
        }

        Ok(Self {
            order,
            channel_count,
            taps,
            left_filters,
            right_filters,
        })
    }

    pub fn order(&self) -> u32 {
        self.order
    }

    pub fn channel_count(&self) -> usize {
        self.channel_count
    }

    pub fn create_state(&self) -> NativeAmbisonicsBinauralState {
        NativeAmbisonicsBinauralState::new(self.channel_count, self.taps)
    }

    /// Decode a planar Ambisonics buffer into an interleaved stereo output buffer.
    ///
    /// Output is accumulated, not cleared.
    pub fn decode(
        &self,
        state: &mut NativeAmbisonicsBinauralState,
        input_planar: &[f32],
        output_interleaved: &mut [f32],
    ) -> Result<NativeHrtfRenderMetrics> {
        if self.taps == 0 {
            return Err(PetalSonicError::Configuration(
                "native Ambisonics decoder has zero taps".to_string(),
            ));
        }

        if input_planar.len() % self.channel_count != 0 {
            return Err(PetalSonicError::Configuration(format!(
                "native Ambisonics input length {} is not divisible by channel count {}",
                input_planar.len(),
                self.channel_count
            )));
        }

        let frames = input_planar.len() / self.channel_count;
        if output_interleaved.len() < frames * 2 {
            return Err(PetalSonicError::Configuration(format!(
                "native Ambisonics decode output buffer too small: need {}, got {} samples",
                frames * 2,
                output_interleaved.len()
            )));
        }

        if state.delay_lines.len() != self.channel_count * self.taps {
            *state = self.create_state();
        }

        let convolution_start = Instant::now();
        for frame_index in 0..frames {
            let mut left = 0.0f32;
            let mut right = 0.0f32;

            for channel in 0..self.channel_count {
                let channel_offset = channel * frames;
                let state_offset = channel * self.taps;
                let filter_offset = channel * self.taps;
                state.delay_lines[state_offset + state.write_index] =
                    input_planar[channel_offset + frame_index];

                let mut tap = 0usize;
                for delay_index in (0..=state.write_index).rev() {
                    let delayed = state.delay_lines[state_offset + delay_index];
                    left += delayed * self.left_filters[filter_offset + tap];
                    right += delayed * self.right_filters[filter_offset + tap];
                    tap += 1;
                }
                for delay_index in (state.write_index + 1..self.taps).rev() {
                    let delayed = state.delay_lines[state_offset + delay_index];
                    left += delayed * self.left_filters[filter_offset + tap];
                    right += delayed * self.right_filters[filter_offset + tap];
                    tap += 1;
                }
            }

            let out_index = frame_index * 2;
            output_interleaved[out_index] += left;
            output_interleaved[out_index + 1] += right;
            state.write_index = (state.write_index + 1) % self.taps;
        }

        Ok(NativeHrtfRenderMetrics {
            direction_lookup_time_us: 0,
            convolution_time_us: convolution_start.elapsed().as_micros() as u64,
        })
    }
}

fn normalize_or_front(direction: Vec3) -> Vec3 {
    if direction.is_finite() && direction.length_squared() > f32::EPSILON {
        direction.normalize()
    } else {
        Vec3::Z
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::native_hrtf::{NativeHrtfDirection, NativeHrtfTable};

    #[test]
    fn channel_count_matches_ambisonics_order() {
        assert_eq!(native_ambisonics_channel_count(0).unwrap(), 1);
        assert_eq!(native_ambisonics_channel_count(1).unwrap(), 4);
        assert_eq!(native_ambisonics_channel_count(2).unwrap(), 9);
        assert!(native_ambisonics_channel_count(3).is_err());
    }

    #[test]
    fn native_encode_accumulates_planar_channels() {
        let encoder = NativeAmbisonicsEncoder::new(0).unwrap();
        let mut encoded = [1.0, 2.0, 3.0];
        encoder
            .encode_source_accumulate(Vec3::Z, &[4.0, 5.0, 6.0], &mut encoded)
            .unwrap();

        assert!((encoded[0] - (1.0 + 4.0 * 0.282_094_8)).abs() < 1e-6);
        assert!((encoded[1] - (2.0 + 5.0 * 0.282_094_8)).abs() < 1e-6);
        assert!((encoded[2] - (3.0 + 6.0 * 0.282_094_8)).abs() < 1e-6);
    }

    #[test]
    fn order_zero_decode_reconstructs_single_direction_hrir() {
        let table = Arc::new(
            NativeHrtfTable::new(
                48_000,
                vec![NativeHrtfDirection::new(Vec3::Z, vec![1.0], vec![2.0])],
            )
            .unwrap(),
        );
        let encoder = NativeAmbisonicsEncoder::new(0).unwrap();
        let decoder = NativeAmbisonicsBinauralDecoder::new(table, 0).unwrap();
        let mut state = decoder.create_state();
        let mut encoded = [0.0];
        let mut output = [0.0, 0.0];

        encoder
            .encode_source_accumulate(Vec3::Z, &[1.0], &mut encoded)
            .unwrap();
        decoder.decode(&mut state, &encoded, &mut output).unwrap();

        assert!((output[0] - 1.0).abs() < 1e-5);
        assert!((output[1] - 2.0).abs() < 1e-5);
    }
}
