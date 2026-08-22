use super::late_reverb::{LateReverbParameters, ThreeBandFdn};
use super::native_ambisonics::{
    DEFAULT_NATIVE_AMBISONICS_ORDER, NativeAmbisonicsBinauralDecoder,
    NativeAmbisonicsBinauralState, NativeAmbisonicsEncoder, native_ambisonics_channel_count,
};
use super::native_hrtf::{
    NativeHrtfRenderMetrics, NativeHrtfRenderer, NativeHrtfSourceState, NativeHrtfTable,
};
use crate::acoustic_propagation::AcousticResponse;
use crate::config::SourceConfig;
use crate::error::{PetalSonicError, Result};
use crate::gain;
use crate::math::{Pose, Vec3};
use crate::playback::PlaybackInstance;
use crate::world::SourceId;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

const DEFAULT_NATIVE_HRTF_BYTES: &[u8] = include_bytes!("../../asset/hrtf/hrtf_b_nh172.petalhrtf");
const DIRECT_LOW_CROSSOVER_HZ: f32 = 400.0;
const DIRECT_HIGH_CROSSOVER_HZ: f32 = 4_000.0;
const DIRECT_GAIN_SMOOTHING_SECONDS: f32 = 0.05;

#[derive(Debug, Clone)]
struct NativeDirectSourceState {
    low_state: f32,
    low_mid_state: f32,
    current_gain: [f32; 3],
}

impl NativeDirectSourceState {
    fn new() -> Self {
        Self {
            low_state: 0.0,
            low_mid_state: 0.0,
            current_gain: [1.0; 3],
        }
    }
}

/// Backend allocations removed from active rendering and transferred to the
/// non-render supervisor for destruction.
pub(crate) struct RetiredSpatialSource {
    _native_hrtf: Option<NativeHrtfSourceState>,
    _native_direct: Option<NativeDirectSourceState>,
}

pub(crate) struct SpatialProcessorConfig {
    pub sample_rate: u32,
    pub frame_size: usize,
    pub max_voices: usize,
    pub distance_scaler: f32,
    pub native_hrtf_path: Option<String>,
    pub hrtf_gain: f32,
    pub use_ambisonics: bool,
    pub environmental_acoustics_enabled: Arc<AtomicBool>,
}

/// PetalSonic's native HRTF, Ambisonics, and geometry-acoustics renderer.
pub struct SpatialProcessor {
    // Native HRTF/Ambisonics renderer and delay state
    native_hrtf_renderer: NativeHrtfRenderer,
    native_hrtf_source_states: HashMap<SourceId, NativeHrtfSourceState>,
    native_ambisonics_encoder: NativeAmbisonicsEncoder,
    native_ambisonics_decoder: Option<NativeAmbisonicsBinauralDecoder>,
    native_ambisonics_state: Option<NativeAmbisonicsBinauralState>,
    native_direct_source_states: HashMap<SourceId, NativeDirectSourceState>,
    acoustic_response: Option<Arc<AcousticResponse>>,

    // Configuration
    frame_size: usize,
    distance_scaler: f32,
    use_ambisonics: bool,
    environmental_acoustics_enabled: Arc<AtomicBool>,
    environmental_acoustics_active: bool,
    direct_low_coefficient: f32,
    direct_low_mid_coefficient: f32,
    direct_gain_smoothing_coefficient: f32,
    late_reverb: ThreeBandFdn,
    /// HRTF gain as linear multiplier.
    hrtf_gain_linear: f32,

    // Cached buffers to avoid allocations
    cached_input_buf: Vec<f32>,          // Input mono samples
    cached_direct_buf: Vec<f32>,         // After DirectEffect
    cached_summed_encoded_buf: Vec<f32>, // Accumulated native Ambisonics field
    cached_binaural_processed: Vec<f32>, // Final binaural output (interleaved stereo)
    cached_late_reverb_input: Vec<f32>,  // Shared listener-centric mono send

    // Listener state
    listener_position: Vec3,
    listener_up: Vec3,
    listener_front: Vec3,
    listener_right: Vec3,
}

