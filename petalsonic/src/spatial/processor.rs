use super::late_reverb::{LateReverbParameters, ThreeBandFdn};
use super::native_ambisonics::{
    DEFAULT_NATIVE_AMBISONICS_ORDER, NativeAmbisonicsBinauralDecoder,
    NativeAmbisonicsBinauralState, NativeAmbisonicsEncoder, native_ambisonics_channel_count,
};
use super::native_hrtf::{
    NativeHrtfRenderMetrics, NativeHrtfRenderer, NativeHrtfSourceState, NativeHrtfTable,
};
use crate::acoustic_propagation::{AcousticResponse, MAX_EARLY_REFLECTION_TAPS};
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
const EARLY_REFLECTION_MAX_DELAY_SECONDS: f32 = 0.25;
const EARLY_REFLECTION_SMOOTHING_SECONDS: f32 = 0.05;
const EARLY_REFLECTION_SLOT_RELEASE_GAIN: f32 = 1.0e-3;
// Keep one active set plus one fading set so a priority change can crossfade without allocating.
const EARLY_REFLECTION_SOURCE_STATE_CAPACITY: usize = 16;

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

#[derive(Debug, Clone)]
struct NativeEarlyReflectionTapState {
    path_id: Option<u16>,
    arrival_direction: Vec3,
    current_delay_samples: f32,
    current_gain: [f32; 3],
    low_state: f32,
    low_mid_state: f32,
    hrtf_state: Option<NativeHrtfSourceState>,
}

impl NativeEarlyReflectionTapState {
    fn new(hrtf_state: Option<NativeHrtfSourceState>) -> Self {
        Self {
            path_id: None,
            arrival_direction: Vec3::Z,
            current_delay_samples: 1.0,
            current_gain: [0.0; 3],
            low_state: 0.0,
            low_mid_state: 0.0,
            hrtf_state,
        }
    }

    fn is_released(&self) -> bool {
        self.current_gain
            .iter()
            .all(|gain| gain.abs() <= EARLY_REFLECTION_SLOT_RELEASE_GAIN)
    }
}

#[derive(Debug, Clone)]
struct NativeEarlyReflectionSourceState {
    emitter: Option<crate::domain::Emitter>,
    silenced: bool,
    delay_line: Vec<f32>,
    write_index: usize,
    taps: Vec<NativeEarlyReflectionTapState>,
}

impl NativeEarlyReflectionSourceState {
    fn new(
        emitter: Option<crate::domain::Emitter>,
        sample_rate: u32,
        renderer: &NativeHrtfRenderer,
        use_ambisonics: bool,
    ) -> Self {
        let delay_samples =
            (sample_rate as f32 * EARLY_REFLECTION_MAX_DELAY_SECONDS).ceil() as usize + 2;
        let taps = (0..MAX_EARLY_REFLECTION_TAPS)
            .map(|_| {
                NativeEarlyReflectionTapState::new(
                    (!use_ambisonics).then(|| renderer.create_source_state()),
                )
            })
            .collect();
        Self {
            emitter,
            silenced: true,
            delay_line: vec![0.0; delay_samples],
            write_index: 0,
            taps,
        }
    }

    fn reset_for_emitter(&mut self, emitter: Option<crate::domain::Emitter>) {
        self.emitter = emitter;
        self.silenced = true;
        self.delay_line.fill(0.0);
        self.write_index = 0;
        for tap in &mut self.taps {
            tap.path_id = None;
            tap.arrival_direction = Vec3::Z;
            tap.current_delay_samples = 1.0;
            tap.current_gain = [0.0; 3];
            tap.low_state = 0.0;
            tap.low_mid_state = 0.0;
            if let Some(hrtf_state) = &mut tap.hrtf_state {
                hrtf_state.reset();
            }
        }
    }

