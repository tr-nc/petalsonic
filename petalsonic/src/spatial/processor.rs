use super::late_reverb::{LateReverbParameters, ThreeBandFdn};
use super::native_ambisonics::{
    DEFAULT_NATIVE_AMBISONICS_ORDER, NativeAmbisonicsBinauralDecoder,
    NativeAmbisonicsBinauralState, NativeAmbisonicsEncoder, native_ambisonics_channel_count,
};
use super::native_hrtf::{
    NativeHrtfRenderMetrics, NativeHrtfRenderer, NativeHrtfSourceState, NativeHrtfTable,
};
use crate::acoustics::{AcousticRay, BatchedAnyHitRayTracer, BatchedClosestHitRayTracer};
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

const SPEED_OF_SOUND_METERS_PER_SECOND: f32 = 343.0;
const NATIVE_EARLY_REFLECTION_MAX_DELAY_SECONDS: f32 = 0.25;
const NATIVE_EARLY_REFLECTION_MAX_TRACE_METERS: f32 = 120.0;
const NATIVE_EARLY_REFLECTION_GAIN: f32 = 0.18;
const DEFAULT_NATIVE_HRTF_BYTES: &[u8] = include_bytes!("../../asset/hrtf/hrtf_b_nh172.petalhrtf");

#[derive(Debug, Clone)]
struct NativeEarlyReflectionSourceState {
    delay_line: Vec<f32>,
    write_index: usize,
    hrtf_state: NativeHrtfSourceState,
}

