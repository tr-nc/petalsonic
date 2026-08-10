use super::native_ambisonics::{
    DEFAULT_NATIVE_AMBISONICS_ORDER, NativeAmbisonicsBinauralDecoder,
    NativeAmbisonicsBinauralState, NativeAmbisonicsEncoder, native_ambisonics_channel_count,
};
use super::native_hrtf::{
    NativeHrtfRenderMetrics, NativeHrtfRenderer, NativeHrtfSourceState, NativeHrtfTable,
};
use crate::acoustics::{AcousticRay, BatchedAnyHitRayTracer, BatchedClosestHitRayTracer};
use crate::config::{AmbisonicsBackend, HrtfBackend, SourceConfig};
use crate::error::{PetalSonicError, Result};
use crate::gain;
use crate::math::{Pose, Vec3};
use crate::playback::PlaybackInstance;
use crate::spatial::effects::SpatialEffectsManager;
use crate::spatial::hrtf;
use crate::world::SourceId;
use audionimbus::{
    AmbisonicsDecodeEffect, AmbisonicsDecodeEffectParams, AmbisonicsDecodeEffectSettings,
    AmbisonicsEncodeEffectParams, AudioBufferSettings, AudioSettings, BinauralEffectParams,
    Context, CoordinateSystem, Direction, Hrtf, HrtfInterpolation, Rendering, SpeakerLayout,
    Vector3, audio_buffer::AudioBuffer as AudioNimbusAudioBuffer,
};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

const STEAM_AMBISONICS_ORDER: u32 = 2;
const SPEED_OF_SOUND_METERS_PER_SECOND: f32 = 343.0;
const NATIVE_EARLY_REFLECTION_MAX_DELAY_SECONDS: f32 = 0.25;
const NATIVE_EARLY_REFLECTION_MAX_TRACE_METERS: f32 = 120.0;
const NATIVE_EARLY_REFLECTION_GAIN: f32 = 0.18;

/// Process-lifetime Steam Audio kernel shared by every world-owned renderer.
///
/// Steam Audio documents its context as a typically single, application-lifetime
/// object. Keeping that lifetime inside PetalSonic avoids exposing a second resource
/// graph to callers and avoids unsafe context destruction/recreation in `phonon.dll`.
struct SharedSteamAudioContext(Context);

// SAFETY: Steam Audio documents its API objects as usable from multiple threads.
// PetalSonic only shares the context itself; mutable effects remain owned by one
// render runtime. Audionimbus separately serializes the non-thread-safe HRTF
// creation call with its process-global lock.
unsafe impl Sync for SharedSteamAudioContext {}

