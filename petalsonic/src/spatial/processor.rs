use super::native_ambisonics::{
    DEFAULT_NATIVE_AMBISONICS_ORDER, NativeAmbisonicsBinauralDecoder,
    NativeAmbisonicsBinauralState, NativeAmbisonicsEncoder, native_ambisonics_channel_count,
};
use super::native_hrtf::{
    NativeHrtfRenderMetrics, NativeHrtfRenderer, NativeHrtfSourceState, NativeHrtfTable,
};
use crate::acoustics::{
    AcousticHit, AcousticRay, BatchedAnyHitRayTracer, BatchedClosestHitRayTracer,
};
use crate::config::{AmbisonicsBackend, DirectPathBackend, HrtfBackend, SourceConfig};
use crate::error::{PetalSonicError, Result};
use crate::gain;
use crate::math::{Pose, Vec3};
use crate::playback::PlaybackInstance;
use crate::spatial::effects::SpatialEffectsManager;
use crate::spatial::hrtf;
use crate::world::SourceId;
use audionimbus::{
    AirAbsorptionModel, AmbisonicsDecodeEffect, AmbisonicsDecodeEffectParams,
    AmbisonicsDecodeEffectSettings, AmbisonicsEncodeEffectParams, AnyHitCallback,
    AudioBufferSettings, AudioSettings, BatchedAnyHitCallback, BatchedClosestHitCallback,
    BinauralEffectParams, ClosestHitCallback, Context, CoordinateSystem, CustomRayTracer, Direct,
    DirectEffectParams, DirectSimulationParameters, DirectSimulationSettings, Direction,
    DistanceAttenuationModel, Equalizer, Hit, Hrtf, HrtfInterpolation, Material, Occlusion,
    OcclusionAlgorithm, Point, Reflections, ReflectionsSharedInputs,
    ReflectionsSimulationParameters, ReflectionsSimulationSettings, Rendering, Scene,
    SimulationFlags, SimulationInputs, SimulationSettings, SimulationSharedInputs, Simulator,
    SpeakerLayout, Transmission, Vector3, audio_buffer::AudioBuffer as AudioNimbusAudioBuffer,
    callback::CustomRayTracingCallbacks,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

const STEAM_AMBISONICS_ORDER: u32 = 2;
const REFLECTIONS_ORDER: u32 = 1;
const REFLECTIONS_MAX_NUM_RAYS: u32 = 1;
const REFLECTIONS_NUM_DIFFUSE_SAMPLES: u32 = 1;
const REFLECTIONS_DURATION_SECONDS: f32 = 0.1;
const REFLECTIONS_MAX_NUM_SOURCES: u32 = 1;
const REFLECTIONS_NUM_THREADS: u32 = 1;
const REFLECTIONS_NUM_BOUNCES: u32 = 1;
const REFLECTIONS_IRRADIANCE_MIN_DISTANCE: f32 = 1.0;
const SPEED_OF_SOUND_METERS_PER_SECOND: f32 = 343.0;
const NATIVE_EARLY_REFLECTION_MAX_DELAY_SECONDS: f32 = 0.25;
const NATIVE_EARLY_REFLECTION_MAX_TRACE_METERS: f32 = 120.0;
const NATIVE_EARLY_REFLECTION_GAIN: f32 = 0.18;

/// Host-provided direct-path override applied on top of Steam Audio simulation output.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct DirectPathOverride {
    pub occlusion: Option<f32>,
    pub transmission: Option<DirectPathTransmission>,
}

/// Transmission override for the direct sound path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DirectPathTransmission {
    FrequencyIndependent([f32; 3]),
    FrequencyDependent([f32; 3]),
}

type SpatialSimulator = Simulator<'static, CustomRayTracer, Direct, Reflections>;
type SpatialScene = Scene<'static, CustomRayTracer>;

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

/// Spatial audio processor that manages Steam Audio integration
pub struct SpatialProcessor {
    // Steam Audio core objects
    context: Context,
    simulator: SpatialSimulator,
    #[allow(dead_code)] // Must be kept alive for simulator lifetime
    scene: SpatialScene,
    hrtf: Option<Hrtf>,

    // Shared ambisonics decode effect (used for all sources in the Steam Audio HRTF path)
    ambisonics_decode_effect: Option<AmbisonicsDecodeEffect>,

    // Native HRTF/Ambisonics renderer and delay state
    native_hrtf_renderer: Option<NativeHrtfRenderer>,
    native_hrtf_source_states: HashMap<SourceId, NativeHrtfSourceState>,
    native_ambisonics_encoder: NativeAmbisonicsEncoder,
    native_ambisonics_decoder: Option<NativeAmbisonicsBinauralDecoder>,
    native_ambisonics_state: Option<NativeAmbisonicsBinauralState>,
    native_reflection_source_states: HashMap<SourceId, NativeEarlyReflectionSourceState>,

