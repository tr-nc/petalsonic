use crate::error::{PetalSonicError, Result};
use crate::math::Vec3;
use crate::spatial::native_hrtf::{NativeHrtfRenderMetrics, NativeHrtfTable};
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex, num_complex::Complex32};
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
    fft_state: Option<NativeAmbisonicsFftState>,
}

impl NativeAmbisonicsBinauralState {
    fn new(channel_count: usize, taps: usize) -> Self {
        Self {
            delay_lines: vec![0.0; channel_count * taps],
            write_index: 0,
            fft_state: None,
        }
    }

    fn with_fft_plan(
        channel_count: usize,
        taps: usize,
        fft_plan: Option<&NativeAmbisonicsFftPlan>,
    ) -> Self {
        Self {
            delay_lines: vec![0.0; channel_count * taps],
            write_index: 0,
            fft_state: fft_plan.map(NativeAmbisonicsFftState::new),
        }
    }

    pub fn reset(&mut self) {
        self.delay_lines.fill(0.0);
        self.write_index = 0;
        if let Some(fft_state) = &mut self.fft_state {
            fft_state.reset();
        }
    }
}

#[derive(Clone)]
struct NativeAmbisonicsFftFilter {
    left_spectrum: Vec<Complex32>,
    right_spectrum: Vec<Complex32>,
}

#[derive(Clone)]
struct NativeAmbisonicsFftPlan {
    block_frames: usize,
    fft_size: usize,
    inverse_scale: f32,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    filters: Vec<NativeAmbisonicsFftFilter>,
}

impl std::fmt::Debug for NativeAmbisonicsFftPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeAmbisonicsFftPlan")
            .field("block_frames", &self.block_frames)
            .field("fft_size", &self.fft_size)
            .field("channel_count", &self.filters.len())
            .finish()
    }
}

impl NativeAmbisonicsFftPlan {
    fn new(
        channel_count: usize,
        taps: usize,
        left_filters: &[f32],
        right_filters: &[f32],
        block_frames: usize,
    ) -> Result<Self> {
        if block_frames == 0 {
            return Err(PetalSonicError::Configuration(
                "native Ambisonics FFT block size must be non-zero".to_string(),
            ));
        }

        let fft_size = fft_convolution_size(block_frames, taps)?;
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(fft_size);
        let inverse = planner.plan_fft_inverse(fft_size);
        let mut padded = forward.make_input_vec();
        let mut spectrum = forward.make_output_vec();
        let mut scratch = forward.make_scratch_vec();
        let mut filters = Vec::with_capacity(channel_count);

        for channel in 0..channel_count {
            let filter_offset = channel * taps;
            padded.fill(0.0);
            padded[..taps].copy_from_slice(&left_filters[filter_offset..filter_offset + taps]);
            forward
                .process_with_scratch(&mut padded, &mut spectrum, &mut scratch)
                .map_err(|error| {
                    native_ambisonics_fft_error("precomputing left decoder filter", error)
                })?;
            let left_spectrum = spectrum.clone();

            padded.fill(0.0);
            padded[..taps].copy_from_slice(&right_filters[filter_offset..filter_offset + taps]);
            forward
                .process_with_scratch(&mut padded, &mut spectrum, &mut scratch)
                .map_err(|error| {
                    native_ambisonics_fft_error("precomputing right decoder filter", error)
                })?;
            let right_spectrum = spectrum.clone();

            filters.push(NativeAmbisonicsFftFilter {
                left_spectrum,
                right_spectrum,
            });
        }

        Ok(Self {
            block_frames,
            fft_size,
            inverse_scale: 1.0 / fft_size as f32,
            forward,
            inverse,
            filters,
        })
    }
}

#[derive(Debug, Clone)]
struct NativeAmbisonicsFftState {
    block_frames: usize,
    fft_size: usize,
    forward_input: Vec<f32>,
    input_spectrum: Vec<Complex32>,
    left_spectrum: Vec<Complex32>,
    right_spectrum: Vec<Complex32>,
    left_time: Vec<f32>,
    right_time: Vec<f32>,
    left_overlap: Vec<f32>,
    right_overlap: Vec<f32>,
    forward_scratch: Vec<Complex32>,
    inverse_scratch: Vec<Complex32>,
}

