use crate::acoustics::{AcousticRay, BatchedAnyHitRayTracer};
use crate::config::SourceConfig;
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
    ClosestHitCallback, Context, CoordinateSystem, CustomRayTracer, Direct, DirectEffectParams,
    DirectSimulationParameters, DirectSimulationSettings, Direction, DistanceAttenuationModel,
    Equalizer, Hrtf, Occlusion, OcclusionAlgorithm, Point, Rendering, Scene, SimulationFlags,
    SimulationInputs, SimulationSettings, SimulationSharedInputs, Simulator, SpeakerLayout,
    Transmission, Vector3,
    callback::CustomRayTracingCallbacks,
    audio_buffer::AudioBuffer as AudioNimbusAudioBuffer,
};
use std::sync::Arc;
use std::time::Instant;

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

type SpatialSimulator = Simulator<'static, CustomRayTracer, Direct>;
type SpatialScene = Scene<'static, CustomRayTracer>;

/// Spatial audio processor that manages Steam Audio integration
pub struct SpatialProcessor {
    // Steam Audio core objects
    context: Context,
    simulator: SpatialSimulator,
    #[allow(dead_code)] // Must be kept alive for simulator lifetime
    scene: SpatialScene,
    hrtf: Hrtf,

    // Shared ambisonics decode effect (used for all sources)
    ambisonics_decode_effect: AmbisonicsDecodeEffect,

    // Per-source effects management
    effects_manager: SpatialEffectsManager,

    // Configuration
    frame_size: usize,
    sample_rate: u32,
    distance_scaler: f32,
    /// HRTF gain in decibels (for introspection / debugging).
    #[allow(dead_code)] // Only used for debugging / future introspection
    hrtf_gain_db: f32,
    /// HRTF gain as linear multiplier (derived from `hrtf_gain_db`).
    hrtf_gain_linear: f32,

    // Cached buffers to avoid allocations
    cached_input_buf: Vec<f32>,             // Input mono samples
    cached_direct_buf: Vec<f32>,            // After DirectEffect
    cached_summed_encoded_buf: Vec<f32>,    // Accumulated ambisonics (9 channels for order 2)
    cached_ambisonics_encode_buf: Vec<f32>, // Temp buffer for encoding
    cached_ambisonics_decode_buf: Vec<f32>, // After AmbisonicsDecode (stereo)
    cached_binaural_processed: Vec<f32>,    // Final binaural output (interleaved stereo)

    // Listener state
    listener_position: Vec3,
    listener_up: Vec3,
    listener_front: Vec3,
    listener_right: Vec3,
    direct_debug: DirectDebugStats,
    latest_direct_snapshot: Option<DirectOcclusionDebugSnapshot>,
}

struct DirectDebugStats {
    last_log_at: Instant,
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
            last_log_at: Instant::now(),
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
    /// Time spent running the spatial simulation (Steam Audio) step.
    pub physics_simulation_time_us: u64,
    /// Time spent encoding all spatial sources into the ambisonics field.
    pub ambisonics_encoding_time_us: u64,
    /// Time spent decoding ambisonics data back to listener channels.
    pub ambisonics_decoding_time_us: u64,
}

/// Summary of spatial processing output including the number of frames produced.
#[derive(Debug, Default, Clone, Copy)]
pub struct SpatialProcessingSummary {
    pub frames_processed: usize,
    pub metrics: SpatialProcessingMetrics,
}