impl NativeEarlyReflectionSourceState {
    fn new(max_delay_samples: usize, hrtf_state: NativeHrtfSourceState) -> Self {
        Self {
            delay_line: vec![0.0; max_delay_samples.max(1)],
            write_index: 0,
            hrtf_state,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct NativeReflectionTap {
    position: Vec3,
    delay_samples: usize,
    gain: f32,
}

/// Backend allocations removed from active rendering and transferred to the
/// non-render supervisor for destruction.
pub(crate) struct RetiredSpatialSource {
    _native_hrtf: Option<NativeHrtfSourceState>,
    _native_reflection: Option<NativeEarlyReflectionSourceState>,
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
    pub batched_any_hit_ray_tracer: Option<Arc<dyn BatchedAnyHitRayTracer>>,
    pub batched_closest_hit_ray_tracer: Option<Arc<dyn BatchedClosestHitRayTracer>>,
}

/// PetalSonic's native HRTF, Ambisonics, and geometry-acoustics renderer.
pub struct SpatialProcessor {
    // Native HRTF/Ambisonics renderer and delay state
    native_hrtf_renderer: NativeHrtfRenderer,
    native_hrtf_source_states: HashMap<SourceId, NativeHrtfSourceState>,
    native_ambisonics_encoder: NativeAmbisonicsEncoder,
    native_ambisonics_decoder: Option<NativeAmbisonicsBinauralDecoder>,
    native_ambisonics_state: Option<NativeAmbisonicsBinauralState>,
    native_reflection_source_states: HashMap<SourceId, NativeEarlyReflectionSourceState>,

    // Configuration
    frame_size: usize,
    sample_rate: u32,
    distance_scaler: f32,
    use_ambisonics: bool,
    environmental_acoustics_enabled: Arc<AtomicBool>,
    environmental_acoustics_active: bool,
    acoustic_scene_supports_occlusion: bool,
    acoustic_scene_supports_reflections: bool,
    native_any_hit_ray_tracer: Option<Arc<dyn BatchedAnyHitRayTracer>>,
    native_closest_hit_ray_tracer: Option<Arc<dyn BatchedClosestHitRayTracer>>,
    late_reverb: ThreeBandFdn,
    /// HRTF gain as linear multiplier.
    hrtf_gain_linear: f32,

    // Cached buffers to avoid allocations
    cached_input_buf: Vec<f32>,             // Input mono samples
    cached_direct_buf: Vec<f32>,            // After DirectEffect
    cached_summed_encoded_buf: Vec<f32>,    // Accumulated native Ambisonics field
    cached_binaural_processed: Vec<f32>,    // Final binaural output (interleaved stereo)
    cached_native_reflection_buf: Vec<f32>, // Delayed mono early reflection scratch
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
            batched_any_hit_ray_tracer,
            batched_closest_hit_ray_tracer,
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

        let acoustic_scene_supports_occlusion = batched_any_hit_ray_tracer.is_some();
        let acoustic_scene_supports_reflections =
            batched_closest_hit_ray_tracer.is_some() && !use_ambisonics;

        // Pre-allocate buffers
        let cached_input_buf = vec![0.0; frame_size];
        let cached_direct_buf = vec![0.0; frame_size];
        let ambisonics_channel_count =
            native_ambisonics_channel_count(DEFAULT_NATIVE_AMBISONICS_ORDER)?;
        let cached_summed_encoded_buf = vec![0.0; frame_size * ambisonics_channel_count];
        let cached_binaural_processed = vec![0.0; frame_size * 2];
        let cached_native_reflection_buf = vec![0.0; frame_size];
        let cached_late_reverb_input = vec![0.0; frame_size];
        let mut late_reverb = ThreeBandFdn::new(sample_rate);
        late_reverb.set_parameters(LateReverbParameters::SILENT);

        // Pre-compute HRTF gain in linear space for efficient application.
        let hrtf_gain_linear = gain::db_to_linear(hrtf_gain);

        log::info!(
            "PetalSonic spatial processor: hrtf_backend=Native, acoustics_backend=Native, use_ambisonics={}, ambisonics_backend=Native, environmental_acoustics_enabled={}, direct_occlusion_available={}, native_early_reflections_available={}",
            use_ambisonics,
            environmental_acoustics_enabled.load(Ordering::Acquire),
            acoustic_scene_supports_occlusion,
            acoustic_scene_supports_reflections
        );

        let environmental_acoustics_active =
            environmental_acoustics_enabled.load(Ordering::Acquire);
        Ok(Self {
            native_hrtf_renderer,
            native_hrtf_source_states: HashMap::with_capacity(max_voices),
            native_ambisonics_encoder,
            native_ambisonics_decoder,
            native_ambisonics_state,
            native_reflection_source_states: HashMap::with_capacity(max_voices),
            frame_size,
            sample_rate,
            distance_scaler,
            use_ambisonics,
            environmental_acoustics_enabled,
            environmental_acoustics_active,
            acoustic_scene_supports_occlusion,
            acoustic_scene_supports_reflections,
            native_any_hit_ray_tracer: batched_any_hit_ray_tracer,
            native_closest_hit_ray_tracer: batched_closest_hit_ray_tracer,
            late_reverb,
            hrtf_gain_linear,
            cached_input_buf,
            cached_direct_buf,
            cached_summed_encoded_buf,
            cached_binaural_processed,
            cached_native_reflection_buf,
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

    pub(crate) fn set_acoustic_scene_capabilities(
        &mut self,
        supports_occlusion: bool,
        supports_reflections: bool,
    ) {
        self.acoustic_scene_supports_occlusion = supports_occlusion;
        self.acoustic_scene_supports_reflections = supports_reflections && !self.use_ambisonics;
    }

    pub(crate) fn retire_source(&mut self, source_id: SourceId) -> Option<RetiredSpatialSource> {
        let native_hrtf = self.native_hrtf_source_states.remove(&source_id);
        let native_reflection = self.native_reflection_source_states.remove(&source_id);
        (native_hrtf.is_some() || native_reflection.is_some()).then_some(RetiredSpatialSource {
            _native_hrtf: native_hrtf,
            _native_reflection: native_reflection,
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

    fn ensure_native_reflection_state_for_source(&mut self, source_id: SourceId) -> Result<()> {
        if !self.early_reflections_enabled()
            || self
                .native_reflection_source_states
                .contains_key(&source_id)
        {
            return Ok(());
        }

        let max_delay_samples =
            (self.sample_rate as f32 * NATIVE_EARLY_REFLECTION_MAX_DELAY_SECONDS).ceil() as usize;
        self.native_reflection_source_states.insert(
            source_id,
            NativeEarlyReflectionSourceState::new(
                max_delay_samples,
                self.native_hrtf_renderer.create_source_state(),
            ),
        );
        Ok(())
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
            self.ensure_native_reflection_state_for_source(*source_id)?;
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
        self.apply_native_direct_effect(position);
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
            self.apply_native_early_reflection_effect(source_id, position)?;
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
    fn apply_native_direct_effect(&mut self, source_position: Vec3) {
        let source_delta = source_position - self.listener_position;
        let distance_world = source_delta.length();
        let distance_meters = distance_world * self.distance_scaler;
        let distance_attenuation = native_distance_attenuation(distance_meters);
        let air_absorption = native_air_absorption(distance_meters);

        let occlusion = if self.direct_occlusion_enabled() {
            self.native_direct_occlusion(source_position, distance_world)
        } else {
            1.0
        };
        let direct_gain = distance_attenuation * air_absorption * occlusion;

        self.cached_direct_buf.fill(0.0);
        for (output, input) in self
            .cached_direct_buf
            .iter_mut()
            .zip(self.cached_input_buf.iter())
        {
            *output = *input * direct_gain;
        }
    }

    fn native_direct_occlusion(&self, source_position: Vec3, distance_world: f32) -> f32 {
        if distance_world <= f32::EPSILON {
            return 1.0;
        }

        let Some(ray_tracer) = &self.native_any_hit_ray_tracer else {
            return 1.0;
        };

        let direction = (source_position - self.listener_position) / distance_world;
        let rays = [AcousticRay {
            origin: self.listener_position,
            direction,
        }];
        let min_distances = [0.0];
        let max_distances = [distance_world];

        let mut hits = [false];
        ray_tracer.trace_any_hit_batch(&rays, &min_distances, &max_distances, &mut hits);
        let is_occluded = hits[0];

        if is_occluded { 0.0 } else { 1.0 }
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

    fn apply_native_early_reflection_effect(
        &mut self,
        source_id: SourceId,
        source_position: Vec3,
    ) -> Result<()> {
        if !self.early_reflections_enabled() {
            return Ok(());
        }

        let Some(tap) = self.native_early_reflection_tap(source_position) else {
            return Ok(());
        };
        let direction = self.get_target_direction(tap.position);
        let Some(state) = self.native_reflection_source_states.get_mut(&source_id) else {
            return Ok(());
        };
        if tap.delay_samples >= state.delay_line.len() {
            return Ok(());
        }

        self.cached_native_reflection_buf.fill(0.0);
        for (output, input) in self
            .cached_native_reflection_buf
            .iter_mut()
            .zip(self.cached_input_buf.iter())
        {
            state.delay_line[state.write_index] = *input * tap.gain;
            let read_index = (state.write_index + state.delay_line.len() - tap.delay_samples)
                % state.delay_line.len();
            *output = state.delay_line[read_index];
            state.write_index = (state.write_index + 1) % state.delay_line.len();
        }

        self.native_hrtf_renderer.render_source(
            &mut state.hrtf_state,
            direction,
            &self.cached_native_reflection_buf,
            &mut self.cached_binaural_processed,
        )
    }

    fn native_early_reflection_tap(&self, source_position: Vec3) -> Option<NativeReflectionTap> {
        let closest_hit_ray_tracer = self.native_closest_hit_ray_tracer.as_ref()?;
        let source_delta = source_position - self.listener_position;
        let source_distance_world = source_delta.length();
        if !source_distance_world.is_finite() || source_distance_world <= f32::EPSILON {
            return None;
        }

        let direct_direction = source_delta / source_distance_world;
        let probe_direction = normalize_or_default(direct_direction + Vec3::new(0.0, -0.65, 0.0));
        let max_trace_distance_world = (NATIVE_EARLY_REFLECTION_MAX_TRACE_METERS
            / self.distance_scaler.max(0.001))
        .max(1.0)
        .min((source_distance_world * 1.5).max(1.0));

        let rays = [AcousticRay {
            origin: self.listener_position,
            direction: probe_direction,
        }];
        let min_distances = [0.05];
        let max_distances = [max_trace_distance_world];
        let mut hits = [None];
        closest_hit_ray_tracer.trace_closest_hit_batch(
            &rays,
            &min_distances,
            &max_distances,
            &mut hits,
        );
        let hit = hits[0]?;

        let hit_distance_world = hit.distance.max(0.0);
        let hit_position = self.listener_position + probe_direction * hit_distance_world;
        let hit_to_source = source_position - hit_position;
        let hit_to_source_distance_world = hit_to_source.length();
        if !hit_to_source_distance_world.is_finite() || hit_to_source_distance_world <= f32::EPSILON
        {
            return None;
        }

        if let Some(any_hit_ray_tracer) = &self.native_any_hit_ray_tracer {
            let visibility_direction = hit_to_source / hit_to_source_distance_world;
            let visibility_origin = hit_position + normalize_or_default(hit.normal) * 0.05;
            let visibility_rays = [AcousticRay {
                origin: visibility_origin,
                direction: visibility_direction,
            }];
            let visibility_min = [0.05];
            let visibility_max = [(hit_to_source_distance_world - 0.05).max(0.05)];
            let mut hits = [false];
            any_hit_ray_tracer.trace_any_hit_batch(
                &visibility_rays,
                &visibility_min,
                &visibility_max,
                &mut hits,
            );
            let blocked = hits[0];
            if blocked {
                return None;
            }
        }

        let total_distance_world = hit_distance_world + hit_to_source_distance_world;
        let total_distance_meters = total_distance_world * self.distance_scaler;
        let delay_seconds = total_distance_meters / SPEED_OF_SOUND_METERS_PER_SECOND;
        let delay_samples = (delay_seconds * self.sample_rate as f32).round() as usize;
        let reflectivity = (1.0
            - (hit.material.absorption[0]
                + hit.material.absorption[1]
                + hit.material.absorption[2])
                / 3.0)
            .clamp(0.0, 1.0);
        let incidence = normalize_or_default(hit.normal)
            .dot(-probe_direction)
            .abs()
            .clamp(0.25, 1.0);
        let gain = NATIVE_EARLY_REFLECTION_GAIN
            * reflectivity
            * incidence
            * native_air_absorption(total_distance_meters)
            * native_distance_attenuation(total_distance_meters);

        if gain <= f32::EPSILON || delay_samples == 0 {
            return None;
        }

        Some(NativeReflectionTap {
            position: hit_position,
            delay_samples,
            gain,
        })
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

    fn direct_occlusion_enabled(&self) -> bool {
        self.acoustic_scene_supports_occlusion && self.environmental_acoustics_active
    }

    fn capture_environmental_acoustics_state(&mut self) {
        self.environmental_acoustics_active =
            self.environmental_acoustics_enabled.load(Ordering::Acquire);
    }

    fn early_reflections_enabled(&self) -> bool {
        self.acoustic_scene_supports_reflections && self.environmental_acoustics_active
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

fn normalize_or_default(direction: Vec3) -> Vec3 {
    if direction.is_finite() && direction.length_squared() > f32::EPSILON {
        direction.normalize()
    } else {
        Vec3::Z
    }
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

    struct AlwaysOccluded;

    impl BatchedAnyHitRayTracer for AlwaysOccluded {
        fn trace_any_hit_batch(
            &self,
            rays: &[AcousticRay],
            _min_distances: &[f32],
            _max_distances: &[f32],
            hits: &mut [bool],
        ) {
            hits[..rays.len()].fill(true);
        }
    }

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
        let mut processor = SpatialProcessor::new(SpatialProcessorConfig {
            sample_rate: 48_000,
            frame_size: 8,
            max_voices: 1,
            distance_scaler: 1.0,
            native_hrtf_path: None,
            hrtf_gain: 0.0,
            use_ambisonics: false,
            environmental_acoustics_enabled: enabled.clone(),
            batched_any_hit_ray_tracer: Some(Arc::new(AlwaysOccluded)),
            batched_closest_hit_ray_tracer: None,
        })
        .unwrap();
        processor.set_acoustic_scene_capabilities(true, false);
        processor.cached_input_buf.fill(1.0);

        processor.apply_native_direct_effect(Vec3::Z);
        assert_eq!(processor.cached_direct_buf, vec![0.0; 8]);

        enabled.store(false, Ordering::Release);
        processor.capture_environmental_acoustics_state();
        processor.apply_native_direct_effect(Vec3::Z);
        assert!(
            processor
                .cached_direct_buf
                .iter()
                .all(|sample| *sample > 0.99)
        );
    }
}
