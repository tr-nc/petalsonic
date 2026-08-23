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
use crate::domain::{DirectGeometry, DirectPlacement, EnvironmentOrigin};
use crate::error::{PetalSonicError, Result};
use crate::events::{PetalSonicEvent, VoiceFirstRenderTelemetry};
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

#[derive(Clone, Copy)]
struct ThreeBandCoefficients {
    low: f32,
    low_mid: f32,
    smoothing: f32,
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
    voice_id: Option<SourceId>,
    silenced: bool,
    draining: bool,
    delay_line: Vec<f32>,
    write_index: usize,
    taps: Vec<NativeEarlyReflectionTapState>,
}

impl NativeEarlyReflectionSourceState {
    fn new(
        voice_id: Option<SourceId>,
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
            voice_id,
            silenced: true,
            draining: false,
            delay_line: vec![0.0; delay_samples],
            write_index: 0,
            taps,
        }
    }

    fn reset_for_voice(&mut self, voice_id: Option<SourceId>) {
        self.voice_id = voice_id;
        self.silenced = true;
        self.draining = false;
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
    native_environment_source_states: HashMap<SourceId, NativeDirectSourceState>,
    native_early_reflection_source_states: HashMap<SourceId, NativeEarlyReflectionSourceState>,
    free_native_early_reflection_source_states: Vec<NativeEarlyReflectionSourceState>,
    draining_early_reflection_ids: Vec<SourceId>,
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
    cached_environment_send_buf: Vec<f32>,  // Voice block routed to the shared environment
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

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SpatialRenderContext {
    pub render_block_index: u64,
    pub spatial_revision: u64,
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
        let cached_environment_send_buf = vec![0.0; frame_size];
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
            native_environment_source_states: HashMap::with_capacity(max_voices),
            native_early_reflection_source_states: HashMap::with_capacity(
                early_reflection_pool_size,
            ),
            free_native_early_reflection_source_states,
            draining_early_reflection_ids: Vec::with_capacity(early_reflection_pool_size),
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
            cached_environment_send_buf,
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
        self.native_environment_source_states.remove(&source_id);
        let release_early_state = self
            .native_early_reflection_source_states
            .get(&source_id)
            .is_some_and(NativeEarlyReflectionSourceState::is_released);
        if release_early_state {
            if let Some(mut state) = self
                .native_early_reflection_source_states
                .remove(&source_id)
            {
                state.reset_for_voice(None);
                self.free_native_early_reflection_source_states.push(state);
            }
        } else if let Some(state) = self
            .native_early_reflection_source_states
            .get_mut(&source_id)
        {
            state.draining = true;
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

    fn ensure_native_environment_state_for_source(&mut self, source_id: SourceId) {
        self.native_environment_source_states
            .entry(source_id)
            .or_insert_with(NativeDirectSourceState::new);
    }

    fn ensure_native_early_reflection_state_for_source(&mut self, source_id: SourceId) {
        if self
            .native_early_reflection_source_states
            .contains_key(&source_id)
            || !self.environmental_acoustics_active
            || self
                .acoustic_response
                .as_ref()
                .is_none_or(|response| response.early_reflections(source_id).is_empty())
        {
            return;
        }
        if let Some(mut state) = self.free_native_early_reflection_source_states.pop() {
            state.reset_for_voice(Some(source_id));
            self.native_early_reflection_source_states
                .insert(source_id, state);
            return;
        }

        let reusable_source_id = self.native_early_reflection_source_states.iter().find_map(
            |(candidate_source_id, state)| {
                (state.is_released()
                    && state.voice_id.is_none_or(|voice_id| {
                        self.acoustic_response
                            .as_ref()
                            .is_none_or(|response| response.early_reflections(voice_id).is_empty())
                    }))
                .then_some(*candidate_source_id)
            },
        );
        if let Some(reusable_source_id) = reusable_source_id
            && let Some(mut state) = self
                .native_early_reflection_source_states
                .remove(&reusable_source_id)
        {
            state.reset_for_voice(Some(source_id));
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
            state.reset_for_voice(state.voice_id);
        }
    }

    /// Process all spatial sources and return bounded timing metrics.
    pub fn process_spatial_sources_with_metrics(
        &mut self,
        spatial_ids: &[SourceId],
        instances: &mut HashMap<SourceId, PlaybackInstance>,
        output_buffer: &mut [f32],
        render_context: SpatialRenderContext,
        events: &mut Vec<PetalSonicEvent>,
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

            if !matches!(instance.direct_path.placement(), DirectPlacement::Disabled) {
                self.ensure_native_hrtf_state_for_source(*source_id)?;
                self.ensure_native_direct_state_for_source(*source_id);
            }
            if !matches!(
                instance.environment_send.origin(),
                EnvironmentOrigin::Disabled
            ) {
                self.ensure_native_environment_state_for_source(*source_id);
                self.ensure_native_early_reflection_state_for_source(*source_id);
            }
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
            let source_metrics =
                self.process_single_source(*source_id, instance, render_context, events)?;
            metrics.direct_processing_time_us += source_metrics.direct_processing_time_us;
            metrics.early_reflection_time_us += source_metrics.early_reflection_time_us;
            metrics.ambisonics_encoding_time_us += source_metrics.ambisonics_encoding_time_us;
            metrics.hrtf_rendering_time_us += source_metrics.hrtf_rendering_time_us;
            metrics.native_hrtf_direction_lookup_time_us +=
                source_metrics.native_hrtf_direction_lookup_time_us;
            metrics.native_hrtf_convolution_time_us +=
                source_metrics.native_hrtf_convolution_time_us;
        }

        let drain_started = Instant::now();
        let draining_metrics = self.process_draining_early_reflections()?;
        metrics.early_reflection_time_us += drain_started.elapsed().as_micros() as u64;
        metrics.hrtf_rendering_time_us += draining_metrics.convolution_time_us;
        metrics.native_hrtf_direction_lookup_time_us += draining_metrics.direction_lookup_time_us;
        metrics.native_hrtf_convolution_time_us += draining_metrics.convolution_time_us;

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

    pub(crate) fn has_environment_tail(&self) -> bool {
        self.late_reverb.needs_processing()
            || self
                .native_early_reflection_source_states
                .values()
                .any(|state| state.draining)
    }

    fn process_draining_early_reflections(&mut self) -> Result<NativeHrtfRenderMetrics> {
        self.draining_early_reflection_ids.clear();
        self.draining_early_reflection_ids.extend(
            self.native_early_reflection_source_states
                .iter()
                .filter_map(|(source_id, state)| state.draining.then_some(*source_id)),
        );
        self.cached_input_buf.fill(0.0);
        let mut metrics = NativeHrtfRenderMetrics::default();
        for index in 0..self.draining_early_reflection_ids.len() {
            let source_id = self.draining_early_reflection_ids[index];
            let source_metrics = self.apply_native_early_reflections(source_id, 0.0, false)?;
            metrics.direction_lookup_time_us += source_metrics.direction_lookup_time_us;
            metrics.convolution_time_us += source_metrics.convolution_time_us;
        }
        for index in 0..self.draining_early_reflection_ids.len() {
            let source_id = self.draining_early_reflection_ids[index];
            let released = self
                .native_early_reflection_source_states
                .get(&source_id)
                .is_some_and(NativeEarlyReflectionSourceState::is_released);
            if released
                && let Some(mut state) = self
                    .native_early_reflection_source_states
                    .remove(&source_id)
            {
                state.reset_for_voice(None);
                self.free_native_early_reflection_source_states.push(state);
            }
        }
        Ok(metrics)
    }

    /// Process a single spatial source
    fn process_single_source(
        &mut self,
        source_id: SourceId,
        instance: &mut PlaybackInstance,
        render_context: SpatialRenderContext,
        events: &mut Vec<PetalSonicEvent>,
    ) -> Result<SourceProcessingMetrics> {
        // Get spatial configuration (position + per-source volume)
        let emitter_position = match &instance.config {
            SourceConfig::Spatial { pose, .. } => pose.position,
            _ => return Ok(SourceProcessingMetrics::default()), // Not a spatial source, skip
        };

        // Convert dB volume from config to linear gain once per block.
        let volume = instance.config.volume();

        // Fill input buffer with audio samples
        let frames_filled = self.fill_input_buffer(instance, volume);

        let mut metrics = SourceProcessingMetrics::default();

        // Direct and environment paths derive from this one cursor block. Neither path asks the
        // Voice to decode or advance a second time.
        let direct_start = Instant::now();
        let direct_local_position =
            self.resolve_direct_local_position(instance.direct_path.placement(), emitter_position);
        self.cached_direct_buf.fill(0.0);
        if let Some(direct_local_position) = direct_local_position {
            self.apply_native_direct_effect(
                source_id,
                direct_local_position,
                instance.direct_path.geometry(),
            )?;
        }

        let environment_local_position = match instance.environment_send.origin() {
            EnvironmentOrigin::FollowEmitter => {
                Some(self.world_to_listener_position(emitter_position))
            }
            EnvironmentOrigin::World(origin) => {
                Some(self.world_to_listener_position(origin.position))
            }
            EnvironmentOrigin::Disabled => None,
        };
        let environment_send_gain = gain::db_to_linear(instance.environment_send.gain_db());
        self.cached_environment_send_buf.fill(0.0);
        if let Some(environment_local_position) = environment_local_position {
            let compatible_shared_path =
                matches!(instance.direct_path.placement(), DirectPlacement::World)
                    && matches!(
                        instance.direct_path.geometry(),
                        DirectGeometry::SimulatedTransmission
                    )
                    && matches!(
                        instance.environment_send.origin(),
                        EnvironmentOrigin::FollowEmitter
                    );
            if compatible_shared_path {
                for (send, direct) in self
                    .cached_environment_send_buf
                    .iter_mut()
                    .zip(&self.cached_direct_buf)
                {
                    *send = *direct * environment_send_gain;
                }
            } else {
                self.apply_native_environment_send_effect(
                    source_id,
                    environment_local_position,
                    environment_send_gain,
                )?;
            }
            for (sum, send) in self
                .cached_late_reverb_input
                .iter_mut()
                .zip(&self.cached_environment_send_buf)
            {
                *sum += *send;
            }
        }
        if frames_filled > 0 {
            self.observe_voice_render(
                source_id,
                instance,
                emitter_position,
                direct_local_position,
                render_context,
                events,
            );
        }
        metrics.direct_processing_time_us = direct_start.elapsed().as_micros() as u64;

        if let Some(direct_local_position) = direct_local_position {
            if self.use_ambisonics {
                let encoding_start = Instant::now();
                self.apply_native_ambisonics_encode_effect(direct_local_position)?;
                metrics.ambisonics_encoding_time_us = encoding_start.elapsed().as_micros() as u64;
            } else {
                let render_start = Instant::now();
                let native_metrics =
                    self.apply_native_hrtf_effect(source_id, direct_local_position)?;
                metrics.add_native_hrtf_metrics(native_metrics);
                metrics.hrtf_rendering_time_us = render_start.elapsed().as_micros() as u64;
            }
        }

        let reflection_start = Instant::now();
        let native_metrics = self.apply_native_early_reflections(
            source_id,
            environment_send_gain,
            environment_local_position.is_some(),
        )?;
        metrics.add_native_hrtf_metrics(native_metrics);
        metrics.early_reflection_time_us = reflection_start.elapsed().as_micros() as u64;
        metrics.hrtf_rendering_time_us += native_metrics.convolution_time_us;

        Ok(metrics)
    }

    /// Fill input buffer from playback instance
    fn fill_input_buffer(&mut self, instance: &mut PlaybackInstance, volume: f32) -> usize {
        self.cached_input_buf.fill(0.0);
        instance.fill_mono_buffer(&mut self.cached_input_buf[..self.frame_size], volume)
    }

    fn observe_voice_render(
        &self,
        source_id: SourceId,
        instance: &mut PlaybackInstance,
        emitter_position: Vec3,
        direct_local_position: Option<Vec3>,
        render_context: SpatialRenderContext,
        events: &mut Vec<PetalSonicEvent>,
    ) {
        let environment_response = self
            .acoustic_response
            .as_ref()
            .and_then(|response| response.telemetry(source_id));
        if let Some(play_command_id) = instance.take_first_render_command_id() {
            let direct_local_pose = match instance.direct_path.placement() {
                DirectPlacement::ListenerRelative(local_pose) => Some(local_pose),
                DirectPlacement::World => direct_local_position.map(Pose::from_position),
                DirectPlacement::Disabled => None,
            };
            let acoustic_origin = match instance.environment_send.origin() {
                EnvironmentOrigin::FollowEmitter => Some(Pose::from_position(emitter_position)),
                EnvironmentOrigin::World(origin) => Some(origin),
                EnvironmentOrigin::Disabled => None,
            };
            events.push(PetalSonicEvent::VoiceFirstRendered(
                VoiceFirstRenderTelemetry {
                    play_command_id,
                    emitter: instance.emitter,
                    render_block_index: render_context.render_block_index,
                    spatial_revision: render_context.spatial_revision,
                    direct_local_pose,
                    acoustic_origin,
                    environment_response,
                },
            ));
            if environment_response.is_some() {
                instance.mark_environment_response_reported();
            }
        }
        if let (Some(play_command_id), Some(response)) = (
            instance.pending_environment_response_id(),
            environment_response,
        ) {
            events.push(PetalSonicEvent::VoiceEnvironmentResponse {
                play_command_id,
                response,
            });
            instance.mark_environment_response_reported();
        }
    }

    /// Apply PetalSonic's native direct path to the input buffer.
    fn apply_native_direct_effect(
        &mut self,
        source_id: SourceId,
        direct_local_position: Vec3,
        geometry: DirectGeometry,
    ) -> Result<()> {
        let distance_world = direct_local_position.length();
        let distance_meters = distance_world * self.distance_scaler;
        let distance_attenuation = native_distance_attenuation(distance_meters);
        let air_absorption = native_air_absorption(distance_meters);
        let target_gain = if self.environmental_acoustics_active
            && matches!(geometry, DirectGeometry::SimulatedTransmission)
        {
            self.acoustic_response
                .as_ref()
                .map(|response| response.direct_gain(source_id))
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

        apply_three_band_gain(
            &self.cached_input_buf,
            &mut self.cached_direct_buf,
            state,
            ThreeBandCoefficients {
                low: self.direct_low_coefficient,
                low_mid: self.direct_low_mid_coefficient,
                smoothing: self.direct_gain_smoothing_coefficient,
            },
            target_gain,
            distance_gain,
        );
        Ok(())
    }

    fn apply_native_environment_send_effect(
        &mut self,
        source_id: SourceId,
        environment_local_position: Vec3,
        send_gain: f32,
    ) -> Result<()> {
        let distance_meters = environment_local_position.length() * self.distance_scaler;
        let distance_gain = native_distance_attenuation(distance_meters)
            * native_air_absorption(distance_meters)
            * send_gain;
        let target_gain = if self.environmental_acoustics_active {
            self.acoustic_response
                .as_ref()
                .map(|response| response.environment_gain(source_id))
                .unwrap_or([1.0; 3])
        } else {
            [1.0; 3]
        };
        let state = self
            .native_environment_source_states
            .get_mut(&source_id)
            .ok_or_else(|| {
                PetalSonicError::SpatialAudio(format!(
                    "No native environment-send state found for source {}",
                    source_id
                ))
            })?;
        apply_three_band_gain(
            &self.cached_input_buf,
            &mut self.cached_environment_send_buf,
            state,
            ThreeBandCoefficients {
                low: self.direct_low_coefficient,
                low_mid: self.direct_low_mid_coefficient,
                smoothing: self.direct_gain_smoothing_coefficient,
            },
            target_gain,
            distance_gain,
        );
        Ok(())
    }

    fn world_to_listener_position(&self, world_position: Vec3) -> Vec3 {
        let delta = world_position - self.listener_position;
        Vec3::new(
            delta.dot(self.listener_right),
            delta.dot(self.listener_up),
            delta.dot(self.listener_front),
        )
    }

    fn resolve_direct_local_position(
        &self,
        placement: DirectPlacement,
        emitter_world_position: Vec3,
    ) -> Option<Vec3> {
        match placement {
            DirectPlacement::World => Some(self.world_to_listener_position(emitter_world_position)),
            DirectPlacement::ListenerRelative(local_pose) => Some(local_pose.position),
            DirectPlacement::Disabled => None,
        }
    }

    fn apply_native_early_reflections(
        &mut self,
        source_id: SourceId,
        send_gain: f32,
        send_enabled: bool,
    ) -> Result<NativeHrtfRenderMetrics> {
        let mut targets = [None; MAX_EARLY_REFLECTION_TAPS];
        if send_enabled
            && self.environmental_acoustics_active
            && let Some(response) = &self.acoustic_response
        {
            for (target, tap) in targets
                .iter_mut()
                .zip(response.early_reflections(source_id))
            {
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
            state.delay_line[write_index] = input * send_gain;
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
        direct_local_position: Vec3,
    ) -> Result<NativeHrtfRenderMetrics> {
        let direction = normalized_direction(direct_local_position);
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

    fn apply_native_ambisonics_encode_effect(&mut self, direct_local_position: Vec3) -> Result<()> {
        let direction = normalized_direction(direct_local_position);
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

    fn capture_environmental_acoustics_state(&mut self) {
        self.environmental_acoustics_active =
            self.environmental_acoustics_enabled.load(Ordering::Acquire);
    }
}

fn apply_three_band_gain(
    input: &[f32],
    output: &mut [f32],
    state: &mut NativeDirectSourceState,
    coefficients: ThreeBandCoefficients,
    target_gain: [f32; 3],
    broadband_gain: f32,
) {
    output.fill(0.0);
    for (output, input) in output.iter_mut().zip(input) {
        state.low_state += coefficients.low * (*input - state.low_state);
        state.low_mid_state += coefficients.low_mid * (*input - state.low_mid_state);
        let bands = [
            state.low_state,
            state.low_mid_state - state.low_state,
            *input - state.low_mid_state,
        ];
        for (current, target) in state.current_gain.iter_mut().zip(target_gain) {
            *current += coefficients.smoothing * (target - *current);
        }
        *output = (bands[0] * state.current_gain[0]
            + bands[1] * state.current_gain[1]
            + bands[2] * state.current_gain[2])
            * broadband_gain;
    }
}

fn normalized_direction(position: Vec3) -> Vec3 {
    if position.is_finite() && position.length_squared() > f32::EPSILON {
        position.normalize()
    } else {
        Vec3::Z
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
    use crate::audio_data::PetalSonicAudioData;
    use crate::domain::{DirectPath, Emitter, EnvironmentSend, PlayCommandId};
    use crate::math::Quat;
    use crate::playback::{LoopMode, VoiceStart};
    use std::time::Duration;

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
    fn listener_relative_direct_pose_is_invariant_under_world_motion() {
        let mut processor = SpatialProcessor::new(SpatialProcessorConfig {
            sample_rate: 48_000,
            frame_size: 8,
            max_voices: 1,
            distance_scaler: 1.0,
            native_hrtf_path: None,
            hrtf_gain: 0.0,
            use_ambisonics: false,
            environmental_acoustics_enabled: Arc::new(AtomicBool::new(false)),
        })
        .unwrap();
        let local_pose = Pose::from_position(Vec3::new(0.02, -0.08, 0.0));
        for listener in [
            Pose::identity(),
            Pose::new(Vec3::new(40.0, -3.0, 12.0), Quat::from_rotation_y(1.7)),
            Pose::new(Vec3::new(-8.0, 6.0, 2.0), Quat::from_rotation_x(-0.4)),
        ] {
            processor.set_listener_pose(listener).unwrap();
            assert_eq!(
                processor.resolve_direct_local_position(
                    DirectPlacement::ListenerRelative(local_pose),
                    Vec3::new(999.0, 999.0, 999.0),
                ),
                Some(local_pose.position)
            );
        }
    }

    #[test]
    fn split_routing_advances_one_cursor_and_never_adds_a_second_direct_copy() {
        fn render_once(environment_send: EnvironmentSend) -> ([f32; 16], usize) {
            let source_id = SourceId::from(3);
            let audio = Arc::new(PetalSonicAudioData::new(
                (1..=32).map(|sample| sample as f32 / 32.0).collect(),
                48_000,
                1,
                Duration::from_secs_f64(32.0 / 48_000.0),
            ));
            let mut voice = PlaybackInstance::from_source(VoiceStart {
                emitter: Emitter {
                    world_id: 1,
                    index: 0,
                    generation: 1,
                },
                audio_data: audio,
                config: SourceConfig::spatial(Pose::from_position(Vec3::Z)),
                loop_mode: LoopMode::Once,
                bus_index: 0,
                playback_rate: 1.0,
                detached: false,
                completion_tag: None,
                direct_path: DirectPath::listener_relative(Pose::from_position(Vec3::X))
                    .with_geometry(DirectGeometry::BypassTransmission),
                environment_send,
                play_command_id: None,
                mono_scratch: vec![0.0; 8],
            });
            voice.play_from_beginning();
            voice.set_mix_parameters(crate::domain::BusParams::default());
            let mut voices = HashMap::from([(source_id, voice)]);
            let mut processor = SpatialProcessor::new(SpatialProcessorConfig {
                sample_rate: 48_000,
                frame_size: 8,
                max_voices: 1,
                distance_scaler: 1.0,
                native_hrtf_path: None,
                hrtf_gain: 0.0,
                use_ambisonics: false,
                environmental_acoustics_enabled: Arc::new(AtomicBool::new(false)),
            })
            .unwrap();
            let mut output = [0.0; 16];
            processor
                .process_spatial_sources_with_metrics(
                    &[source_id],
                    &mut voices,
                    &mut output,
                    SpatialRenderContext::default(),
                    &mut Vec::new(),
                )
                .unwrap();
            (output, voices[&source_id].info.current_frame)
        }

        let (split_output, split_cursor) = render_once(EnvironmentSend::from_world_pose(
            Pose::from_position(Vec3::X),
        ));
        let (direct_only_output, direct_only_cursor) = render_once(EnvironmentSend::disabled());

        assert_eq!(split_cursor, 8);
        assert_eq!(direct_only_cursor, 8);
        assert_eq!(split_output, direct_only_output);
        assert!(split_output.iter().any(|sample| sample.abs() > 0.0));
    }

    #[test]
    fn opted_in_voice_telemetry_correlates_first_render_and_late_environment_response() {
        let source_id = SourceId::from(9);
        let emitter = Emitter {
            world_id: 1,
            index: 4,
            generation: 2,
        };
        let play_command_id = PlayCommandId(71);
        let direct_local_pose = Pose::from_position(Vec3::new(0.02, -0.08, 0.0));
        let acoustic_origin = Pose::from_position(Vec3::new(14.0, 0.0, -3.0));
        let audio = Arc::new(PetalSonicAudioData::new(
            vec![0.5; 32],
            48_000,
            1,
            Duration::from_secs_f64(32.0 / 48_000.0),
        ));
        let mut voice = PlaybackInstance::from_source(VoiceStart {
            emitter,
            audio_data: audio,
            config: SourceConfig::spatial(Pose::from_position(Vec3::new(99.0, 2.0, 8.0))),
            loop_mode: LoopMode::Once,
            bus_index: 0,
            playback_rate: 1.0,
            detached: false,
            completion_tag: None,
            direct_path: DirectPath::listener_relative(direct_local_pose)
                .with_geometry(DirectGeometry::BypassTransmission),
            environment_send: EnvironmentSend::from_world_pose(acoustic_origin),
            play_command_id: Some(play_command_id),
            mono_scratch: vec![0.0; 8],
        });
        voice.play_from_beginning();
        voice.set_mix_parameters(crate::domain::BusParams::default());
        let mut voices = HashMap::from([(source_id, voice)]);
        let mut processor = SpatialProcessor::new(SpatialProcessorConfig {
            sample_rate: 48_000,
            frame_size: 8,
            max_voices: 1,
            distance_scaler: 1.0,
            native_hrtf_path: None,
            hrtf_gain: 0.0,
            use_ambisonics: false,
            environmental_acoustics_enabled: Arc::new(AtomicBool::new(false)),
        })
        .unwrap();
        let mut output = [0.0; 16];
        let mut events = Vec::with_capacity(2);

        processor
            .process_spatial_sources_with_metrics(
                &[source_id],
                &mut voices,
                &mut output,
                SpatialRenderContext {
                    render_block_index: 17,
                    spatial_revision: 23,
                },
                &mut events,
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        let PetalSonicEvent::VoiceFirstRendered(first) = events[0] else {
            panic!("expected first-render telemetry");
        };
        assert_eq!(first.play_command_id, play_command_id);
        assert_eq!(first.emitter, emitter);
        assert_eq!(first.render_block_index, 17);
        assert_eq!(first.spatial_revision, 23);
        assert_eq!(first.direct_local_pose, Some(direct_local_pose));
        assert_eq!(first.acoustic_origin, Some(acoustic_origin));
        assert_eq!(first.environment_response, None);

        events.clear();
        processor.replace_acoustic_response(Arc::new(AcousticResponse {
            spatial_revision: 24,
            geometry_version: 8,
            direct: vec![DirectAcousticResponse {
                voice_id: source_id,
                gain: [1.0; 3],
                environment_gain: [1.0; 3],
                early_reflections: Vec::new(),
            }],
            late_reverb: LateReverbParameters::SILENT,
            published_at: Instant::now() - Duration::from_millis(7),
            solve_time_us: 1,
        }));
        processor
            .process_spatial_sources_with_metrics(
                &[source_id],
                &mut voices,
                &mut output,
                SpatialRenderContext {
                    render_block_index: 18,
                    spatial_revision: 24,
                },
                &mut events,
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        let PetalSonicEvent::VoiceEnvironmentResponse {
            play_command_id: response_id,
            response,
        } = events[0]
        else {
            panic!("expected environment-response telemetry");
        };
        assert_eq!(response_id, play_command_id);
        assert_eq!(response.spatial_revision, 24);
        assert_eq!(response.geometry_version, 8);
        assert!(response.age >= Duration::from_millis(7));

        events.clear();
        processor
            .process_spatial_sources_with_metrics(
                &[source_id],
                &mut voices,
                &mut output,
                SpatialRenderContext {
                    render_block_index: 19,
                    spatial_revision: 24,
                },
                &mut events,
            )
            .unwrap();
        assert!(events.is_empty(), "voice telemetry must be one-shot");
    }

    #[test]
    fn environmental_acoustics_toggle_bypasses_geometry_but_keeps_native_direct_path() {
        let enabled = Arc::new(AtomicBool::new(true));
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
                voice_id: source_id,
                gain: [0.0; 3],
                environment_gain: [0.0; 3],
                early_reflections: Vec::new(),
            }],
            late_reverb: LateReverbParameters::SILENT,
            published_at: Instant::now(),
            solve_time_us: 1,
        }));

        for _ in 0..1_200 {
            processor.cached_input_buf.fill(1.0);
            processor
                .apply_native_direct_effect(
                    source_id,
                    Vec3::Z,
                    DirectGeometry::SimulatedTransmission,
                )
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
                .apply_native_direct_effect(
                    source_id,
                    Vec3::Z,
                    DirectGeometry::SimulatedTransmission,
                )
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
                voice_id: source_id,
                gain: [1.0; 3],
                environment_gain: [1.0; 3],
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
        processor.ensure_native_early_reflection_state_for_source(source_id);

        let mut reflected_energy = 0.0;
        for block in 0..8 {
            processor.cached_input_buf.fill(0.0);
            if block == 0 {
                processor.cached_input_buf[0] = 1.0;
            }
            processor.cached_binaural_processed.fill(0.0);
            processor
                .apply_native_early_reflections(source_id, 1.0, true)
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
                .apply_native_early_reflections(source_id, 1.0, true)
                .unwrap();
        }
        let state = &processor.native_early_reflection_source_states[&source_id];
        assert!(state.taps.iter().all(|tap| tap.path_id.is_none()));
    }

    #[test]
    fn completed_direct_voice_drains_bounded_early_reflection_tail() {
        let source_id = SourceId::from(7);
        let mut processor = SpatialProcessor::new(SpatialProcessorConfig {
            sample_rate: 48_000,
            frame_size: 1_024,
            max_voices: 1,
            distance_scaler: 1.0,
            native_hrtf_path: None,
            hrtf_gain: 0.0,
            use_ambisonics: false,
            environmental_acoustics_enabled: Arc::new(AtomicBool::new(true)),
        })
        .unwrap();
        processor.replace_acoustic_response(Arc::new(AcousticResponse {
            spatial_revision: 1,
            geometry_version: 1,
            direct: vec![DirectAcousticResponse {
                voice_id: source_id,
                gain: [1.0; 3],
                environment_gain: [1.0; 3],
                early_reflections: vec![EarlyReflectionTap {
                    path_id: 4,
                    arrival_direction: Vec3::Z,
                    delay_seconds: 0.03,
                    gain: [0.8; 3],
                }],
            }],
            late_reverb: LateReverbParameters::SILENT,
            published_at: Instant::now(),
            solve_time_us: 1,
        }));
        processor.ensure_native_early_reflection_state_for_source(source_id);
        processor.cached_input_buf.fill(0.0);
        processor.cached_input_buf[0] = 1.0;
        processor.cached_binaural_processed.fill(0.0);
        processor
            .apply_native_early_reflections(source_id, 1.0, true)
            .unwrap();

        let _ = processor.retire_source(source_id);
        assert!(processor.has_environment_tail());
        assert!(processor.native_early_reflection_source_states[&source_id].draining);

        let mut tail_energy = 0.0;
        for _ in 0..128 {
            processor.cached_binaural_processed.fill(0.0);
            processor.process_draining_early_reflections().unwrap();
            tail_energy += processor
                .cached_binaural_processed
                .iter()
                .map(|sample| sample * sample)
                .sum::<f32>();
            if !processor
                .native_early_reflection_source_states
                .contains_key(&source_id)
            {
                break;
            }
        }
        assert!(
            tail_energy > 0.0,
            "delayed environment response was truncated"
        );
        assert!(
            !processor
                .native_early_reflection_source_states
                .contains_key(&source_id),
            "early tail did not finish in bound"
        );
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
                    voice_id: SourceId::from(index as u64),
                    gain: [1.0; 3],
                    environment_gain: [1.0; 3],
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
                processor
                    .ensure_native_early_reflection_state_for_source(SourceId::from(index as u64));
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
            .map(|(source_id, _)| DirectAcousticResponse {
                voice_id: *source_id,
                gain: [1.0; 3],
                environment_gain: [1.0; 3],
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
        for (source_id, _) in &sources {
            processor.ensure_native_early_reflection_state_for_source(*source_id);
        }
        processor.cached_input_buf.fill(0.1);
        for _ in 0..32 {
            processor.cached_binaural_processed.fill(0.0);
            for (source_id, _) in &sources {
                processor
                    .apply_native_early_reflections(*source_id, 1.0, true)
                    .unwrap();
            }
        }

        let started = Instant::now();
        for _ in 0..BLOCKS {
            processor.cached_binaural_processed.fill(0.0);
            for (source_id, _) in black_box(&sources) {
                processor
                    .apply_native_early_reflections(*source_id, 1.0, true)
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

    #[test]
    #[ignore = "release-mode performance probe"]
    fn split_direct_environment_routing_release_budget() {
        use std::hint::black_box;

        const SAMPLE_RATE: u32 = 48_000;
        const FRAMES: usize = 1_024;
        const VOICES: usize = 8;
        const BLOCKS: usize = 1_000;
        let source_ids: Vec<_> = (1..=VOICES)
            .map(|voice| SourceId::from(voice as u64))
            .collect();
        let audio = Arc::new(PetalSonicAudioData::new(
            vec![0.1 / VOICES as f32; FRAMES],
            SAMPLE_RATE,
            1,
            Duration::from_secs_f64(FRAMES as f64 / SAMPLE_RATE as f64),
        ));
        let mut voices = source_ids
            .iter()
            .enumerate()
            .map(|(index, source_id)| {
                let mut voice = PlaybackInstance::from_source(VoiceStart {
                    emitter: Emitter {
                        world_id: 1,
                        index: index as u32,
                        generation: 1,
                    },
                    audio_data: audio.clone(),
                    config: SourceConfig::spatial(Pose::from_position(Vec3::Z)),
                    loop_mode: LoopMode::Infinite,
                    bus_index: 0,
                    playback_rate: 1.0,
                    detached: false,
                    completion_tag: None,
                    direct_path: DirectPath::listener_relative(Pose::from_position(Vec3::new(
                        index as f32 * 0.02 - 0.07,
                        -0.08,
                        0.2,
                    )))
                    .with_geometry(DirectGeometry::BypassTransmission),
                    environment_send: EnvironmentSend::from_world_pose(Pose::from_position(
                        Vec3::new(index as f32 - 3.5, 0.0, 4.0),
                    )),
                    play_command_id: None,
                    mono_scratch: vec![0.0; FRAMES],
                });
                voice.play_from_beginning();
                voice.set_mix_parameters(crate::domain::BusParams::default());
                (*source_id, voice)
            })
            .collect::<HashMap<_, _>>();
        let mut processor = SpatialProcessor::new(SpatialProcessorConfig {
            sample_rate: SAMPLE_RATE,
            frame_size: FRAMES,
            max_voices: VOICES,
            distance_scaler: 1.0,
            native_hrtf_path: None,
            hrtf_gain: 0.0,
            use_ambisonics: false,
            environmental_acoustics_enabled: Arc::new(AtomicBool::new(true)),
        })
        .unwrap();
        processor.replace_acoustic_response(Arc::new(AcousticResponse {
            spatial_revision: 1,
            geometry_version: 1,
            direct: source_ids
                .iter()
                .map(|source_id| DirectAcousticResponse {
                    voice_id: *source_id,
                    gain: [1.0; 3],
                    environment_gain: [1.0; 3],
                    early_reflections: vec![EarlyReflectionTap {
                        path_id: 1,
                        arrival_direction: Vec3::Z,
                        delay_seconds: 0.01,
                        gain: [0.1, 0.08, 0.05],
                    }],
                })
                .collect(),
            late_reverb: LateReverbParameters {
                pre_delay_seconds: 0.02,
                rt60_seconds: [0.8, 1.2, 0.9],
                wet_gain: 0.2,
            },
            published_at: Instant::now(),
            solve_time_us: 1,
        }));
        let mut output = vec![0.0; FRAMES * 2];
        let mut events = Vec::with_capacity(VOICES * 2);
        for _ in 0..32 {
            output.fill(0.0);
            processor
                .process_spatial_sources_with_metrics(
                    &source_ids,
                    &mut voices,
                    &mut output,
                    SpatialRenderContext::default(),
                    &mut events,
                )
                .unwrap();
        }

        let started = Instant::now();
        for _ in 0..BLOCKS {
            output.fill(0.0);
            processor
                .process_spatial_sources_with_metrics(
                    black_box(&source_ids),
                    black_box(&mut voices),
                    black_box(&mut output),
                    SpatialRenderContext::default(),
                    black_box(&mut events),
                )
                .unwrap();
        }
        let elapsed = started.elapsed();
        let audio_seconds = FRAMES as f64 * BLOCKS as f64 / SAMPLE_RATE as f64;
        println!(
            "split direct/environment routing: voices={VOICES} blocks={BLOCKS} frames={FRAMES} elapsed_ms={:.3} us_per_block={:.3} realtime_cpu_percent={:.3}",
            elapsed.as_secs_f64() * 1_000.0,
            elapsed.as_secs_f64() * 1_000_000.0 / BLOCKS as f64,
            elapsed.as_secs_f64() / audio_seconds * 100.0,
        );
        assert!(output.iter().all(|sample| sample.is_finite()));
    }
}