/// Detailed timing metrics captured for a single spatial processing pass.
#[derive(Debug, Default, Clone, Copy)]
pub struct SpatialProcessingMetrics {
    /// Number of spatial sources considered by this processing pass.
    pub spatial_source_count: usize,
    /// Time spent in the acoustic query/simulation stage, when configured.
    pub physics_simulation_time_us: u64,
    /// Time spent applying direct-path processing.
    pub direct_processing_time_us: u64,
    /// Time spent encoding all spatial sources into the ambisonics field.
    pub ambisonics_encoding_time_us: u64,
    /// Time spent decoding ambisonics data back to listener channels.
    pub ambisonics_decoding_time_us: u64,
    /// Time spent rendering HRTF/binaural output.
    pub hrtf_rendering_time_us: u64,
    /// Time spent rendering the shared three-band FDN late-reverb bus.
    pub late_reverb_time_us: u64,
    /// Time spent selecting native HRTF directions.
    pub native_hrtf_direction_lookup_time_us: u64,
    /// Time spent in native HRTF FIR convolution.
    pub native_hrtf_convolution_time_us: u64,
}

#[derive(Debug, Default, Clone, Copy)]
struct SourceProcessingMetrics {
    direct_processing_time_us: u64,
    ambisonics_encoding_time_us: u64,
    hrtf_rendering_time_us: u64,
    native_hrtf_direction_lookup_time_us: u64,
    native_hrtf_convolution_time_us: u64,
}

impl SourceProcessingMetrics {
    fn add_native_hrtf_metrics(&mut self, metrics: NativeHrtfRenderMetrics) {
        self.native_hrtf_direction_lookup_time_us += metrics.direction_lookup_time_us;
        self.native_hrtf_convolution_time_us += metrics.convolution_time_us;
    }
}

impl SpatialProcessor {
    /// Create a new spatial processor from the fixed world backend plan.
    pub fn new(config: SpatialProcessorConfig) -> Result<Self> {
        let SpatialProcessorConfig {
            sample_rate,
            frame_size,
            max_voices,
            distance_scaler,
            native_hrtf_path,
            hrtf_gain,
            use_ambisonics,
            environmental_acoustics_enabled,
        } = config;
        let table = load_native_hrtf_table(sample_rate, native_hrtf_path.as_deref())?;
        let native_hrtf_renderer = NativeHrtfRenderer::with_frame_size(table.clone(), frame_size)?;
        let mut native_ambisonics_decoder = None;
        let mut native_ambisonics_state = None;
        if use_ambisonics {
            let decoder = NativeAmbisonicsBinauralDecoder::with_frame_size(
                table.clone(),
                DEFAULT_NATIVE_AMBISONICS_ORDER,
                frame_size,
            )?;
            native_ambisonics_state = Some(decoder.create_state());
            native_ambisonics_decoder = Some(decoder);
        }

        let native_ambisonics_encoder =
            NativeAmbisonicsEncoder::new(DEFAULT_NATIVE_AMBISONICS_ORDER)?;

        // Pre-allocate buffers
        let cached_input_buf = vec![0.0; frame_size];
        let cached_direct_buf = vec![0.0; frame_size];
        let ambisonics_channel_count =
            native_ambisonics_channel_count(DEFAULT_NATIVE_AMBISONICS_ORDER)?;
        let cached_summed_encoded_buf = vec![0.0; frame_size * ambisonics_channel_count];
        let cached_binaural_processed = vec![0.0; frame_size * 2];
        let cached_late_reverb_input = vec![0.0; frame_size];
        let mut late_reverb = ThreeBandFdn::new(sample_rate);
        late_reverb.set_parameters(LateReverbParameters::SILENT);

        // Pre-compute HRTF gain in linear space for efficient application.
        let hrtf_gain_linear = gain::db_to_linear(hrtf_gain);

        log::info!(
            "PetalSonic spatial processor: hrtf_backend=Native, acoustics_backend=NativeAsync, use_ambisonics={}, ambisonics_backend=Native, environmental_acoustics_enabled={}",
            use_ambisonics,
            environmental_acoustics_enabled.load(Ordering::Acquire)
        );

        let environmental_acoustics_active =
            environmental_acoustics_enabled.load(Ordering::Acquire);
        Ok(Self {
            native_hrtf_renderer,
            native_hrtf_source_states: HashMap::with_capacity(max_voices),
            native_ambisonics_encoder,
            native_ambisonics_decoder,
            native_ambisonics_state,
            native_direct_source_states: HashMap::with_capacity(max_voices),
            acoustic_response: None,
            frame_size,
            distance_scaler,
            use_ambisonics,
            environmental_acoustics_enabled,
            environmental_acoustics_active,
            direct_low_coefficient: one_pole_coefficient(DIRECT_LOW_CROSSOVER_HZ, sample_rate),
            direct_low_mid_coefficient: one_pole_coefficient(DIRECT_HIGH_CROSSOVER_HZ, sample_rate),
            direct_gain_smoothing_coefficient: one_pole_coefficient(
                1.0 / (std::f32::consts::TAU * DIRECT_GAIN_SMOOTHING_SECONDS),
                sample_rate,
            ),
            late_reverb,
            hrtf_gain_linear,
            cached_input_buf,
            cached_direct_buf,
            cached_summed_encoded_buf,
            cached_binaural_processed,
            cached_late_reverb_input,
            listener_position: Vec3::ZERO,
            listener_up: Vec3::new(0.0, 1.0, 0.0),
            listener_front: Vec3::new(0.0, 0.0, -1.0),
            listener_right: Vec3::new(1.0, 0.0, 0.0),
        })
    }