impl SpatialProcessor {
    /// Create a new spatial processor
    ///
    /// # Arguments
    /// * `sample_rate` - Sample rate for audio processing
    /// * `frame_size` - Number of frames to process per call
    /// * `distance_scaler` - Scale factor to convert game units to meters (default: 10.0)
    /// * `hrtf_path` - Optional path to a custom HRTF SOFA file (None uses default HRTF)
    /// * `hrtf_gain` - HRTF gain compensation in decibels (default: 0.0 dB = no change)
    pub fn new(
        sample_rate: u32,
        frame_size: usize,
        distance_scaler: f32,
        hrtf_path: Option<&str>,
        hrtf_gain: f32,
        batched_any_hit_ray_tracer: Option<Arc<dyn BatchedAnyHitRayTracer>>,
    ) -> Result<Self> {
        // Create Steam Audio context
        let context = Context::try_new(&audionimbus::ContextSettings::default()).map_err(|e| {
            PetalSonicError::SpatialAudio(format!("Failed to create Steam Audio context: {}", e))
        })?;

        let audio_settings = AudioSettings {
            sampling_rate: sample_rate,
            frame_size: frame_size as u32,
        };

        // Create HRTF (custom or default)
        let hrtf = if let Some(path) = hrtf_path {
            hrtf::create_hrtf_from_file(&context, &audio_settings, path)?
        } else {
            hrtf::create_default_hrtf(&context, &audio_settings)?
        };

        // Create ambisonics decode effect (shared across all sources)
        let ambisonics_decode_effect = AmbisonicsDecodeEffect::try_new(
            &context,
            &audio_settings,
            &AmbisonicsDecodeEffectSettings {
                max_order: 2,
                speaker_layout: SpeakerLayout::Stereo,
                hrtf: &hrtf,
                rendering: Rendering::Binaural,
            },
        )
        .map_err(|e| {
            PetalSonicError::SpatialAudio(format!("Failed to create AmbisonicsDecodeEffect: {}", e))
        })?;

        // Create simulator
        // The max order is unused for now, just a placeholder.
        let simulation_settings = SimulationSettings::new(sample_rate, frame_size as u32, 4)
            .with_custom_ray_tracer(256)
            .with_direct(DirectSimulationSettings {
                max_num_occlusion_samples: 32,
            });

        let mut simulator: SpatialSimulator = Simulator::try_new(&context, &simulation_settings)
            .map_err(|e| {
                PetalSonicError::SpatialAudio(format!("Failed to create simulator: {}", e))
            })?;

        // Create scene
        let any_hit_backend = batched_any_hit_ray_tracer.clone();
        let distance_scaler = distance_scaler;
        let any_hit_single_callback = AnyHitCallback::new(move |ray, min_distance, max_distance| {
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
                    direction: Vec3::new(
                        ray.direction.x,
                        ray.direction.y,
                        ray.direction.z,
                    ),
                }],
                &[min_distance * inv_distance_scaler],
                &[max_distance * inv_distance_scaler],
            );