impl Deref for SharedSteamAudioContext {
    type Target = Context;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

static STEAM_AUDIO_CONTEXT: OnceLock<std::result::Result<Arc<SharedSteamAudioContext>, String>> =
    OnceLock::new();

fn shared_steam_audio_context() -> Result<Arc<SharedSteamAudioContext>> {
    STEAM_AUDIO_CONTEXT
        .get_or_init(|| {
            Context::try_new(&audionimbus::ContextSettings::default())
                .map(|context| Arc::new(SharedSteamAudioContext(context)))
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map(Arc::clone)
        .map_err(|reason| {
            PetalSonicError::SpatialAudio(format!("Failed to create Steam Audio context: {reason}"))
        })
}

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
    _steam: Option<crate::spatial::effects::SpatialSourceEffects>,
    _native_hrtf: Option<NativeHrtfSourceState>,
    _native_reflection: Option<NativeEarlyReflectionSourceState>,
}

pub(crate) struct SpatialProcessorConfig {
    pub sample_rate: u32,
    pub frame_size: usize,
    pub max_voices: usize,
    pub distance_scaler: f32,
    pub steam_hrtf_path: Option<String>,
    pub native_hrtf_path: Option<String>,
    pub hrtf_gain: f32,
    pub hrtf_backend: HrtfBackend,
    pub use_ambisonics: bool,
    pub ambisonics_backend: AmbisonicsBackend,
    pub batched_any_hit_ray_tracer: Option<Arc<dyn BatchedAnyHitRayTracer>>,
    pub batched_closest_hit_ray_tracer: Option<Arc<dyn BatchedClosestHitRayTracer>>,
}

/// Spatial audio processor that manages Steam Audio integration
pub struct SpatialProcessor {
    // Drop source/effect wrappers before the context they use.
    effects_manager: SpatialEffectsManager,

    // Native HRTF/Ambisonics renderer and delay state
    native_hrtf_renderer: Option<NativeHrtfRenderer>,
    native_hrtf_source_states: HashMap<SourceId, NativeHrtfSourceState>,
    native_ambisonics_encoder: NativeAmbisonicsEncoder,
    native_ambisonics_decoder: Option<NativeAmbisonicsBinauralDecoder>,
    native_ambisonics_state: Option<NativeAmbisonicsBinauralState>,
    native_reflection_source_states: HashMap<SourceId, NativeEarlyReflectionSourceState>,

    // Steam Audio effects that do not call into host scene code. The context is
    // process-lived; field order still ensures this world's effects drop first.
    ambisonics_decode_effect: Option<AmbisonicsDecodeEffect>,
    hrtf: Option<Hrtf>,
    context: Arc<SharedSteamAudioContext>,

    // Configuration
    frame_size: usize,
    sample_rate: u32,
    distance_scaler: f32,
    hrtf_backend: HrtfBackend,
    use_ambisonics: bool,
    ambisonics_backend: AmbisonicsBackend,
    direct_occlusion_enabled: bool,
    native_early_reflections_enabled: bool,
    native_any_hit_ray_tracer: Option<Arc<dyn BatchedAnyHitRayTracer>>,
    native_closest_hit_ray_tracer: Option<Arc<dyn BatchedClosestHitRayTracer>>,
    /// HRTF gain as linear multiplier.
    hrtf_gain_linear: f32,

    // Cached buffers to avoid allocations
    cached_input_buf: Vec<f32>,             // Input mono samples
    cached_direct_buf: Vec<f32>,            // After DirectEffect
    cached_summed_encoded_buf: Vec<f32>,    // Accumulated ambisonics (9 channels for order 2)
    cached_ambisonics_encode_buf: Vec<f32>, // Temp buffer for encoding
    cached_ambisonics_decode_buf: Vec<f32>, // After AmbisonicsDecode (stereo)
    cached_binaural_processed: Vec<f32>,    // Final binaural output (interleaved stereo)
    cached_native_reflection_buf: Vec<f32>, // Delayed mono early reflection scratch

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
            steam_hrtf_path,
            native_hrtf_path,
            hrtf_gain,
            hrtf_backend,
            use_ambisonics,
            ambisonics_backend,
            batched_any_hit_ray_tracer,
            batched_closest_hit_ray_tracer,
        } = config;
        #[cfg(all(test, target_os = "windows"))]
        eprintln!("[DEBUG-win-context] acquiring shared context");
        let context = shared_steam_audio_context()?;
        #[cfg(all(test, target_os = "windows"))]
        eprintln!("[DEBUG-win-context] acquired shared context");

        let audio_settings = AudioSettings {
            sampling_rate: sample_rate,
            frame_size: frame_size as u32,
        };

        // Create only the fixed backend resources selected for this world.
        let mut hrtf = None;
        let mut ambisonics_decode_effect = None;
        let mut native_hrtf_renderer = None;
        let mut native_ambisonics_decoder = None;
        let mut native_ambisonics_state = None;

        if hrtf_backend == HrtfBackend::SteamAudio {
            #[cfg(all(test, target_os = "windows"))]
            eprintln!("[DEBUG-win-context] creating HRTF");
            let loaded_hrtf =
                create_steam_hrtf(&context, &audio_settings, steam_hrtf_path.as_deref())?;
            #[cfg(all(test, target_os = "windows"))]
            eprintln!("[DEBUG-win-context] created HRTF; creating decode effect");
            ambisonics_decode_effect = Some(create_ambisonics_decode_effect(
                &context,
                &audio_settings,
                &loaded_hrtf,
            )?);
            #[cfg(all(test, target_os = "windows"))]
            eprintln!("[DEBUG-win-context] created decode effect");
            hrtf = Some(loaded_hrtf);
        }

        if hrtf_backend == HrtfBackend::Native {
            let table = load_native_hrtf_table(sample_rate, native_hrtf_path.as_deref())?;
            native_hrtf_renderer = Some(NativeHrtfRenderer::with_frame_size(
                table.clone(),
                frame_size,
            )?);
            if use_ambisonics && hrtf_backend == HrtfBackend::Native {
                let decoder = NativeAmbisonicsBinauralDecoder::with_frame_size(
                    table.clone(),
                    DEFAULT_NATIVE_AMBISONICS_ORDER,
                    frame_size,
                )?;
                native_ambisonics_state = Some(decoder.create_state());
                native_ambisonics_decoder = Some(decoder);
            }
        }

        let native_ambisonics_encoder =
            NativeAmbisonicsEncoder::new(DEFAULT_NATIVE_AMBISONICS_ORDER)?;

        let direct_occlusion_enabled = batched_any_hit_ray_tracer.is_some();
        let native_early_reflections_enabled = batched_closest_hit_ray_tracer.is_some()
            && hrtf_backend == HrtfBackend::Native
            && !use_ambisonics;

        // Pre-allocate buffers
        let cached_input_buf = vec![0.0; frame_size];
        let cached_direct_buf = vec![0.0; frame_size];
        let ambisonics_channel_count =
            native_ambisonics_channel_count(DEFAULT_NATIVE_AMBISONICS_ORDER)?;
        let cached_summed_encoded_buf = vec![0.0; frame_size * ambisonics_channel_count];
        let cached_ambisonics_encode_buf = vec![0.0; frame_size * ambisonics_channel_count];
        let cached_ambisonics_decode_buf = vec![0.0; frame_size * 2]; // Planar stereo
        let cached_binaural_processed = vec![0.0; frame_size * 2];
        let cached_native_reflection_buf = vec![0.0; frame_size];

        // Pre-compute HRTF gain in linear space for efficient application.
        let hrtf_gain_linear = gain::db_to_linear(hrtf_gain);

        log::info!(
            "PetalSonic spatial processor: hrtf_backend={:?}, acoustics_backend=Native, use_ambisonics={}, ambisonics_backend={:?}, direct_occlusion_enabled={}, native_early_reflections_enabled={}",
            hrtf_backend,
            use_ambisonics,
            ambisonics_backend,
            direct_occlusion_enabled,
            native_early_reflections_enabled
        );

        Ok(Self {
            effects_manager: SpatialEffectsManager::new(max_voices),
            native_hrtf_renderer,
            native_hrtf_source_states: HashMap::with_capacity(max_voices),
            native_ambisonics_encoder,
            native_ambisonics_decoder,
            native_ambisonics_state,
            native_reflection_source_states: HashMap::with_capacity(max_voices),
            ambisonics_decode_effect,
            hrtf,
            context,
            frame_size,
            sample_rate,
            distance_scaler,
            hrtf_backend,
            use_ambisonics,
            ambisonics_backend,
            direct_occlusion_enabled,
            native_early_reflections_enabled,
            native_any_hit_ray_tracer: batched_any_hit_ray_tracer,
            native_closest_hit_ray_tracer: batched_closest_hit_ray_tracer,
            hrtf_gain_linear,
            cached_input_buf,
            cached_direct_buf,
            cached_summed_encoded_buf,
            cached_ambisonics_encode_buf,
            cached_ambisonics_decode_buf,
            cached_binaural_processed,
            cached_native_reflection_buf,
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
        self.direct_occlusion_enabled = supports_occlusion;
        self.native_early_reflections_enabled = supports_reflections
            && self.hrtf_backend == HrtfBackend::Native
            && !self.use_ambisonics;
    }

    /// Create effects for a spatial source
    pub fn create_effects_for_source(&mut self, source_id: SourceId) -> Result<()> {
        let audio_settings = AudioSettings {
            sampling_rate: self.sample_rate,
            frame_size: self.frame_size as u32,
        };

        self.effects_manager.create_effects_for_source(
            source_id,
            &self.context,
            &audio_settings,
            self.hrtf.as_ref(),
        )
    }

    pub(crate) fn retire_source(&mut self, source_id: SourceId) -> Option<RetiredSpatialSource> {
        let steam = self.effects_manager.retire_source(source_id);
        let native_hrtf = self.native_hrtf_source_states.remove(&source_id);
        let native_reflection = self.native_reflection_source_states.remove(&source_id);
        (steam.is_some() || native_hrtf.is_some() || native_reflection.is_some()).then_some(
            RetiredSpatialSource {
                _steam: steam,
                _native_hrtf: native_hrtf,
                _native_reflection: native_reflection,
            },
        )
    }

    fn uses_steam_source_effects(&self) -> bool {
        (self.use_ambisonics && self.ambisonics_backend == AmbisonicsBackend::SteamAudio)
            || (!self.use_ambisonics && self.hrtf_backend == HrtfBackend::SteamAudio)
    }

    fn ensure_native_hrtf_state_for_source(&mut self, source_id: SourceId) -> Result<()> {
        if self.hrtf_backend != HrtfBackend::Native
            || self.use_ambisonics
            || self.native_hrtf_source_states.contains_key(&source_id)
        {
            return Ok(());
        }

        let renderer = self.native_hrtf_renderer.as_ref().ok_or_else(|| {
            PetalSonicError::SpatialAudio("native HRTF renderer is not initialized".to_string())
        })?;
        self.native_hrtf_source_states
            .insert(source_id, renderer.create_source_state());
        Ok(())
    }

    fn ensure_native_reflection_state_for_source(&mut self, source_id: SourceId) -> Result<()> {
        if !self.native_early_reflections_enabled
            || self
                .native_reflection_source_states
                .contains_key(&source_id)
        {
            return Ok(());
        }

        let renderer = self.native_hrtf_renderer.as_ref().ok_or_else(|| {
            PetalSonicError::SpatialAudio("native HRTF renderer is not initialized".to_string())
        })?;
        let max_delay_samples =
            (self.sample_rate as f32 * NATIVE_EARLY_REFLECTION_MAX_DELAY_SECONDS).ceil() as usize;
        self.native_reflection_source_states.insert(
            source_id,
            NativeEarlyReflectionSourceState::new(
                max_delay_samples,
                renderer.create_source_state(),
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
        if spatial_ids.is_empty() {
            // No spatial sources, don't modify the buffer (may contain non-spatial audio)
            return Ok(SpatialProcessingMetrics::default());
        }

        let mut metrics = SpatialProcessingMetrics {
            spatial_source_count: spatial_ids.len(),
            ..SpatialProcessingMetrics::default()
        };

        // Ensure all spatial sources have backend state created before processing.
        // This guarantees newly played spatial sources participate in the very first
        // block, avoiding a "first block louder" case where distance attenuation /
        // air absorption would still be at their default values.
        let uses_steam_source_effects = self.uses_steam_source_effects();
        for source_id in spatial_ids {
            let Some(instance) = instances.get(source_id) else {
                continue;
            };
            if !matches!(instance.config, SourceConfig::Spatial { .. }) {
                continue;
            }

            if uses_steam_source_effects && !self.effects_manager.has_effects(*source_id) {
                self.create_effects_for_source(*source_id)?;
            }
            self.ensure_native_hrtf_state_for_source(*source_id)?;
            self.ensure_native_reflection_state_for_source(*source_id)?;
        }

        // Clear accumulation buffers
        self.cached_summed_encoded_buf.fill(0.0);
        self.cached_binaural_processed.fill(0.0);

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
            match self.hrtf_backend {
                HrtfBackend::SteamAudio => self.apply_ambisonics_decode_effect()?,
                HrtfBackend::Native => {
                    let native_metrics = self.apply_native_ambisonics_decode_effect()?;
                    metrics.native_hrtf_direction_lookup_time_us +=
                        native_metrics.direction_lookup_time_us;
                    metrics.native_hrtf_convolution_time_us += native_metrics.convolution_time_us;
                }
            }
            let decode_elapsed = decoding_start.elapsed().as_micros() as u64;
            metrics.ambisonics_decoding_time_us = decode_elapsed;
            metrics.hrtf_rendering_time_us += decode_elapsed;

            if self.hrtf_backend == HrtfBackend::Native && self.hrtf_gain_linear != 1.0 {
                self.apply_hrtf_gain();
            }
        } else if self.hrtf_gain_linear != 1.0 {
            self.apply_hrtf_gain();
        }

        // Add to output buffer (don't overwrite - allow mixing with non-spatial sources)
        let frames_to_copy = (output_buffer.len() / 2).min(self.frame_size);
        for i in 0..frames_to_copy {
            output_buffer[i * 2] += self.cached_binaural_processed[i * 2];
            output_buffer[i * 2 + 1] += self.cached_binaural_processed[i * 2 + 1];
        }

        Ok(metrics)
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
        metrics.direct_processing_time_us = direct_start.elapsed().as_micros() as u64;

        if self.use_ambisonics {
            let encoding_start = Instant::now();
            match self.ambisonics_backend {
                AmbisonicsBackend::SteamAudio => {
                    self.apply_steam_ambisonics_encode_effect(source_id, position)?
                }
                AmbisonicsBackend::Native => {
                    self.apply_native_ambisonics_encode_effect(position)?
                }
            }
            metrics.ambisonics_encoding_time_us = encoding_start.elapsed().as_micros() as u64;
        } else {
            let render_start = Instant::now();
            match self.hrtf_backend {
                HrtfBackend::SteamAudio => self.apply_steam_binaural_effect(source_id, position)?,
                HrtfBackend::Native => {
                    let native_metrics = self.apply_native_hrtf_effect(source_id, position)?;
                    metrics.add_native_hrtf_metrics(native_metrics);
                    self.apply_native_early_reflection_effect(source_id, position)?;
                }
            }
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

        let occlusion = if self.direct_occlusion_enabled {
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
        let renderer = self.native_hrtf_renderer.as_ref().ok_or_else(|| {
            PetalSonicError::SpatialAudio("native HRTF renderer is not initialized".to_string())
        })?;
        let state = self
            .native_hrtf_source_states
            .get_mut(&source_id)
            .ok_or_else(|| {
                PetalSonicError::SpatialAudio(format!(
                    "No native HRTF state found for source {}",
                    source_id
                ))
            })?;

        renderer.render_source_with_metrics(
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
        if !self.native_early_reflections_enabled {
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

        let renderer = self.native_hrtf_renderer.as_ref().ok_or_else(|| {
            PetalSonicError::SpatialAudio("native HRTF renderer is not initialized".to_string())
        })?;
        renderer.render_source(
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

    /// Apply Steam Audio's ambisonics encode effect.
    fn apply_steam_ambisonics_encode_effect(
        &mut self,
        source_id: SourceId,
        source_position: Vec3,
    ) -> Result<()> {
        // Calculate direction first to avoid borrow checker issues
        let direction = self.get_target_direction(source_position);

        let effects = self
            .effects_manager
            .get_effects_mut(source_id)
            .ok_or_else(|| {
                PetalSonicError::SpatialAudio(format!("No effects found for source {}", source_id))
            })?;

        let ambisonics_encode_effect_params = AmbisonicsEncodeEffectParams {
            direction: Direction::new(direction.x, direction.y, direction.z),
            order: STEAM_AMBISONICS_ORDER,
        };

        let input_buf = AudioNimbusAudioBuffer::try_with_data_and_settings(
            &self.cached_direct_buf,
            AudioBufferSettings {
                num_channels: Some(1),
                ..Default::default()
            },
        )
        .map_err(|e| {
            PetalSonicError::SpatialAudio(format!("Failed to create input buffer: {}", e))
        })?;

        self.cached_ambisonics_encode_buf.fill(0.0);
        let steam_channel_count = native_ambisonics_channel_count(STEAM_AMBISONICS_ORDER)?;
        let steam_sample_count = self.frame_size * steam_channel_count;
        let output_buf = AudioNimbusAudioBuffer::try_with_data_and_settings(
            &mut self.cached_ambisonics_encode_buf[..steam_sample_count],
            AudioBufferSettings {
                num_channels: Some(steam_channel_count as u32),
                ..Default::default()
            },
        )
        .map_err(|e| {
            PetalSonicError::SpatialAudio(format!("Failed to create output buffer: {}", e))
        })?;

        effects
            .ambisonics_encode_effect
            .apply(&ambisonics_encode_effect_params, &input_buf, &output_buf)
            .map_err(|e| {
                PetalSonicError::SpatialAudio(format!(
                    "Failed to apply AmbisonicsEncodeEffect: {}",
                    e
                ))
            })?;

        // Accumulate encoded output to summed buffer
        for i in 0..steam_sample_count {
            self.cached_summed_encoded_buf[i] += self.cached_ambisonics_encode_buf[i];
        }

        Ok(())
    }

    fn apply_native_ambisonics_encode_effect(&mut self, source_position: Vec3) -> Result<()> {
        let direction = self.get_target_direction(source_position);
        self.native_ambisonics_encoder.encode_source_accumulate(
            direction,
            &self.cached_direct_buf,
            &mut self.cached_summed_encoded_buf,
        )
    }

    fn apply_steam_binaural_effect(
        &mut self,
        source_id: SourceId,
        source_position: Vec3,
    ) -> Result<()> {
        let direction = self.get_target_direction(source_position);
        let hrtf = self.hrtf.as_ref().ok_or_else(|| {
            PetalSonicError::SpatialAudio("Steam Audio HRTF is not initialized".to_string())
        })?;
        let effects = self
            .effects_manager
            .get_effects_mut(source_id)
            .ok_or_else(|| {
                PetalSonicError::SpatialAudio(format!("No effects found for source {}", source_id))
            })?;
        let binaural_effect = effects.binaural_effect.as_mut().ok_or_else(|| {
            PetalSonicError::SpatialAudio(format!(
                "No Steam Audio BinauralEffect found for source {}",
                source_id
            ))
        })?;

        self.cached_ambisonics_decode_buf.fill(0.0);
        let input_buf = AudioNimbusAudioBuffer::try_with_data_and_settings(
            &self.cached_direct_buf,
            AudioBufferSettings {
                num_channels: Some(1),
                ..Default::default()
            },
        )
        .map_err(|e| {
            PetalSonicError::SpatialAudio(format!("Failed to create binaural input buffer: {}", e))
        })?;
        let output_buf = AudioNimbusAudioBuffer::try_with_data_and_settings(
            &mut self.cached_ambisonics_decode_buf,
            AudioBufferSettings {
                num_channels: Some(2),
                ..Default::default()
            },
        )
        .map_err(|e| {
            PetalSonicError::SpatialAudio(format!("Failed to create binaural output buffer: {}", e))
        })?;

        let params = BinauralEffectParams {
            direction: petal_to_steam_hrtf_direction(direction),
            interpolation: HrtfInterpolation::Nearest,
            spatial_blend: 1.0,
            hrtf,
            peak_delays: None,
        };

        binaural_effect
            .apply(&params, &input_buf, &output_buf)
            .map_err(|e| {
                PetalSonicError::SpatialAudio(format!("Failed to apply BinauralEffect: {}", e))
            })?;

        let frames = self.frame_size;
        let (left, right) = self.cached_ambisonics_decode_buf.split_at(frames);
        for frame in 0..frames {
            let out_index = frame * 2;
            self.cached_binaural_processed[out_index] += left[frame];
            self.cached_binaural_processed[out_index + 1] += right[frame];
        }

        Ok(())
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

    /// Apply ambisonics decode effect to convert accumulated ambisonics to binaural stereo
    fn apply_ambisonics_decode_effect(&mut self) -> Result<()> {
        let hrtf = self.hrtf.as_ref().ok_or_else(|| {
            PetalSonicError::SpatialAudio("Steam Audio HRTF is not initialized".to_string())
        })?;
        let ambisonics_decode_effect = self.ambisonics_decode_effect.as_mut().ok_or_else(|| {
            PetalSonicError::SpatialAudio(
                "Steam Audio AmbisonicsDecodeEffect is not initialized".to_string(),
            )
        })?;

        let ambisonics_decode_effect_params = AmbisonicsDecodeEffectParams {
            order: STEAM_AMBISONICS_ORDER,
            hrtf,
            orientation: CoordinateSystem {
                ahead: Vector3::new(0.0, 0.0, -1.0),
                ..Default::default()
            },
        };

        let steam_channel_count = native_ambisonics_channel_count(STEAM_AMBISONICS_ORDER)?;
        let steam_sample_count = self.frame_size * steam_channel_count;
        let input_buf = AudioNimbusAudioBuffer::try_with_data_and_settings(
            &self.cached_summed_encoded_buf[..steam_sample_count],
            AudioBufferSettings {
                num_channels: Some(steam_channel_count as u32),
                ..Default::default()
            },
        )
        .map_err(|e| {
            PetalSonicError::SpatialAudio(format!("Failed to create input buffer: {}", e))
        })?;

        let output_buf = AudioNimbusAudioBuffer::try_with_data_and_settings(
            &mut self.cached_ambisonics_decode_buf,
            AudioBufferSettings {
                num_channels: Some(2), // Stereo
                ..Default::default()
            },
        )
        .map_err(|e| {
            PetalSonicError::SpatialAudio(format!("Failed to create output buffer: {}", e))
        })?;

        ambisonics_decode_effect
            .apply(&ambisonics_decode_effect_params, &input_buf, &output_buf)
            .map_err(|e| {
                PetalSonicError::SpatialAudio(format!(
                    "Failed to apply AmbisonicsDecodeEffect: {}",
                    e
                ))
            })?;

        // Interleave to binaural_processed buffer
        let decoded_buf = AudioNimbusAudioBuffer::try_with_data_and_settings(
            &mut self.cached_ambisonics_decode_buf,
            AudioBufferSettings {
                num_channels: Some(2),
                ..Default::default()
            },
        )
        .map_err(|e| {
            PetalSonicError::SpatialAudio(format!("Failed to create decoded buffer: {}", e))
        })?;

        decoded_buf
            .interleave(&self.context, &mut self.cached_binaural_processed)
            .map_err(|e| {
                PetalSonicError::SpatialAudio(format!("Failed to interleave decoded audio: {}", e))
            })?;

        // Apply HRTF gain compensation (linear multiplier derived from dB)
        if self.hrtf_gain_linear != 1.0 {
            for sample in self.cached_binaural_processed.iter_mut() {
                *sample *= self.hrtf_gain_linear;
            }
        }

        Ok(())
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
}

fn create_steam_hrtf(
    context: &Context,
    audio_settings: &AudioSettings,
    steam_hrtf_path: Option<&str>,
) -> Result<Hrtf> {
    if let Some(path) = steam_hrtf_path {
        hrtf::create_hrtf_from_file(context, audio_settings, path)
    } else {
        hrtf::create_default_hrtf(context, audio_settings)
    }
}

fn create_ambisonics_decode_effect(
    context: &Context,
    audio_settings: &AudioSettings,
    hrtf: &Hrtf,
) -> Result<AmbisonicsDecodeEffect> {
    AmbisonicsDecodeEffect::try_new(
        context,
        audio_settings,
        &AmbisonicsDecodeEffectSettings {
            max_order: STEAM_AMBISONICS_ORDER,
            speaker_layout: SpeakerLayout::Stereo,
            hrtf,
            rendering: Rendering::Binaural,
        },
    )
    .map_err(|e| {
        PetalSonicError::SpatialAudio(format!("Failed to create AmbisonicsDecodeEffect: {}", e))
    })
}

fn load_native_hrtf_table(
    sample_rate: u32,
    native_hrtf_path: Option<&str>,
) -> Result<Arc<NativeHrtfTable>> {
    let path = native_hrtf_path.ok_or_else(|| {
        PetalSonicError::Configuration(
            "native HRTF backend requires native_hrtf_path or hrtf_path to point to a .petalhrtf file"
                .to_string(),
        )
    })?;
    let table = NativeHrtfTable::from_petalhrtf_file(path)?;
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

fn petal_to_steam_hrtf_direction(direction: Vec3) -> Direction {
    // PetalSonic native HRTF uses listener-local z=front. Steam Audio's HRTF API uses
    // right-handed coordinates where -z is ahead, so flip z for apples-to-apples SOFA lookup.
    Direction::new(direction.x, direction.y, -direction.z)
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

    #[test]
    fn steam_hrtf_direction_uses_steam_ahead_axis() {
        let direction = petal_to_steam_hrtf_direction(Vec3::new(0.25, 0.5, 1.0));
        assert_eq!(direction, Direction::new(0.25, 0.5, -1.0));
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
}