    // Per-source effects management
    effects_manager: SpatialEffectsManager,

    // Configuration
    frame_size: usize,
    sample_rate: u32,
    distance_scaler: f32,
    hrtf_backend: HrtfBackend,
    direct_path_backend: DirectPathBackend,
    use_ambisonics: bool,
    ambisonics_backend: AmbisonicsBackend,
    direct_occlusion_enabled: bool,
    reflections_enabled: bool,
    native_early_reflections_enabled: bool,
    native_any_hit_ray_tracer: Option<Arc<dyn BatchedAnyHitRayTracer>>,
    native_closest_hit_ray_tracer: Option<Arc<dyn BatchedClosestHitRayTracer>>,
    /// HRTF gain in decibels (for introspection / debugging).
    #[allow(dead_code)] // Only used for debugging / future introspection
    hrtf_gain_db: f32,
    /// HRTF gain as linear multiplier (derived from `hrtf_gain_db`).
    hrtf_gain_linear: f32,

    // Cached buffers to avoid allocations
    cached_input_buf: Vec<f32>,             // Input mono samples
    cached_direct_buf: Vec<f32>,            // After DirectEffect
    cached_reflections_buf: Vec<f32>,       // Reflections ambisonics output
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
    direct_debug: DirectDebugStats,
    latest_direct_snapshot: Option<DirectOcclusionDebugSnapshot>,
}

struct DirectDebugStats {
    sample_count: usize,
    occlusion_sum: f32,
    occlusion_min: f32,
    occlusion_max: f32,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DirectOcclusionDebugSnapshot {
    pub sample_count: usize,
    pub avg_occlusion: f32,
    pub min_occlusion: f32,
    pub max_occlusion: f32,
}

impl Default for DirectDebugStats {
    fn default() -> Self {
        Self {
            sample_count: 0,
            occlusion_sum: 0.0,
            occlusion_min: f32::INFINITY,
            occlusion_max: f32::NEG_INFINITY,
        }
    }
}

impl DirectDebugStats {
    fn snapshot(&self) -> Option<DirectOcclusionDebugSnapshot> {
        if self.sample_count == 0 {
            return None;
        }

        Some(DirectOcclusionDebugSnapshot {
            sample_count: self.sample_count,
            avg_occlusion: self.occlusion_sum / self.sample_count as f32,
            min_occlusion: self.occlusion_min,
            max_occlusion: self.occlusion_max,
        })
    }

    fn clear_samples(&mut self) {
        self.sample_count = 0;
        self.occlusion_sum = 0.0;
        self.occlusion_min = f32::INFINITY;
        self.occlusion_max = f32::NEG_INFINITY;
    }
}

/// Detailed timing metrics captured for a single spatial processing pass.
#[derive(Debug, Default, Clone, Copy)]
pub struct SpatialProcessingMetrics {
    /// Number of spatial sources considered by this processing pass.
    pub spatial_source_count: usize,
    /// Time spent running the spatial simulation (Steam Audio) step.
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

/// Summary of spatial processing output including the number of frames produced.
#[derive(Debug, Default, Clone, Copy)]
pub struct SpatialProcessingSummary {
    pub frames_processed: usize,
    pub metrics: SpatialProcessingMetrics,
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
    /// Create a new spatial processor
    ///
    /// # Arguments
    /// * `sample_rate` - Sample rate for audio processing
    /// * `frame_size` - Number of frames to process per call
    /// * `distance_scaler` - Scale factor to convert game units to meters (default: 10.0)
    /// * `hrtf_gain` - HRTF gain compensation in decibels (default: 0.0 dB = no change)
    pub fn new(
        sample_rate: u32,
        frame_size: usize,
        distance_scaler: f32,
        steam_hrtf_path: Option<&str>,
        native_hrtf_path: Option<&str>,
        hrtf_gain: f32,
        hrtf_backend: HrtfBackend,
        direct_path_backend: DirectPathBackend,
        use_ambisonics: bool,
        ambisonics_backend: AmbisonicsBackend,
        batched_any_hit_ray_tracer: Option<Arc<dyn BatchedAnyHitRayTracer>>,
        batched_closest_hit_ray_tracer: Option<Arc<dyn BatchedClosestHitRayTracer>>,
    ) -> Result<Self> {
        // Create Steam Audio context
        let context = Context::try_new(&audionimbus::ContextSettings::default()).map_err(|e| {
            PetalSonicError::SpatialAudio(format!("Failed to create Steam Audio context: {}", e))
        })?;

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
            let loaded_hrtf = create_steam_hrtf(&context, &audio_settings, steam_hrtf_path)?;
            ambisonics_decode_effect = Some(create_ambisonics_decode_effect(
                &context,
                &audio_settings,
                &loaded_hrtf,
            )?);
            hrtf = Some(loaded_hrtf);
        }