impl NativeAmbisonicsFftState {
    fn new(plan: &NativeAmbisonicsFftPlan) -> Self {
        Self {
            block_frames: plan.block_frames,
            fft_size: plan.fft_size,
            forward_input: plan.forward.make_input_vec(),
            input_spectrum: plan.forward.make_output_vec(),
            left_spectrum: plan.inverse.make_input_vec(),
            right_spectrum: plan.inverse.make_input_vec(),
            left_time: plan.inverse.make_output_vec(),
            right_time: plan.inverse.make_output_vec(),
            left_overlap: vec![0.0; plan.fft_size - plan.block_frames],
            right_overlap: vec![0.0; plan.fft_size - plan.block_frames],
            forward_scratch: plan.forward.make_scratch_vec(),
            inverse_scratch: plan.inverse.make_scratch_vec(),
        }
    }

    fn matches_plan(&self, plan: &NativeAmbisonicsFftPlan) -> bool {
        self.block_frames == plan.block_frames && self.fft_size == plan.fft_size
    }

    fn reset(&mut self) {
        self.forward_input.fill(0.0);
        self.input_spectrum.fill(Complex32::new(0.0, 0.0));
        self.left_spectrum.fill(Complex32::new(0.0, 0.0));
        self.right_spectrum.fill(Complex32::new(0.0, 0.0));
        self.left_time.fill(0.0);
        self.right_time.fill(0.0);
        self.left_overlap.fill(0.0);
        self.right_overlap.fill(0.0);
        self.forward_scratch.fill(Complex32::new(0.0, 0.0));
        self.inverse_scratch.fill(Complex32::new(0.0, 0.0));
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
    fft_plan: Option<Arc<NativeAmbisonicsFftPlan>>,
}

impl NativeAmbisonicsBinauralDecoder {
    pub fn new(table: Arc<NativeHrtfTable>, order: u32) -> Result<Self> {
        Self::from_table(table, order, None)
    }

    pub fn with_frame_size(
        table: Arc<NativeHrtfTable>,
        order: u32,
        frame_size: usize,
    ) -> Result<Self> {
        Self::from_table(table, order, Some(frame_size))
    }

    fn from_table(
        table: Arc<NativeHrtfTable>,
        order: u32,
        frame_size: Option<usize>,
    ) -> Result<Self> {
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

        let fft_plan = frame_size
            .map(|frame_size| {
                NativeAmbisonicsFftPlan::new(
                    channel_count,
                    taps,
                    &left_filters,
                    &right_filters,
                    frame_size,
                )
                .map(Arc::new)
            })
            .transpose()?;

        Ok(Self {
            order,
            channel_count,
            taps,
            left_filters,
            right_filters,
            fft_plan,
        })
    }

    pub fn order(&self) -> u32 {
        self.order
    }

    pub fn channel_count(&self) -> usize {
        self.channel_count
    }

    pub fn create_state(&self) -> NativeAmbisonicsBinauralState {
        if let Some(fft_plan) = self.fft_plan.as_deref() {
            NativeAmbisonicsBinauralState::with_fft_plan(
                self.channel_count,
                self.taps,
                Some(fft_plan),
            )
        } else {
            NativeAmbisonicsBinauralState::new(self.channel_count, self.taps)
        }
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

        self.ensure_state_compatible(state);

        let convolution_start = Instant::now();
        if let Some(plan) = self.fft_plan.as_deref() {
            if frames == plan.block_frames {
                if let Some(fft_state) = state.fft_state.as_ref() {
                    if fft_state.matches_plan(plan) {
                        self.decode_frequency_domain(
                            state,
                            plan,
                            input_planar,
                            output_interleaved,
                        )?;
                        return Ok(NativeHrtfRenderMetrics {
                            direction_lookup_time_us: 0,
                            convolution_time_us: convolution_start.elapsed().as_micros() as u64,
                        });
                    }
                }
            }
        }

        self.decode_time_domain(state, input_planar, output_interleaved, frames);

        Ok(NativeHrtfRenderMetrics {
            direction_lookup_time_us: 0,
            convolution_time_us: convolution_start.elapsed().as_micros() as u64,
        })
    }