            results.into_iter().next().unwrap_or(false)
        });

        let any_hit_backend = batched_any_hit_ray_tracer.clone();
        let distance_scaler = distance_scaler;
        let any_hit_callback = BatchedAnyHitCallback::new(move |rays, min_distances, max_distances| {
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

        let ray_callbacks = Box::leak(Box::new(CustomRayTracingCallbacks::new(
            ClosestHitCallback::new(|_, _, _| None),
            any_hit_single_callback,
            BatchedClosestHitCallback::new(|rays, _, _| vec![None; rays.len()]),
            any_hit_callback,
        )));

        let scene = Scene::try_with_custom(&context, ray_callbacks)
            .map_err(|e| PetalSonicError::SpatialAudio(format!("Failed to create scene: {}", e)))?;

        simulator.set_scene(&scene);
        simulator.commit(); // Must be called after set_scene

        // Pre-allocate buffers
        let cached_input_buf = vec![0.0; frame_size];
        let cached_direct_buf = vec![0.0; frame_size];
        let cached_summed_encoded_buf = vec![0.0; frame_size * 9]; // 9 channels for order 2
        let cached_ambisonics_encode_buf = vec![0.0; frame_size * 9];
        let cached_ambisonics_decode_buf = vec![0.0; frame_size * 2]; // Stereo
        let cached_binaural_processed = vec![0.0; frame_size * 2];

        // Pre-compute HRTF gain in linear space for efficient application.
        let hrtf_gain_db = hrtf_gain;
        let hrtf_gain_linear = gain::db_to_linear(hrtf_gain_db);

        Ok(Self {
            context,
            simulator,
            scene,
            hrtf,
            ambisonics_decode_effect,
            effects_manager: SpatialEffectsManager::new(),
            frame_size,
            sample_rate,
            distance_scaler,
            hrtf_gain_db,
            hrtf_gain_linear,
            cached_input_buf,
            cached_direct_buf,
            cached_summed_encoded_buf,
            cached_ambisonics_encode_buf,
            cached_ambisonics_decode_buf,
            cached_binaural_processed,
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
        )
    }

    /// Remove effects for a spatial source
    pub fn remove_effects_for_source(&mut self, source_id: SourceId) {
        self.effects_manager.remove_effects_for_source(source_id);
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
        self.direct_debug.clear_samples();
        self.latest_direct_snapshot = None;

        // Ensure all spatial sources have effects created before running simulation.
        // This guarantees newly played spatial sources participate in the very first
        // simulation pass, avoiding a "first block louder" case where distance
        // attenuation / air absorption would still be at their default values.
        for (source_id, instance) in instances.iter() {
            if matches!(instance.config, SourceConfig::Spatial { .. })
                && !self.effects_manager.has_effects(*source_id)
            {
                self.create_effects_for_source(*source_id)?;
            }
        }

        // Clear accumulation buffer
        self.cached_summed_encoded_buf.fill(0.0);
        self.cached_binaural_processed.fill(0.0);

        // Run simulation for all sources
        let simulation_start = Instant::now();
        self.simulate(instances)?;
        metrics.physics_simulation_time_us = simulation_start.elapsed().as_micros() as u64;

        // Process each spatial source and accumulate encoding time
        for (source_id, instance) in instances.iter_mut() {
            metrics.ambisonics_encoding_time_us +=
                self.process_single_source(*source_id, instance)?;
        }

        self.latest_direct_snapshot = self.direct_debug.snapshot();

        // Decode accumulated ambisonics to binaural stereo
        let decoding_start = Instant::now();
        self.apply_ambisonics_decode_effect()?;
        metrics.ambisonics_decoding_time_us = decoding_start.elapsed().as_micros() as u64;

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
    ) -> Result<u64> {
        // Get spatial configuration (position + per-source volume)
        let position = match &instance.config {
            SourceConfig::Spatial { pose, .. } => pose.position,
            _ => return Ok(0), // Not a spatial source, skip
        };

        // Convert dB volume from config to linear gain once per block.
        let volume = instance.config.volume();

        // Fill input buffer with audio samples
        self.fill_input_buffer(instance, volume);

        // Apply direct effect (distance attenuation + air absorption)
        self.apply_direct_effect(source_id, instance)?;

        // Apply ambisonics encode effect and capture timing
        let encode_start = Instant::now();
        self.apply_ambisonics_encode_effect(source_id, position)?;
        let encode_elapsed = encode_start.elapsed().as_micros() as u64;

        Ok(encode_elapsed)
    }

    /// Fill input buffer from playback instance
    fn fill_input_buffer(&mut self, instance: &mut PlaybackInstance, volume: f32) {
        self.cached_input_buf.fill(0.0);

        let samples = instance.audio_data.samples();
        let total_frames = samples.len();
        let current_frame = instance.info.current_frame;

        // Read samples for this block with wraparound support for infinite looping
        for i in 0..self.frame_size {
            let mut sample_idx = current_frame + i;

            // Handle wraparound for infinite looping
            if sample_idx >= total_frames {
                if matches!(instance.loop_mode, crate::playback::LoopMode::Infinite) {
                    // Mark that we reached end (for event emission)
                    if !instance.reached_end_this_iteration {
                        instance.reached_end_this_iteration = true;
                    }
                    // Wrap around to beginning
                    sample_idx %= total_frames;
                } else {
                    // LoopMode::Once - stop reading samples
                    break;
                }
            }

            self.cached_input_buf[i] = samples[sample_idx] * volume;
        }

        // Advance cursor and check for completion with wraparound support
        // This ensures both spatial and non-spatial paths use identical completion logic
        self.advance_instance_with_wrap(instance);
    }

    /// Advance playback instance with wraparound support (for spatial processing)
    fn advance_instance_with_wrap(&mut self, instance: &mut PlaybackInstance) {
        let total_frames = instance.audio_data.samples().len();
        instance.info.current_frame += self.frame_size;

        // Check if we've reached or passed the end
        if instance.info.current_frame >= total_frames {
            match instance.loop_mode {
                crate::playback::LoopMode::Infinite => {
                    // Wrap around - keep playing
                    instance.info.current_frame %= total_frames;
                    // Note: reached_end_this_iteration already set in fill_input_buffer
                    // State remains Playing
                }
                crate::playback::LoopMode::Once => {
                    // Stop playback
                    instance.reached_end_this_iteration = true;
                    instance.info.play_state = crate::playback::PlayState::Stopped;
                }
            }
        }

        instance.info.update_position(
            instance.info.current_frame,
            instance.audio_data.sample_rate(),
        );
    }

    /// Apply direct effect to the input buffer
    fn apply_direct_effect(&mut self, source_id: SourceId, instance: &PlaybackInstance) -> Result<()> {
        let occlusion_for_debug = {
            let effects = self
                .effects_manager
                .get_effects_mut(source_id)
                .ok_or_else(|| {
                    PetalSonicError::SpatialAudio(format!("No effects found for source {}", source_id))
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

    fn record_direct_debug_stats(&mut self, occlusion: Option<f32>) {
        let Some(occlusion) = occlusion else {
            return;
        };

        self.direct_debug.sample_count += 1;
        self.direct_debug.occlusion_sum += occlusion;
        self.direct_debug.occlusion_min = self.direct_debug.occlusion_min.min(occlusion);
        self.direct_debug.occlusion_max = self.direct_debug.occlusion_max.max(occlusion);

        if self.direct_debug.last_log_at.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }

        let avg = if self.direct_debug.sample_count > 0 {
            self.direct_debug.occlusion_sum / self.direct_debug.sample_count as f32
        } else {
            0.0
        };

        log::info!(
            "PetalSonic direct occlusion: samples={} avg={:.3} min={:.3} max={:.3}",
            self.direct_debug.sample_count,
            avg,
            self.direct_debug.occlusion_min,
            self.direct_debug.occlusion_max,
        );
    }

    pub fn direct_occlusion_debug_snapshot(&self) -> Option<DirectOcclusionDebugSnapshot> {
        self.latest_direct_snapshot
    }

    /// Apply ambisonics encode effect
    fn apply_ambisonics_encode_effect(
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
            order: 2,
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

        let output_buf = AudioNimbusAudioBuffer::try_with_data_and_settings(
            &mut self.cached_ambisonics_encode_buf,
            AudioBufferSettings {
                num_channels: Some(9), // Order 2 = 9 channels
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
        for i in 0..self.cached_ambisonics_encode_buf.len() {
            self.cached_summed_encoded_buf[i] += self.cached_ambisonics_encode_buf[i];
        }

        Ok(())
    }

    /// Apply ambisonics decode effect to convert accumulated ambisonics to binaural stereo
    fn apply_ambisonics_decode_effect(&mut self) -> Result<()> {
        let ambisonics_decode_effect_params = AmbisonicsDecodeEffectParams {
            order: 2,
            hrtf: &self.hrtf,
            orientation: CoordinateSystem {
                ahead: Vector3::new(0.0, 0.0, -1.0),
                ..Default::default()
            },
        };

        let input_buf = AudioNimbusAudioBuffer::try_with_data_and_settings(
            &self.cached_summed_encoded_buf,
            AudioBufferSettings {
                num_channels: Some(9),
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

        self.ambisonics_decode_effect
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

    /// Calculate direction from listener to source in listener's coordinate system
    fn get_target_direction(&self, source_position: Vec3) -> Vec3 {
        let target_direction = (source_position - self.listener_position).normalize();
        Vec3::new(
            target_direction.dot(self.listener_right),
            target_direction.dot(self.listener_up),
            target_direction.dot(self.listener_front),
        )
    }

    /// Run Steam Audio simulation for all sources
    fn simulate(&mut self, instances: &[(SourceId, &mut PlaybackInstance)]) -> Result<()> {
        // Set simulation inputs for each source
        for (source_id, instance) in instances.iter() {
            let position = match &instance.config {
                SourceConfig::Spatial { pose, .. } => pose.position,
                _ => continue,
            };

            let scaled_position = position * self.distance_scaler;
            let simulation_inputs = SimulationInputs::new(CoordinateSystem {
                origin: Point::new(scaled_position.x, scaled_position.y, scaled_position.z),
                ..Default::default()
            })
            .with_direct(
                DirectSimulationParameters::new()
                    .with_distance_attenuation(DistanceAttenuationModel::Default)
                    .with_air_absorption(AirAbsorptionModel::Default)
                    .with_occlusion(Occlusion::new(OcclusionAlgorithm::Raycast)),
            );

            // Get the source and set inputs - need mutable access
            if let Some(effects) = self.effects_manager.get_effects_mut(*source_id) {
                effects
                    .source
                    .set_inputs(SimulationFlags::DIRECT, simulation_inputs)
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
        });

        self.simulator
            .set_shared_inputs(SimulationFlags::DIRECT, &simulation_shared_inputs)
            .map_err(|e| {
                PetalSonicError::SpatialAudio(format!("Failed to set shared inputs: {}", e))
            })?;
        self.simulator.run_direct();

        Ok(())
    }

    /// Get the frame size
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }
}