    /// Update listener pose
    pub fn set_listener_pose(&mut self, pose: Pose) -> Result<()> {
        // Extract position and orientation from pose
        self.listener_position = pose.position;

        // Use the helper methods from Pose
        self.listener_front = pose.forward();
        self.listener_up = pose.up();
        self.listener_right = pose.right();

        Ok(())
    }

    pub(crate) fn replace_acoustic_response(
        &mut self,
        response: Arc<AcousticResponse>,
    ) -> Option<Arc<AcousticResponse>> {
        self.late_reverb.set_parameters(response.late_reverb);
        self.acoustic_response.replace(response)
    }

    pub(crate) fn retire_source(&mut self, source_id: SourceId) -> Option<RetiredSpatialSource> {
        let native_hrtf = self.native_hrtf_source_states.remove(&source_id);
        let native_direct = self.native_direct_source_states.remove(&source_id);
        (native_hrtf.is_some() || native_direct.is_some()).then_some(RetiredSpatialSource {
            _native_hrtf: native_hrtf,
            _native_direct: native_direct,
        })
    }

    fn ensure_native_hrtf_state_for_source(&mut self, source_id: SourceId) -> Result<()> {
        if self.use_ambisonics || self.native_hrtf_source_states.contains_key(&source_id) {
            return Ok(());
        }

        self.native_hrtf_source_states
            .insert(source_id, self.native_hrtf_renderer.create_source_state());
        Ok(())
    }

    fn ensure_native_direct_state_for_source(&mut self, source_id: SourceId) {
        self.native_direct_source_states
            .entry(source_id)
            .or_insert_with(NativeDirectSourceState::new);
    }