    fn ensure_state_compatible(&self, state: &mut NativeAmbisonicsBinauralState) {
        if state.delay_lines.len() != self.channel_count * self.taps {
            *state = self.create_state();
            return;
        }

        if let Some(plan) = self.fft_plan.as_deref() {
            let needs_fft_state = state
                .fft_state
                .as_ref()
                .is_none_or(|fft_state| !fft_state.matches_plan(plan));
            if needs_fft_state {
                *state = self.create_state();
            }
        }
    }

    fn decode_frequency_domain(
        &self,
        state: &mut NativeAmbisonicsBinauralState,
        plan: &NativeAmbisonicsFftPlan,
        input_planar: &[f32],
        output_interleaved: &mut [f32],
    ) -> Result<()> {
        let fft_state = state.fft_state.as_mut().ok_or_else(|| {
            PetalSonicError::SpatialAudio(
                "native Ambisonics FFT state is not initialized".to_string(),
            )
        })?;

        fft_state.left_spectrum.fill(Complex32::new(0.0, 0.0));
        fft_state.right_spectrum.fill(Complex32::new(0.0, 0.0));

        for channel in 0..self.channel_count {
            let channel_offset = channel * plan.block_frames;
            let filter = &plan.filters[channel];

            fft_state.forward_input.fill(0.0);
            fft_state.forward_input[..plan.block_frames]
                .copy_from_slice(&input_planar[channel_offset..channel_offset + plan.block_frames]);
            plan.forward
                .process_with_scratch(
                    &mut fft_state.forward_input,
                    &mut fft_state.input_spectrum,
                    &mut fft_state.forward_scratch,
                )
                .map_err(|error| native_ambisonics_fft_error("processing input channel", error))?;

            for (((left, right), input_bin), (left_filter, right_filter)) in fft_state
                .left_spectrum
                .iter_mut()
                .zip(fft_state.right_spectrum.iter_mut())
                .zip(&fft_state.input_spectrum)
                .zip(filter.left_spectrum.iter().zip(&filter.right_spectrum))
            {
                *left += *input_bin * *left_filter;
                *right += *input_bin * *right_filter;
            }
        }
        force_real_realfft_bins(&mut fft_state.left_spectrum);
        force_real_realfft_bins(&mut fft_state.right_spectrum);

        plan.inverse
            .process_with_scratch(
                &mut fft_state.left_spectrum,
                &mut fft_state.left_time,
                &mut fft_state.inverse_scratch,
            )
            .map_err(|error| native_ambisonics_fft_error("inverse left ear", error))?;
        plan.inverse
            .process_with_scratch(
                &mut fft_state.right_spectrum,
                &mut fft_state.right_time,
                &mut fft_state.inverse_scratch,
            )
            .map_err(|error| native_ambisonics_fft_error("inverse right ear", error))?;

        let scale = plan.inverse_scale;
        for frame_index in 0..plan.block_frames {
            let previous_left = fft_state
                .left_overlap
                .get(frame_index)
                .copied()
                .unwrap_or(0.0);
            let previous_right = fft_state
                .right_overlap
                .get(frame_index)
                .copied()
                .unwrap_or(0.0);
            let out_index = frame_index * 2;
            output_interleaved[out_index] +=
                fft_state.left_time[frame_index] * scale + previous_left;
            output_interleaved[out_index + 1] +=
                fft_state.right_time[frame_index] * scale + previous_right;
        }

        refresh_overlap(
            &mut fft_state.left_overlap,
            &fft_state.left_time,
            plan.block_frames,
            scale,
        );
        refresh_overlap(
            &mut fft_state.right_overlap,
            &fft_state.right_time,
            plan.block_frames,
            scale,
        );
        append_delay_lines(
            &mut state.delay_lines,
            &mut state.write_index,
            self.channel_count,
            self.taps,
            input_planar,
            plan.block_frames,
        );

        Ok(())
    }