    fn is_released(&self) -> bool {
        self.taps.iter().all(|tap| tap.is_released())
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
    native_early_reflection_source_states: HashMap<SourceId, NativeEarlyReflectionSourceState>,
    free_native_early_reflection_source_states: Vec<NativeEarlyReflectionSourceState>,
    acoustic_response: Option<Arc<AcousticResponse>>,

    // Configuration
    frame_size: usize,
    sample_rate: u32,
    distance_scaler: f32,
    use_ambisonics: bool,
    environmental_acoustics_enabled: Arc<AtomicBool>,
    environmental_acoustics_active: bool,
    direct_low_coefficient: f32,
    direct_low_mid_coefficient: f32,
    direct_gain_smoothing_coefficient: f32,
    early_reflection_smoothing_coefficient: f32,
    late_reverb: ThreeBandFdn,
    /// HRTF gain as linear multiplier.
    hrtf_gain_linear: f32,

    // Cached buffers to avoid allocations
    cached_input_buf: Vec<f32>,             // Input mono samples
    cached_direct_buf: Vec<f32>,            // After DirectEffect
    cached_summed_encoded_buf: Vec<f32>,    // Accumulated native Ambisonics field
    cached_binaural_processed: Vec<f32>,    // Final binaural output (interleaved stereo)
    cached_early_reflection_bufs: Vec<f32>, // One mono block per bounded reflection tap
    cached_late_reverb_input: Vec<f32>,     // Shared listener-centric mono send

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
    /// Time spent rendering bounded early-reflection taps.
    pub early_reflection_time_us: u64,
    /// Time spent selecting native HRTF directions.
    pub native_hrtf_direction_lookup_time_us: u64,
    /// Time spent in native HRTF FIR convolution.
    pub native_hrtf_convolution_time_us: u64,
}

#[derive(Debug, Default, Clone, Copy)]
struct SourceProcessingMetrics {
    direct_processing_time_us: u64,
    early_reflection_time_us: u64,
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
        let cached_early_reflection_bufs = vec![0.0; frame_size * MAX_EARLY_REFLECTION_TAPS];
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
        let early_reflection_pool_size = EARLY_REFLECTION_SOURCE_STATE_CAPACITY.min(max_voices);
        let free_native_early_reflection_source_states = (0..early_reflection_pool_size)
            .map(|_| {
                NativeEarlyReflectionSourceState::new(
                    None,
                    sample_rate,
                    &native_hrtf_renderer,
                    use_ambisonics,
                )
            })
            .collect();
        Ok(Self {
            native_hrtf_renderer,
            native_hrtf_source_states: HashMap::with_capacity(max_voices),
            native_ambisonics_encoder,
            native_ambisonics_decoder,
            native_ambisonics_state,
            native_direct_source_states: HashMap::with_capacity(max_voices),
            native_early_reflection_source_states: HashMap::with_capacity(
                early_reflection_pool_size,
            ),
            free_native_early_reflection_source_states,
            acoustic_response: None,
            frame_size,
            sample_rate,
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
            early_reflection_smoothing_coefficient: one_pole_coefficient(
                1.0 / (std::f32::consts::TAU * EARLY_REFLECTION_SMOOTHING_SECONDS),
                sample_rate,
            ),
            late_reverb,
            hrtf_gain_linear,
            cached_input_buf,
            cached_direct_buf,
            cached_summed_encoded_buf,
            cached_binaural_processed,
            cached_early_reflection_bufs,
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
        if let Some(mut state) = self
            .native_early_reflection_source_states
            .remove(&source_id)
        {
            state.reset_for_emitter(None);
            self.free_native_early_reflection_source_states.push(state);
        }
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

    fn ensure_native_early_reflection_state_for_source(
        &mut self,
        source_id: SourceId,
        emitter: crate::domain::Emitter,
    ) {
        if self
            .native_early_reflection_source_states
            .contains_key(&source_id)
            || !self.environmental_acoustics_active
            || self
                .acoustic_response
                .as_ref()
                .is_none_or(|response| response.early_reflections(emitter).is_empty())
        {
            return;
        }
        if let Some(mut state) = self.free_native_early_reflection_source_states.pop() {
            state.reset_for_emitter(Some(emitter));
            self.native_early_reflection_source_states
                .insert(source_id, state);
            return;
        }

        let reusable_source_id = self.native_early_reflection_source_states.iter().find_map(
            |(candidate_source_id, state)| {
                (state.is_released()
                    && state.emitter.is_none_or(|emitter| {
                        self.acoustic_response
                            .as_ref()
                            .is_none_or(|response| response.early_reflections(emitter).is_empty())
                    }))
                .then_some(*candidate_source_id)
            },
        );
        if let Some(reusable_source_id) = reusable_source_id
            && let Some(mut state) = self
                .native_early_reflection_source_states
                .remove(&reusable_source_id)
        {
            state.reset_for_emitter(Some(emitter));
            self.native_early_reflection_source_states
                .insert(source_id, state);
        }
    }

    pub(crate) fn silence_source_state(&mut self, source_id: SourceId) {
        if let Some(state) = self
            .native_early_reflection_source_states
            .get_mut(&source_id)
            && !state.silenced
        {
            state.reset_for_emitter(state.emitter);
        }
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
            self.ensure_native_early_reflection_state_for_source(*source_id, instance.emitter);
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
            metrics.early_reflection_time_us += source_metrics.early_reflection_time_us;
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

        let reflection_start = Instant::now();
        let native_metrics = self.apply_native_early_reflections(source_id, instance.emitter)?;
        metrics.add_native_hrtf_metrics(native_metrics);
        metrics.early_reflection_time_us = reflection_start.elapsed().as_micros() as u64;
        metrics.hrtf_rendering_time_us += native_metrics.convolution_time_us;

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

    fn apply_native_early_reflections(
        &mut self,
        source_id: SourceId,
        emitter: crate::domain::Emitter,
    ) -> Result<NativeHrtfRenderMetrics> {
        let mut targets = [None; MAX_EARLY_REFLECTION_TAPS];
        if self.environmental_acoustics_active
            && let Some(response) = &self.acoustic_response
        {
            for (target, tap) in targets.iter_mut().zip(response.early_reflections(emitter)) {
                *target = Some(*tap);
            }
        }
        let Some(state) = self
            .native_early_reflection_source_states
            .get_mut(&source_id)
        else {
            return Ok(NativeHrtfRenderMetrics::default());
        };
        state.silenced = false;

        let mut slot_targets = [None; MAX_EARLY_REFLECTION_TAPS];
        let mut target_claimed = [false; MAX_EARLY_REFLECTION_TAPS];
        for (slot_index, slot) in state.taps.iter().enumerate() {
            let Some(path_id) = slot.path_id else {
                continue;
            };
            if let Some((target_index, target)) =
                targets.iter().enumerate().find_map(|(index, target)| {
                    target
                        .filter(|target| target.path_id == path_id && !target_claimed[index])
                        .map(|target| (index, target))
                })
            {
                slot_targets[slot_index] = Some(target);
                target_claimed[target_index] = true;
            }
        }
        for (slot_index, slot) in state.taps.iter_mut().enumerate() {
            if slot_targets[slot_index].is_some() || !slot.is_released() {
                continue;
            }
            slot.path_id = None;
            if let Some((target_index, target)) =
                targets.iter().enumerate().find_map(|(index, target)| {
                    target
                        .filter(|_| !target_claimed[index])
                        .map(|target| (index, target))
                })
            {
                slot.path_id = Some(target.path_id);
                slot.arrival_direction = target.arrival_direction;
                slot.current_delay_samples = reflection_delay_samples(
                    target.delay_seconds,
                    self.sample_rate,
                    state.delay_line.len(),
                );
                slot_targets[slot_index] = Some(target);
                target_claimed[target_index] = true;
            }
        }

        self.cached_early_reflection_bufs.fill(0.0);
        let mut write_index = state.write_index;
        for (sample_index, input) in self.cached_input_buf.iter().copied().enumerate() {
            state.delay_line[write_index] = input;
            for (slot_index, slot) in state.taps.iter_mut().enumerate() {
                let target = slot_targets[slot_index];
                let target_delay = target
                    .map(|target| {
                        reflection_delay_samples(
                            target.delay_seconds,
                            self.sample_rate,
                            state.delay_line.len(),
                        )
                    })
                    .unwrap_or(slot.current_delay_samples);
                slot.current_delay_samples += self.early_reflection_smoothing_coefficient
                    * (target_delay - slot.current_delay_samples);
                let target_gain = target.map(|target| target.gain).unwrap_or([0.0; 3]);
                for (current, target) in slot.current_gain.iter_mut().zip(target_gain) {
                    *current += self.early_reflection_smoothing_coefficient * (target - *current);
                }

                let delayed = read_fractional_delay(
                    &state.delay_line,
                    write_index,
                    slot.current_delay_samples,
                );
                slot.low_state += self.direct_low_coefficient * (delayed - slot.low_state);
                slot.low_mid_state +=
                    self.direct_low_mid_coefficient * (delayed - slot.low_mid_state);
                let bands = [
                    slot.low_state,
                    slot.low_mid_state - slot.low_state,
                    delayed - slot.low_mid_state,
                ];
                self.cached_early_reflection_bufs[slot_index * self.frame_size + sample_index] =
                    bands[0] * slot.current_gain[0]
                        + bands[1] * slot.current_gain[1]
                        + bands[2] * slot.current_gain[2];
                if let Some(target) = target {
                    slot.arrival_direction = target.arrival_direction;
                }
            }
            write_index = (write_index + 1) % state.delay_line.len();
        }
        state.write_index = write_index;
        for (slot, target) in state.taps.iter_mut().zip(slot_targets) {
            if target.is_none() && slot.is_released() {
                slot.path_id = None;
            }
        }

        let listener_right = self.listener_right;
        let listener_up = self.listener_up;
        let listener_front = self.listener_front;
        let mut metrics = NativeHrtfRenderMetrics::default();
        for (slot_index, slot) in state.taps.iter_mut().enumerate() {
            if slot.path_id.is_none() && slot.is_released() {
                continue;
            }
            let direction = listener_local_direction(
                slot.arrival_direction,
                listener_right,
                listener_up,
                listener_front,
            );
            let reflection = &self.cached_early_reflection_bufs
                [slot_index * self.frame_size..(slot_index + 1) * self.frame_size];
            if self.use_ambisonics {
                self.native_ambisonics_encoder.encode_source_accumulate(
                    direction,
                    reflection,
                    &mut self.cached_summed_encoded_buf,
                )?;
            } else if let Some(hrtf_state) = &mut slot.hrtf_state {
                let tap_metrics = self.native_hrtf_renderer.render_source_with_metrics(
                    hrtf_state,
                    direction,
                    reflection,
                    &mut self.cached_binaural_processed,
                )?;
                metrics.direction_lookup_time_us += tap_metrics.direction_lookup_time_us;
                metrics.convolution_time_us += tap_metrics.convolution_time_us;
            }
        }
        Ok(metrics)
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

fn reflection_delay_samples(delay_seconds: f32, sample_rate: u32, delay_line_len: usize) -> f32 {
    (delay_seconds * sample_rate as f32).clamp(1.0, delay_line_len.saturating_sub(2).max(1) as f32)
}

fn read_fractional_delay(delay_line: &[f32], write_index: usize, delay_samples: f32) -> f32 {
    let read_position =
        (write_index as f32 - delay_samples).rem_euclid(delay_line.len().max(1) as f32);
    let first = read_position.floor() as usize % delay_line.len();
    let second = (first + 1) % delay_line.len();
    let fraction = read_position - read_position.floor();
    delay_line[first] * (1.0 - fraction) + delay_line[second] * fraction
}

fn listener_local_direction(
    world_direction: Vec3,
    listener_right: Vec3,
    listener_up: Vec3,
    listener_front: Vec3,
) -> Vec3 {
    let direction =
        if world_direction.is_finite() && world_direction.length_squared() > f32::EPSILON {
            world_direction.normalize()
        } else {
            Vec3::Z
        };
    Vec3::new(
        direction.dot(listener_right),
        direction.dot(listener_up),
        direction.dot(listener_front),
    )
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
    use crate::acoustic_propagation::{DirectAcousticResponse, EarlyReflectionTap};
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
                early_reflections: Vec::new(),
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

    #[test]
    fn early_reflection_delay_and_runtime_fade_remain_finite() {
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
        processor.replace_acoustic_response(Arc::new(AcousticResponse {
            spatial_revision: 1,
            geometry_version: 1,
            direct: vec![DirectAcousticResponse {
                emitter,
                gain: [1.0; 3],
                early_reflections: vec![EarlyReflectionTap {
                    path_id: 7,
                    arrival_direction: Vec3::Z,
                    delay_seconds: 4.0 / 48_000.0,
                    gain: [0.5, 0.25, 0.1],
                }],
            }],
            late_reverb: LateReverbParameters::SILENT,
            published_at: Instant::now(),
            solve_time_us: 1,
        }));
        processor.ensure_native_early_reflection_state_for_source(source_id, emitter);

        let mut reflected_energy = 0.0;
        for block in 0..8 {
            processor.cached_input_buf.fill(0.0);
            if block == 0 {
                processor.cached_input_buf[0] = 1.0;
            }
            processor.cached_binaural_processed.fill(0.0);
            processor
                .apply_native_early_reflections(source_id, emitter)
                .unwrap();
            reflected_energy += processor
                .cached_binaural_processed
                .iter()
                .map(|sample| sample * sample)
                .sum::<f32>();
            assert!(
                processor
                    .cached_binaural_processed
                    .iter()
                    .all(|sample| sample.is_finite())
            );
        }
        assert!(reflected_energy > 0.0);

        enabled.store(false, Ordering::Release);
        processor.capture_environmental_acoustics_state();
        for _ in 0..1_200 {
            processor.cached_input_buf.fill(0.0);
            processor.cached_binaural_processed.fill(0.0);
            processor
                .apply_native_early_reflections(source_id, emitter)
                .unwrap();
        }
        let state = &processor.native_early_reflection_source_states[&source_id];
        assert!(state.taps.iter().all(|tap| tap.path_id.is_none()));
    }

    #[test]
    fn early_reflection_source_state_pool_stays_bounded_across_priority_changes() {
        let enabled = Arc::new(AtomicBool::new(true));
        let mut processor = SpatialProcessor::new(SpatialProcessorConfig {
            sample_rate: 48_000,
            frame_size: 8,
            max_voices: 64,
            distance_scaler: 1.0,
            native_hrtf_path: None,
            hrtf_gain: 0.0,
            use_ambisonics: true,
            environmental_acoustics_enabled: enabled,
        })
        .unwrap();

        for generation in 0..3 {
            let first_index = generation * EARLY_REFLECTION_SOURCE_STATE_CAPACITY;
            let direct = (first_index..first_index + EARLY_REFLECTION_SOURCE_STATE_CAPACITY)
                .map(|index| DirectAcousticResponse {
                    emitter: Emitter {
                        world_id: 1,
                        index: index as u32,
                        generation: 1,
                    },
                    gain: [1.0; 3],
                    early_reflections: vec![EarlyReflectionTap {
                        path_id: index as u16,
                        arrival_direction: Vec3::Z,
                        delay_seconds: 0.01,
                        gain: [0.1; 3],
                    }],
                })
                .collect();
            processor.replace_acoustic_response(Arc::new(AcousticResponse {
                spatial_revision: generation as u64 + 1,
                geometry_version: 1,
                direct,
                late_reverb: LateReverbParameters::SILENT,
                published_at: Instant::now(),
                solve_time_us: 1,
            }));
            for index in first_index..first_index + EARLY_REFLECTION_SOURCE_STATE_CAPACITY {
                processor.ensure_native_early_reflection_state_for_source(
                    SourceId::from(index as u64),
                    Emitter {
                        world_id: 1,
                        index: index as u32,
                        generation: 1,
                    },
                );
            }
            assert!(
                processor.native_early_reflection_source_states.len()
                    <= EARLY_REFLECTION_SOURCE_STATE_CAPACITY
            );
        }
    }

    #[test]
    #[ignore = "release-mode performance probe"]
    fn active_early_reflections_release_budget() {
        use std::hint::black_box;

        const SAMPLE_RATE: u32 = 48_000;
        const FRAMES: usize = 1_024;
        const BLOCKS: usize = 1_000;
        let enabled = Arc::new(AtomicBool::new(true));
        let mut processor = SpatialProcessor::new(SpatialProcessorConfig {
            sample_rate: SAMPLE_RATE,
            frame_size: FRAMES,
            max_voices: 8,
            distance_scaler: 1.0,
            native_hrtf_path: None,
            hrtf_gain: 0.0,
            use_ambisonics: false,
            environmental_acoustics_enabled: enabled,
        })
        .unwrap();
        let sources: Vec<_> = (0..8)
            .map(|index| {
                (
                    SourceId::from(index as u64),
                    Emitter {
                        world_id: 1,
                        index,
                        generation: 1,
                    },
                )
            })
            .collect();
        let direct = sources
            .iter()
            .map(|(_, emitter)| DirectAcousticResponse {
                emitter: *emitter,
                gain: [1.0; 3],
                early_reflections: (0..MAX_EARLY_REFLECTION_TAPS)
                    .map(|path_id| EarlyReflectionTap {
                        path_id: path_id as u16,
                        arrival_direction: if path_id == 0 { Vec3::X } else { Vec3::Z },
                        delay_seconds: 0.01 + path_id as f32 * 0.005,
                        gain: [0.15, 0.1, 0.05],
                    })
                    .collect(),
            })
            .collect();
        processor.replace_acoustic_response(Arc::new(AcousticResponse {
            spatial_revision: 1,
            geometry_version: 1,
            direct,
            late_reverb: LateReverbParameters::SILENT,
            published_at: Instant::now(),
            solve_time_us: 1,
        }));
        for (source_id, emitter) in &sources {
            processor.ensure_native_early_reflection_state_for_source(*source_id, *emitter);
        }
        processor.cached_input_buf.fill(0.1);
        for _ in 0..32 {
            processor.cached_binaural_processed.fill(0.0);
            for (source_id, emitter) in &sources {
                processor
                    .apply_native_early_reflections(*source_id, *emitter)
                    .unwrap();
            }
        }

        let started = Instant::now();
        for _ in 0..BLOCKS {
            processor.cached_binaural_processed.fill(0.0);
            for (source_id, emitter) in black_box(&sources) {
                processor
                    .apply_native_early_reflections(*source_id, *emitter)
                    .unwrap();
            }
        }
        let elapsed = started.elapsed();
        let audio_seconds = FRAMES as f64 * BLOCKS as f64 / SAMPLE_RATE as f64;
        println!(
            "active early reflections: sources={} taps_per_source={} blocks={BLOCKS} frames={FRAMES} elapsed_ms={:.3} us_per_block={:.3} realtime_cpu_percent={:.3}",
            sources.len(),
            MAX_EARLY_REFLECTION_TAPS,
            elapsed.as_secs_f64() * 1_000.0,
            elapsed.as_secs_f64() * 1_000_000.0 / BLOCKS as f64,
            elapsed.as_secs_f64() / audio_seconds * 100.0,
        );
        assert!(
            processor
                .cached_binaural_processed
                .iter()
                .all(|sample| sample.is_finite())
        );
    }
}