    /// Process all spatial sources and return bounded timing metrics.
    pub fn process_spatial_sources_with_metrics(
        &mut self,
        spatial_ids: &[SourceId],
        instances: &mut HashMap<SourceId, PlaybackInstance>,
        output_buffer: &mut [f32],
    ) -> Result<SpatialProcessingMetrics> {
        self.capture_environmental_acoustics_state();

        let mut metrics = SpatialProcessingMetrics {
            spatial_source_count: spatial_ids.len(),
            ..SpatialProcessingMetrics::default()
        };

        // Ensure all spatial sources have backend state created before processing.
        // This guarantees newly played spatial sources participate in the very first
        // block, avoiding a "first block louder" case where distance attenuation /
        // air absorption would still be at their default values.
        for source_id in spatial_ids {
            let Some(instance) = instances.get(source_id) else {
                continue;
            };
            if !matches!(instance.config, SourceConfig::Spatial { .. }) {
                continue;
            }

            self.ensure_native_hrtf_state_for_source(*source_id)?;
            self.ensure_native_direct_state_for_source(*source_id);
        }

        // Clear accumulation buffers
        self.cached_summed_encoded_buf.fill(0.0);
        self.cached_binaural_processed.fill(0.0);
        self.cached_late_reverb_input.fill(0.0);

        // Process each spatial source and accumulate detailed timing.
        for source_id in spatial_ids {
            let Some(instance) = instances.get_mut(source_id) else {
                continue;
            };
            let source_metrics = self.process_single_source(*source_id, instance)?;
            metrics.direct_processing_time_us += source_metrics.direct_processing_time_us;
            metrics.ambisonics_encoding_time_us += source_metrics.ambisonics_encoding_time_us;
            metrics.hrtf_rendering_time_us += source_metrics.hrtf_rendering_time_us;
            metrics.native_hrtf_direction_lookup_time_us +=
                source_metrics.native_hrtf_direction_lookup_time_us;
            metrics.native_hrtf_convolution_time_us +=
                source_metrics.native_hrtf_convolution_time_us;
        }

        if self.use_ambisonics {
            let decoding_start = Instant::now();
            let native_metrics = self.apply_native_ambisonics_decode_effect()?;
            metrics.native_hrtf_direction_lookup_time_us += native_metrics.direction_lookup_time_us;
            metrics.native_hrtf_convolution_time_us += native_metrics.convolution_time_us;
            let decode_elapsed = decoding_start.elapsed().as_micros() as u64;
            metrics.ambisonics_decoding_time_us = decode_elapsed;
            metrics.hrtf_rendering_time_us += decode_elapsed;

            if self.hrtf_gain_linear != 1.0 {
                self.apply_hrtf_gain();
            }
        } else if self.hrtf_gain_linear != 1.0 {
            self.apply_hrtf_gain();
        }

        let late_reverb_start = Instant::now();
        self.late_reverb.process_block(
            &self.cached_late_reverb_input,
            &mut self.cached_binaural_processed,
            self.environmental_acoustics_active,
        );
        metrics.late_reverb_time_us = late_reverb_start.elapsed().as_micros() as u64;

        // Add to output buffer (don't overwrite - allow mixing with non-spatial sources)
        let frames_to_copy = (output_buffer.len() / 2).min(self.frame_size);
        for i in 0..frames_to_copy {
            output_buffer[i * 2] += self.cached_binaural_processed[i * 2];
            output_buffer[i * 2 + 1] += self.cached_binaural_processed[i * 2 + 1];
        }

        Ok(metrics)
    }

    pub(crate) fn has_late_reverb_tail(&self) -> bool {
        self.late_reverb.needs_processing()
    }

    /// Process a single spatial source
    fn process_single_source(
        &mut self,
        source_id: SourceId,
        instance: &mut PlaybackInstance,
    ) -> Result<SourceProcessingMetrics> {
        // Get spatial configuration (position + per-source volume)
        let position = match &instance.config {
            SourceConfig::Spatial { pose, .. } => pose.position,
            _ => return Ok(SourceProcessingMetrics::default()), // Not a spatial source, skip
        };

        // Convert dB volume from config to linear gain once per block.
        let volume = instance.config.volume();

        // Fill input buffer with audio samples
        self.fill_input_buffer(instance, volume);

        let mut metrics = SourceProcessingMetrics::default();

        // Apply direct effect (distance attenuation + air absorption)
        let direct_start = Instant::now();
        self.apply_native_direct_effect(source_id, instance.emitter, position)?;
        for (send, direct) in self
            .cached_late_reverb_input
            .iter_mut()
            .zip(self.cached_direct_buf.iter())
        {
            *send += *direct;
        }
        metrics.direct_processing_time_us = direct_start.elapsed().as_micros() as u64;

        if self.use_ambisonics {
            let encoding_start = Instant::now();
            self.apply_native_ambisonics_encode_effect(position)?;
            metrics.ambisonics_encoding_time_us = encoding_start.elapsed().as_micros() as u64;
        } else {
            let render_start = Instant::now();
            let native_metrics = self.apply_native_hrtf_effect(source_id, position)?;
            metrics.add_native_hrtf_metrics(native_metrics);
            metrics.hrtf_rendering_time_us = render_start.elapsed().as_micros() as u64;
        }

        Ok(metrics)
    }

    /// Fill input buffer from playback instance
    fn fill_input_buffer(&mut self, instance: &mut PlaybackInstance, volume: f32) {
        self.cached_input_buf.fill(0.0);
        instance.fill_mono_buffer(&mut self.cached_input_buf[..self.frame_size], volume);
    }