    fn decode_time_domain(
        &self,
        state: &mut NativeAmbisonicsBinauralState,
        input_planar: &[f32],
        output_interleaved: &mut [f32],
        frames: usize,
    ) {
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
    }
}

fn fft_convolution_size(block_frames: usize, taps: usize) -> Result<usize> {
    let linear_convolution_len = block_frames
        .checked_add(taps)
        .and_then(|value| value.checked_sub(1))
        .ok_or_else(|| {
            PetalSonicError::Configuration("native Ambisonics FFT size overflow".to_string())
        })?;
    linear_convolution_len
        .checked_next_power_of_two()
        .ok_or_else(|| {
            PetalSonicError::Configuration("native Ambisonics FFT size overflow".to_string())
        })
}

fn native_ambisonics_fft_error(context: &str, error: realfft::FftError) -> PetalSonicError {
    PetalSonicError::SpatialAudio(format!("native Ambisonics FFT {context} failed: {error}"))
}

fn force_real_realfft_bins(spectrum: &mut [Complex32]) {
    if let Some(first) = spectrum.first_mut() {
        first.im = 0.0;
    }
    if spectrum.len() > 1 {
        if let Some(last) = spectrum.last_mut() {
            last.im = 0.0;
        }
    }
}

fn refresh_overlap(overlap: &mut [f32], time_domain: &[f32], block_frames: usize, scale: f32) {
    let remaining_old = overlap.len().saturating_sub(block_frames);
    if remaining_old > 0 {
        overlap.copy_within(block_frames..block_frames + remaining_old, 0);
    }
    overlap[remaining_old..].fill(0.0);

    for (overlap_sample, tail_sample) in overlap.iter_mut().zip(&time_domain[block_frames..]) {
        *overlap_sample += *tail_sample * scale;
    }
}

fn append_delay_lines(
    delay_lines: &mut [f32],
    write_index: &mut usize,
    channel_count: usize,
    taps: usize,
    input_planar: &[f32],
    frames: usize,
) {
    for frame_index in 0..frames {
        for channel in 0..channel_count {
            let channel_offset = channel * frames;
            let state_offset = channel * taps;
            delay_lines[state_offset + *write_index] = input_planar[channel_offset + frame_index];
        }
        *write_index = (*write_index + 1) % taps;
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

    #[test]
    fn frequency_domain_decode_matches_time_domain_across_blocks() {
        let table = Arc::new(
            NativeHrtfTable::new(
                48_000,
                vec![
                    NativeHrtfDirection::new(
                        Vec3::Z,
                        vec![0.75, -0.2, 0.05],
                        vec![0.5, 0.125, -0.25],
                    ),
                    NativeHrtfDirection::new(Vec3::X, vec![0.1, 0.2, -0.1], vec![-0.3, 0.4, 0.2]),
                    NativeHrtfDirection::new(
                        Vec3::Y,
                        vec![-0.2, 0.05, 0.3],
                        vec![0.25, -0.15, 0.1],
                    ),
                ],
            )
            .unwrap(),
        );
        let time_decoder = NativeAmbisonicsBinauralDecoder::new(table.clone(), 2).unwrap();
        let fft_decoder = NativeAmbisonicsBinauralDecoder::with_frame_size(table, 2, 8).unwrap();
        let mut time_state = time_decoder.create_state();
        let mut fft_state = fft_decoder.create_state();
        let channel_count = time_decoder.channel_count();
        let frames = 8;

        for block_index in 0..3 {
            let mut input = vec![0.0f32; channel_count * frames];
            for channel in 0..channel_count {
                for frame in 0..frames {
                    let phase =
                        block_index as f32 * 0.31 + channel as f32 * 0.17 + frame as f32 * 0.13;
                    input[channel * frames + frame] = phase.sin() * 0.25;
                }
            }

            let mut time_output = vec![0.0f32; frames * 2];
            let mut fft_output = vec![0.0f32; frames * 2];
            time_decoder
                .decode(&mut time_state, &input, &mut time_output)
                .unwrap();
            fft_decoder
                .decode(&mut fft_state, &input, &mut fft_output)
                .unwrap();

            for (time_sample, fft_sample) in time_output.iter().zip(fft_output) {
                assert!((*time_sample - fft_sample).abs() < 1e-4);
            }
        }
    }
}