        if hrtf_backend == HrtfBackend::Native {
            let table = load_native_hrtf_table(sample_rate, native_hrtf_path)?;
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

        // Create simulator
        // The max order is unused for now, just a placeholder.
        let simulation_settings = SimulationSettings::new(sample_rate, frame_size as u32, 4)
            .with_custom_ray_tracer(256)
            .with_reflections(ReflectionsSimulationSettings::Convolution {
                max_num_rays: REFLECTIONS_MAX_NUM_RAYS,
                num_diffuse_samples: REFLECTIONS_NUM_DIFFUSE_SAMPLES,
                max_duration: REFLECTIONS_DURATION_SECONDS,
                max_num_sources: REFLECTIONS_MAX_NUM_SOURCES,
                num_threads: REFLECTIONS_NUM_THREADS,
            })
            .with_direct(DirectSimulationSettings {
                max_num_occlusion_samples: 32,
            });

        let mut simulator: SpatialSimulator = Simulator::try_new(&context, &simulation_settings)
            .map_err(|e| {
                PetalSonicError::SpatialAudio(format!("Failed to create simulator: {}", e))
            })?;
        let direct_occlusion_enabled = batched_any_hit_ray_tracer.is_some();
        let reflections_enabled =
            batched_closest_hit_ray_tracer.is_some() && hrtf_backend == HrtfBackend::SteamAudio;
        let native_early_reflections_enabled = batched_closest_hit_ray_tracer.is_some()
            && hrtf_backend == HrtfBackend::Native
            && !use_ambisonics;

        // Create scene
        let any_hit_backend = batched_any_hit_ray_tracer.clone();
        let any_hit_single_callback =
            AnyHitCallback::new(move |ray, min_distance, max_distance| {
                let Some(backend) = &any_hit_backend else {
                    return false;
                };

                let inv_distance_scaler = if distance_scaler != 0.0 {
                    1.0 / distance_scaler
                } else {
                    1.0
                };

                let results = backend.trace_any_hit_batch(
                    &[AcousticRay {
                        origin: Vec3::new(
                            ray.origin.x * inv_distance_scaler,
                            ray.origin.y * inv_distance_scaler,
                            ray.origin.z * inv_distance_scaler,
                        ),
                        direction: Vec3::new(ray.direction.x, ray.direction.y, ray.direction.z),
                    }],
                    &[min_distance * inv_distance_scaler],
                    &[max_distance * inv_distance_scaler],
                );

                results.into_iter().next().unwrap_or(false)
            });

        let any_hit_backend = batched_any_hit_ray_tracer.clone();
        let any_hit_callback =
            BatchedAnyHitCallback::new(move |rays, min_distances, max_distances| {
                let Some(backend) = &any_hit_backend else {
                    return vec![false; rays.len()];
                };

                let inv_distance_scaler = if distance_scaler != 0.0 {
                    1.0 / distance_scaler
                } else {
                    1.0
                };

                let acoustic_rays = rays
                    .iter()
                    .map(|ray| AcousticRay {
                        origin: Vec3::new(
                            ray.origin.x * inv_distance_scaler,
                            ray.origin.y * inv_distance_scaler,
                            ray.origin.z * inv_distance_scaler,
                        ),
                        direction: Vec3::new(ray.direction.x, ray.direction.y, ray.direction.z),
                    })
                    .collect::<Vec<_>>();

                let min_distances = min_distances
                    .iter()
                    .map(|distance| distance * inv_distance_scaler)
                    .collect::<Vec<_>>();
                let max_distances = max_distances
                    .iter()
                    .map(|distance| distance * inv_distance_scaler)
                    .collect::<Vec<_>>();

                backend.trace_any_hit_batch(&acoustic_rays, &min_distances, &max_distances)
            });

        let closest_hit_backend = batched_closest_hit_ray_tracer.clone();
        let closest_hit_single_callback =
            ClosestHitCallback::new(move |ray, min_distance, max_distance| {
                let Some(backend) = &closest_hit_backend else {
                    return Some(no_hit());
                };

                let inv_distance_scaler = if distance_scaler != 0.0 {
                    1.0 / distance_scaler
                } else {
                    1.0
                };

                Some(
                    backend
                        .trace_closest_hit_batch(
                            &[AcousticRay {
                                origin: Vec3::new(
                                    ray.origin.x * inv_distance_scaler,
                                    ray.origin.y * inv_distance_scaler,
                                    ray.origin.z * inv_distance_scaler,
                                ),
                                direction: Vec3::new(
                                    ray.direction.x,
                                    ray.direction.y,
                                    ray.direction.z,
                                ),
                            }],
                            &[min_distance * inv_distance_scaler],
                            &[max_distance * inv_distance_scaler],
                        )
                        .into_iter()
                        .next()
                        .flatten()
                        .map_or_else(no_hit, acoustic_hit_to_audionimbus_hit),
                )
            });

        let closest_hit_backend = batched_closest_hit_ray_tracer.clone();
        let closest_hit_callback =
            BatchedClosestHitCallback::new(move |rays, min_distances, max_distances| {
                let Some(backend) = &closest_hit_backend else {
                    return vec![no_hit(); rays.len()].into_iter().map(Some).collect();
                };

                let inv_distance_scaler = if distance_scaler != 0.0 {
                    1.0 / distance_scaler
                } else {
                    1.0
                };

                let acoustic_rays = rays
                    .iter()
                    .map(|ray| AcousticRay {
                        origin: Vec3::new(
                            ray.origin.x * inv_distance_scaler,
                            ray.origin.y * inv_distance_scaler,
                            ray.origin.z * inv_distance_scaler,
                        ),
                        direction: Vec3::new(ray.direction.x, ray.direction.y, ray.direction.z),
                    })
                    .collect::<Vec<_>>();

                let min_distances = min_distances
                    .iter()
                    .map(|distance| distance * inv_distance_scaler)
                    .collect::<Vec<_>>();
                let max_distances = max_distances
                    .iter()
                    .map(|distance| distance * inv_distance_scaler)
                    .collect::<Vec<_>>();

                backend
                    .trace_closest_hit_batch(&acoustic_rays, &min_distances, &max_distances)
                    .into_iter()
                    .map(|hit| Some(hit.map_or_else(no_hit, acoustic_hit_to_audionimbus_hit)))
                    .collect()
            });

        let ray_callbacks = Box::leak(Box::new(CustomRayTracingCallbacks::new(
            closest_hit_single_callback,
            any_hit_single_callback,
            closest_hit_callback,
            any_hit_callback,
        )));

        let scene = Scene::try_with_custom(&context, ray_callbacks)
            .map_err(|e| PetalSonicError::SpatialAudio(format!("Failed to create scene: {}", e)))?;

        simulator.set_scene(&scene);
        simulator.commit(); // Must be called after set_scene

        // Pre-allocate buffers
        let cached_input_buf = vec![0.0; frame_size];
        let cached_direct_buf = vec![0.0; frame_size];
        let ambisonics_channel_count =
            native_ambisonics_channel_count(DEFAULT_NATIVE_AMBISONICS_ORDER)?;
        let cached_reflections_buf = vec![0.0; frame_size * ambisonics_channel_count];
        let cached_summed_encoded_buf = vec![0.0; frame_size * ambisonics_channel_count];
        let cached_ambisonics_encode_buf = vec![0.0; frame_size * ambisonics_channel_count];
        let cached_ambisonics_decode_buf = vec![0.0; frame_size * 2]; // Planar stereo
        let cached_binaural_processed = vec![0.0; frame_size * 2];
        let cached_native_reflection_buf = vec![0.0; frame_size];

        // Pre-compute HRTF gain in linear space for efficient application.
        let hrtf_gain_db = hrtf_gain;
        let hrtf_gain_linear = gain::db_to_linear(hrtf_gain_db);

        log::info!(
            "PetalSonic spatial processor: hrtf_backend={:?}, direct_path_backend={:?}, use_ambisonics={}, ambisonics_backend={:?}, direct_occlusion_enabled={}, reflections_enabled={}, native_early_reflections_enabled={}",
            hrtf_backend,
            direct_path_backend,
            use_ambisonics,
            ambisonics_backend,
            direct_occlusion_enabled,
            reflections_enabled,
            native_early_reflections_enabled
        );

        Ok(Self {
            context,
            simulator,
            scene,
            hrtf,
            ambisonics_decode_effect,
            native_hrtf_renderer,
            native_hrtf_source_states: HashMap::new(),
            native_ambisonics_encoder,
            native_ambisonics_decoder,
            native_ambisonics_state,
            native_reflection_source_states: HashMap::new(),
            effects_manager: SpatialEffectsManager::new(),
            frame_size,
            sample_rate,
            distance_scaler,
            hrtf_backend,
            direct_path_backend,
            use_ambisonics,
            ambisonics_backend,
            direct_occlusion_enabled,
            reflections_enabled,
            native_early_reflections_enabled,
            native_any_hit_ray_tracer: batched_any_hit_ray_tracer,
            native_closest_hit_ray_tracer: batched_closest_hit_ray_tracer,
            hrtf_gain_db,
            hrtf_gain_linear,
            cached_input_buf,
            cached_direct_buf,
            cached_reflections_buf,
            cached_summed_encoded_buf,
            cached_ambisonics_encode_buf,
            cached_ambisonics_decode_buf,
            cached_binaural_processed,
            cached_native_reflection_buf,
            listener_position: Vec3::ZERO,
            listener_up: Vec3::new(0.0, 1.0, 0.0),
            listener_front: Vec3::new(0.0, 0.0, -1.0),
            listener_right: Vec3::new(1.0, 0.0, 0.0),
            direct_debug: DirectDebugStats::default(),
            latest_direct_snapshot: None,
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

    /// Create effects for a spatial source
    pub fn create_effects_for_source(&mut self, source_id: SourceId) -> Result<()> {
        let audio_settings = AudioSettings {
            sampling_rate: self.sample_rate,
            frame_size: self.frame_size as u32,
        };

        self.effects_manager.create_effects_for_source(
            source_id,
            &self.context,
            &mut self.simulator,
            &audio_settings,
            self.hrtf.as_ref(),
        )
    }

    /// Remove effects for a spatial source
    pub fn remove_effects_for_source(&mut self, source_id: SourceId) {
        self.effects_manager.remove_effects_for_source(source_id);
        self.native_hrtf_source_states.remove(&source_id);
        self.native_reflection_source_states.remove(&source_id);
    }

    fn uses_steam_source_effects(&self) -> bool {
        self.direct_path_backend == DirectPathBackend::SteamAudio
            || self.reflections_enabled
            || (self.use_ambisonics && self.ambisonics_backend == AmbisonicsBackend::SteamAudio)
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

    /// Process all spatial sources and output to stereo buffer
    ///
    /// # Arguments
    /// * `instances` - Slice of spatial playback instances to process
    /// * `output_buffer` - Stereo output buffer (interleaved L/R)
    ///
    /// # Returns
    /// Number of frames processed
    pub fn process_spatial_sources(
        &mut self,
        instances: &mut [(SourceId, &mut PlaybackInstance)],
        output_buffer: &mut [f32],
    ) -> Result<usize> {
        Ok(self
            .process_spatial_sources_with_metrics(instances, output_buffer)?
            .frames_processed)
    }

    /// Same as [`process_spatial_sources`] but also returns detailed timing metrics.
    pub fn process_spatial_sources_with_metrics(
        &mut self,
        instances: &mut [(SourceId, &mut PlaybackInstance)],
        output_buffer: &mut [f32],
    ) -> Result<SpatialProcessingSummary> {
        if instances.is_empty() {
            // No spatial sources, don't modify the buffer (may contain non-spatial audio)
            return Ok(SpatialProcessingSummary::default());
        }

        let mut metrics = SpatialProcessingMetrics::default();
        metrics.spatial_source_count = instances
            .iter()
            .filter(|(_, instance)| matches!(instance.config, SourceConfig::Spatial { .. }))
            .count();
        self.direct_debug.clear_samples();
        self.latest_direct_snapshot = None;

        // Ensure all spatial sources have backend state created before processing.
        // This guarantees newly played spatial sources participate in the very first
        // block, avoiding a "first block louder" case where distance attenuation /
        // air absorption would still be at their default values.
        let uses_steam_source_effects = self.uses_steam_source_effects();
        for (source_id, instance) in instances.iter() {
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

        // Run Steam Audio simulation only when a selected backend still consumes Steam Audio
        // source outputs. The fully-native path traces directly against the host scene data.
        if uses_steam_source_effects {
            let simulation_start = Instant::now();
            self.simulate(instances)?;
            metrics.physics_simulation_time_us = simulation_start.elapsed().as_micros() as u64;
        }

        // Process each spatial source and accumulate detailed timing.
        for (source_id, instance) in instances.iter_mut() {
            let source_metrics = self.process_single_source(*source_id, instance)?;
            metrics.direct_processing_time_us += source_metrics.direct_processing_time_us;
            metrics.ambisonics_encoding_time_us += source_metrics.ambisonics_encoding_time_us;
            metrics.hrtf_rendering_time_us += source_metrics.hrtf_rendering_time_us;
            metrics.native_hrtf_direction_lookup_time_us +=
                source_metrics.native_hrtf_direction_lookup_time_us;
            metrics.native_hrtf_convolution_time_us +=
                source_metrics.native_hrtf_convolution_time_us;
        }

        self.latest_direct_snapshot = self.direct_debug.snapshot();

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

        Ok(SpatialProcessingSummary {
            frames_processed: frames_to_copy,
            metrics,
        })
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
        match self.direct_path_backend {
            DirectPathBackend::SteamAudio => {
                self.apply_steam_audio_direct_effect(source_id, instance)?
            }
            DirectPathBackend::Native => self.apply_native_direct_effect(position, instance),
        }
        metrics.direct_processing_time_us = direct_start.elapsed().as_micros() as u64;

        if self.use_ambisonics {
            // Reflections are already ambisonics-encoded, so accumulate them directly.
            self.apply_reflection_effect(source_id)?;
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

    /// Apply Steam Audio's direct effect to the input buffer.
    fn apply_steam_audio_direct_effect(
        &mut self,
        source_id: SourceId,
        instance: &PlaybackInstance,
    ) -> Result<()> {
        let occlusion_for_debug = {
            let effects = self
                .effects_manager
                .get_effects_mut(source_id)
                .ok_or_else(|| {
                    PetalSonicError::SpatialAudio(format!(
                        "No effects found for source {}",
                        source_id
                    ))
                })?;

            // Get simulation results
            let outputs = effects
                .source
                .get_outputs(SimulationFlags::DIRECT)
                .map_err(|e| {
                    PetalSonicError::SpatialAudio(format!("Failed to get direct outputs: {}", e))
                })?;
            let direct_outputs = outputs.direct();

            let distance_attenuation = direct_outputs.distance_attenuation.unwrap_or(1.0);
            let air_absorption = direct_outputs
                .air_absorption
                .as_ref()
                .map(|eq| Equalizer([eq[0], eq[1], eq[2]]))
                .unwrap_or(Equalizer([1.0, 1.0, 1.0]));

            let mut direct_effect_params = DirectEffectParams {
                distance_attenuation: Some(distance_attenuation),
                air_absorption: Some(air_absorption),
                directivity: None,
                // In this path Steam Audio's direct output matches DirectEffect polarity:
                // 1.0 is fully audible, 0.0 is fully occluded.
                occlusion: direct_outputs.occlusion,
                // Phase 1 only supports boolean any-hit occlusion. Keep transmission disabled
                // until closest-hit/material data is wired in Phase 2.
                transmission: None,
            };

            if let Some(direct_path_override) = instance.direct_path_override {
                if let Some(occlusion) = direct_path_override.occlusion {
                    direct_effect_params.occlusion = Some(occlusion);
                }

                if let Some(transmission) = direct_path_override.transmission {
                    direct_effect_params.transmission = Some(match transmission {
                        DirectPathTransmission::FrequencyIndependent(bands) => {
                            Transmission::FrequencyIndependent(Equalizer(bands))
                        }
                        DirectPathTransmission::FrequencyDependent(bands) => {
                            Transmission::FrequencyDependent(Equalizer(bands))
                        }
                    });
                }
            }

            let occlusion_for_debug = direct_effect_params.occlusion;

            let input_buf = AudioNimbusAudioBuffer::try_with_data_and_settings(
                &self.cached_input_buf,
                AudioBufferSettings {
                    num_channels: Some(1),
                    ..Default::default()
                },
            )
            .map_err(|e| {
                PetalSonicError::SpatialAudio(format!("Failed to create input buffer: {}", e))
            })?;

            let direct_buf = AudioNimbusAudioBuffer::try_with_data_and_settings(
                &mut self.cached_direct_buf,
                AudioBufferSettings {
                    num_channels: Some(1),
                    ..Default::default()
                },
            )
            .map_err(|e| {
                PetalSonicError::SpatialAudio(format!("Failed to create direct buffer: {}", e))
            })?;

            effects
                .direct_effect
                .apply(&direct_effect_params, &input_buf, &direct_buf)
                .map_err(|e| {
                    PetalSonicError::SpatialAudio(format!("Failed to apply DirectEffect: {}", e))
                })?;

            occlusion_for_debug
        };

        self.record_direct_debug_stats(occlusion_for_debug);

        Ok(())
    }

    /// Apply PetalSonic's native direct path to the input buffer.
    fn apply_native_direct_effect(&mut self, source_position: Vec3, instance: &PlaybackInstance) {
        let source_delta = source_position - self.listener_position;
        let distance_world = source_delta.length();
        let distance_meters = distance_world * self.distance_scaler;
        let distance_attenuation = native_distance_attenuation(distance_meters);
        let air_absorption = native_air_absorption(distance_meters);

        let mut occlusion = if self.direct_occlusion_enabled {
            self.native_direct_occlusion(source_position, distance_world)
        } else {
            1.0
        };
        let mut transmission_gain = 0.0;

        if let Some(direct_path_override) = instance.direct_path_override {
            if let Some(override_occlusion) = direct_path_override.occlusion {
                occlusion = override_occlusion.clamp(0.0, 1.0);
            }

            if let Some(transmission) = direct_path_override.transmission {
                transmission_gain = native_transmission_gain(transmission);
            }
        }

        let audibility = occlusion.max((1.0 - occlusion) * transmission_gain);
        let direct_gain = distance_attenuation * air_absorption * audibility;

        self.cached_direct_buf.fill(0.0);
        for (output, input) in self
            .cached_direct_buf
            .iter_mut()
            .zip(self.cached_input_buf.iter())
        {
            *output = *input * direct_gain;
        }

        self.record_direct_debug_stats(Some(occlusion));
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

        let is_occluded = ray_tracer
            .trace_any_hit_batch(&rays, &min_distances, &max_distances)
            .into_iter()
            .next()
            .unwrap_or(false);

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
        let hit = closest_hit_ray_tracer
            .trace_closest_hit_batch(&rays, &min_distances, &max_distances)
            .into_iter()
            .next()
            .flatten()?;

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
            let blocked = any_hit_ray_tracer
                .trace_any_hit_batch(&visibility_rays, &visibility_min, &visibility_max)
                .into_iter()
                .next()
                .unwrap_or(false);
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

    fn record_direct_debug_stats(&mut self, occlusion: Option<f32>) {
        let Some(occlusion) = occlusion else {
            return;
        };

        self.direct_debug.sample_count += 1;
        self.direct_debug.occlusion_sum += occlusion;
        self.direct_debug.occlusion_min = self.direct_debug.occlusion_min.min(occlusion);
        self.direct_debug.occlusion_max = self.direct_debug.occlusion_max.max(occlusion);
    }

    pub fn direct_occlusion_debug_snapshot(&self) -> Option<DirectOcclusionDebugSnapshot> {
        self.latest_direct_snapshot
    }

    fn apply_reflection_effect(&mut self, source_id: SourceId) -> Result<()> {
        if !self.reflections_enabled {
            return Ok(());
        }

        self.cached_reflections_buf.fill(0.0);

        let effects = self
            .effects_manager
            .get_effects_mut(source_id)
            .ok_or_else(|| {
                PetalSonicError::SpatialAudio(format!("No effects found for source {}", source_id))
            })?;

        let outputs = effects
            .source
            .get_outputs(SimulationFlags::REFLECTIONS)
            .map_err(|e| {
                PetalSonicError::SpatialAudio(format!("Failed to get reflection outputs: {}", e))
            })?;
        let reflection_params = outputs.reflections::<audionimbus::Convolution>();

        let input_buf = AudioNimbusAudioBuffer::try_with_data_and_settings(
            &self.cached_input_buf,
            AudioBufferSettings {
                num_channels: Some(1),
                ..Default::default()
            },
        )
        .map_err(|e| {
            PetalSonicError::SpatialAudio(format!(
                "Failed to create reflection input buffer: {}",
                e
            ))
        })?;

        let reflection_channel_count = native_ambisonics_channel_count(STEAM_AMBISONICS_ORDER)?;
        let reflection_sample_count = self.frame_size * reflection_channel_count;
        let output_buf = AudioNimbusAudioBuffer::try_with_data_and_settings(
            &mut self.cached_reflections_buf[..reflection_sample_count],
            AudioBufferSettings {
                num_channels: Some(reflection_channel_count as u32),
                ..Default::default()
            },
        )
        .map_err(|e| {
            PetalSonicError::SpatialAudio(format!(
                "Failed to create reflection output buffer: {}",
                e
            ))
        })?;

        effects
            .reflection_effect
            .apply(&reflection_params, &input_buf, &output_buf)
            .map_err(|e| {
                PetalSonicError::SpatialAudio(format!("Failed to apply ReflectionEffect: {}", e))
            })?;

        for i in 0..reflection_sample_count {
            self.cached_summed_encoded_buf[i] += self.cached_reflections_buf[i];
        }

        Ok(())
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

    /// Run the Steam Audio simulation paths that are still enabled.
    fn simulate(&mut self, instances: &[(SourceId, &mut PlaybackInstance)]) -> Result<()> {
        let run_steam_direct = self.direct_path_backend == DirectPathBackend::SteamAudio;
        if !run_steam_direct && !self.reflections_enabled {
            return Ok(());
        }

        let simulation_flags = match (run_steam_direct, self.reflections_enabled) {
            (true, true) => SimulationFlags::DIRECT | SimulationFlags::REFLECTIONS,
            (true, false) => SimulationFlags::DIRECT,
            (false, true) => SimulationFlags::REFLECTIONS,
            (false, false) => unreachable!(),
        };

        // Set simulation inputs for each source
        for (source_id, instance) in instances.iter() {
            let position = match &instance.config {
                SourceConfig::Spatial { pose, .. } => pose.position,
                _ => continue,
            };

            let scaled_position = position * self.distance_scaler;
            let mut direct_params = DirectSimulationParameters::new()
                .with_distance_attenuation(DistanceAttenuationModel::Default)
                .with_air_absorption(AirAbsorptionModel::Default);

            if self.direct_occlusion_enabled {
                direct_params =
                    direct_params.with_occlusion(Occlusion::new(OcclusionAlgorithm::Raycast));
            }

            let simulation_inputs = SimulationInputs::new(CoordinateSystem {
                origin: Point::new(scaled_position.x, scaled_position.y, scaled_position.z),
                ..Default::default()
            })
            .with_direct(direct_params)
            .with_reflections(ReflectionsSimulationParameters::Convolution {
                baked_data_identifier: None,
            });

            // Get the source and set inputs - need mutable access
            if let Some(effects) = self.effects_manager.get_effects_mut(*source_id) {
                effects
                    .source
                    .set_inputs(simulation_flags, simulation_inputs)
                    .map_err(|e| {
                        PetalSonicError::SpatialAudio(format!(
                            "Failed to set source simulation inputs: {}",
                            e
                        ))
                    })?;
            }
        }

        self.simulator.commit();

        // Set shared listener inputs
        let scaled_listener_position = self.listener_position * self.distance_scaler;
        let simulation_shared_inputs = SimulationSharedInputs::new(CoordinateSystem {
            origin: Point::new(
                scaled_listener_position.x,
                scaled_listener_position.y,
                scaled_listener_position.z,
            ),
            right: Vector3::new(
                self.listener_right.x,
                self.listener_right.y,
                self.listener_right.z,
            ),
            up: Vector3::new(self.listener_up.x, self.listener_up.y, self.listener_up.z),
            ahead: Vector3::new(
                self.listener_front.x,
                self.listener_front.y,
                self.listener_front.z,
            ),
        })
        .with_reflections(ReflectionsSharedInputs {
            num_rays: REFLECTIONS_MAX_NUM_RAYS,
            num_bounces: REFLECTIONS_NUM_BOUNCES,
            duration: REFLECTIONS_DURATION_SECONDS,
            order: REFLECTIONS_ORDER,
            irradiance_min_distance: REFLECTIONS_IRRADIANCE_MIN_DISTANCE,
        });

        self.simulator
            .set_shared_inputs(simulation_flags, &simulation_shared_inputs)
            .map_err(|e| {
                PetalSonicError::SpatialAudio(format!("Failed to set shared inputs: {}", e))
            })?;

        if run_steam_direct {
            self.simulator.run_direct();
        }

        if self.reflections_enabled {
            self.simulator.run_reflections().map_err(|e| {
                PetalSonicError::SpatialAudio(format!("Failed to run reflections: {}", e))
            })?;
        }

        Ok(())
    }

    /// Get the frame size
    pub fn frame_size(&self) -> usize {
        self.frame_size
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

fn native_transmission_gain(transmission: DirectPathTransmission) -> f32 {
    let bands = match transmission {
        DirectPathTransmission::FrequencyIndependent(bands)
        | DirectPathTransmission::FrequencyDependent(bands) => bands,
    };

    ((bands[0] + bands[1] + bands[2]) / 3.0).clamp(0.0, 1.0)
}

fn no_hit() -> Hit {
    Hit {
        distance: f32::INFINITY,
        triangle_index: None,
        object_index: None,
        material_index: None,
        normal: Vector3::new(0.0, 1.0, 0.0),
        material: None,
    }
}

fn acoustic_hit_to_audionimbus_hit(hit: AcousticHit) -> Hit {
    Hit {
        distance: hit.distance,
        triangle_index: None,
        object_index: None,
        material_index: None,
        normal: Vector3::new(hit.normal.x, hit.normal.y, hit.normal.z),
        material: Some(Material {
            absorption: hit.material.absorption,
            scattering: hit.material.scattering,
            transmission: hit.material.transmission,
        }),
    }
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

    #[test]
    fn native_transmission_gain_averages_and_clamps_bands() {
        let gain = native_transmission_gain(DirectPathTransmission::FrequencyIndependent([
            0.25, 0.5, 1.25,
        ]));
        assert!((gain - (2.0 / 3.0)).abs() < 1e-6);

        let clamped =
            native_transmission_gain(DirectPathTransmission::FrequencyDependent([2.0, 2.0, 2.0]));
        assert_eq!(clamped, 1.0);
    }
}