    /// Apply PetalSonic's native direct path to the input buffer.
    fn apply_native_direct_effect(
        &mut self,
        source_id: SourceId,
        emitter: crate::domain::Emitter,
        source_position: Vec3,
    ) -> Result<()> {
        let source_delta = source_position - self.listener_position;
        let distance_world = source_delta.length();
        let distance_meters = distance_world * self.distance_scaler;
        let distance_attenuation = native_distance_attenuation(distance_meters);
        let air_absorption = native_air_absorption(distance_meters);
        let target_gain = if self.environmental_acoustics_active {
            self.acoustic_response
                .as_ref()
                .map(|response| response.direct_gain(emitter))
                .unwrap_or([1.0; 3])
        } else {
            [1.0; 3]
        };
        let distance_gain = distance_attenuation * air_absorption;
        let state = self
            .native_direct_source_states
            .get_mut(&source_id)
            .ok_or_else(|| {
                PetalSonicError::SpatialAudio(format!(
                    "No native direct state found for source {}",
                    source_id
                ))
            })?;

        self.cached_direct_buf.fill(0.0);
        for (output, input) in self
            .cached_direct_buf
            .iter_mut()
            .zip(self.cached_input_buf.iter())
        {
            state.low_state += self.direct_low_coefficient * (*input - state.low_state);
            state.low_mid_state += self.direct_low_mid_coefficient * (*input - state.low_mid_state);
            let bands = [
                state.low_state,
                state.low_mid_state - state.low_state,
                *input - state.low_mid_state,
            ];
            for (current, target) in state.current_gain.iter_mut().zip(target_gain) {
                *current += self.direct_gain_smoothing_coefficient * (target - *current);
            }
            *output = (bands[0] * state.current_gain[0]
                + bands[1] * state.current_gain[1]
                + bands[2] * state.current_gain[2])
                * distance_gain;
        }
        Ok(())
    }

    fn apply_native_hrtf_effect(
        &mut self,
        source_id: SourceId,
        source_position: Vec3,
    ) -> Result<NativeHrtfRenderMetrics> {
        let direction = self.get_target_direction(source_position);
        let state = self
            .native_hrtf_source_states
            .get_mut(&source_id)
            .ok_or_else(|| {
                PetalSonicError::SpatialAudio(format!(
                    "No native HRTF state found for source {}",
                    source_id
                ))
            })?;

        self.native_hrtf_renderer.render_source_with_metrics(
            state,
            direction,
            &self.cached_direct_buf,
            &mut self.cached_binaural_processed,
        )
    }

    fn apply_native_ambisonics_encode_effect(&mut self, source_position: Vec3) -> Result<()> {
        let direction = self.get_target_direction(source_position);
        self.native_ambisonics_encoder.encode_source_accumulate(
            direction,
            &self.cached_direct_buf,
            &mut self.cached_summed_encoded_buf,
        )
    }

    fn apply_native_ambisonics_decode_effect(&mut self) -> Result<NativeHrtfRenderMetrics> {
        let decoder = self.native_ambisonics_decoder.as_ref().ok_or_else(|| {
            PetalSonicError::SpatialAudio(
                "native Ambisonics decoder is not initialized".to_string(),
            )
        })?;
        let state = self.native_ambisonics_state.as_mut().ok_or_else(|| {
            PetalSonicError::SpatialAudio(
                "native Ambisonics decoder state is not initialized".to_string(),
            )
        })?;

        decoder.decode(
            state,
            &self.cached_summed_encoded_buf,
            &mut self.cached_binaural_processed,
        )
    }

    fn apply_hrtf_gain(&mut self) {
        for sample in self.cached_binaural_processed.iter_mut() {
            *sample *= self.hrtf_gain_linear;
        }
    }

    /// Calculate direction from listener to source in listener's coordinate system.
    ///
    /// PetalSonic's listener-local convention is x=right, y=up, z=front.
    fn get_target_direction(&self, source_position: Vec3) -> Vec3 {
        let delta = source_position - self.listener_position;
        if !delta.is_finite() || delta.length_squared() <= f32::EPSILON {
            return Vec3::Z;
        }

        let target_direction = delta.normalize();
        Vec3::new(
            target_direction.dot(self.listener_right),
            target_direction.dot(self.listener_up),
            target_direction.dot(self.listener_front),
        )
    }

    fn capture_environmental_acoustics_state(&mut self) {
        self.environmental_acoustics_active =
            self.environmental_acoustics_enabled.load(Ordering::Acquire);
    }
}

fn load_native_hrtf_table(
    sample_rate: u32,
    native_hrtf_path: Option<&str>,
) -> Result<Arc<NativeHrtfTable>> {
    let table = if let Some(path) = native_hrtf_path {
        NativeHrtfTable::from_petalhrtf_file(path)?
    } else {
        NativeHrtfTable::from_petalhrtf_bytes(DEFAULT_NATIVE_HRTF_BYTES)?
    };
    if table.sample_rate() != sample_rate {
        return Err(PetalSonicError::Configuration(format!(
            "native HRTF sample rate {} does not match world sample rate {}",
            table.sample_rate(),
            sample_rate
        )));
    }

    Ok(Arc::new(table))
}

fn one_pole_coefficient(cutoff_hz: f32, sample_rate: u32) -> f32 {
    1.0 - (-std::f32::consts::TAU * cutoff_hz / sample_rate.max(1) as f32).exp()
}

fn native_distance_attenuation(distance_meters: f32) -> f32 {
    if !distance_meters.is_finite() {
        return 0.0;
    }

    1.0 / distance_meters.max(1.0)
}

fn native_air_absorption(distance_meters: f32) -> f32 {
    if !distance_meters.is_finite() {
        return 0.0;
    }

    // Conservative broadband approximation used until native spectral filtering lands.
    // Roughly -1.7 dB over 100 m, enough to remove harshness without large loudness shifts.
    (-0.0002 * distance_meters.max(0.0)).exp().clamp(0.2, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acoustic_propagation::DirectAcousticResponse;
    use crate::domain::Emitter;

    #[test]
    fn native_distance_attenuation_is_clamped_near_listener() {
        assert_eq!(native_distance_attenuation(0.0), 1.0);
        assert_eq!(native_distance_attenuation(0.5), 1.0);
        assert_eq!(native_distance_attenuation(1.0), 1.0);
        assert!((native_distance_attenuation(4.0) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn native_air_absorption_stays_bounded() {
        assert_eq!(native_air_absorption(f32::NAN), 0.0);
        assert_eq!(native_air_absorption(-10.0), 1.0);
        assert!((0.2..=1.0).contains(&native_air_absorption(10_000.0)));
    }

    #[test]
    fn environmental_acoustics_toggle_bypasses_geometry_but_keeps_native_direct_path() {
        let enabled = Arc::new(AtomicBool::new(true));
        let emitter = Emitter {
            world_id: 1,
            index: 0,
            generation: 1,
        };
        let source_id = SourceId::from(1);
        let mut processor = SpatialProcessor::new(SpatialProcessorConfig {
            sample_rate: 48_000,
            frame_size: 8,
            max_voices: 1,
            distance_scaler: 1.0,
            native_hrtf_path: None,
            hrtf_gain: 0.0,
            use_ambisonics: false,
            environmental_acoustics_enabled: enabled.clone(),
        })
        .unwrap();
        processor.ensure_native_direct_state_for_source(source_id);
        processor.replace_acoustic_response(Arc::new(AcousticResponse {
            spatial_revision: 1,
            geometry_version: 1,
            direct: vec![DirectAcousticResponse {
                emitter,
                gain: [0.0; 3],
            }],
            late_reverb: LateReverbParameters::SILENT,
            published_at: Instant::now(),
            solve_time_us: 1,
        }));

        for _ in 0..1_200 {
            processor.cached_input_buf.fill(1.0);
            processor
                .apply_native_direct_effect(source_id, emitter, Vec3::Z)
                .unwrap();
        }
        assert!(
            processor
                .cached_direct_buf
                .iter()
                .all(|sample| *sample < 0.05)
        );

        enabled.store(false, Ordering::Release);
        processor.capture_environmental_acoustics_state();
        for _ in 0..1_200 {
            processor.cached_input_buf.fill(1.0);
            processor
                .apply_native_direct_effect(source_id, emitter, Vec3::Z)
                .unwrap();
        }
        assert!(
            processor
                .cached_direct_buf
                .iter()
                .all(|sample| *sample > 0.95)
        );
    }
}
