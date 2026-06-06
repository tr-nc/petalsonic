use crate::acoustics::{BatchedAnyHitRayTracer, BatchedClosestHitRayTracer};
use std::sync::Arc;

/// Backend used for direct-path spatial processing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DirectPathBackend {
    /// Use Steam Audio simulation and direct effect for distance, air absorption, and occlusion.
    #[default]
    SteamAudio,
    /// Use PetalSonic's native direct path for distance, air absorption, and occlusion.
    Native,
}

/// Backend used for binaural/HRTF rendering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HrtfBackend {
    /// Use Steam Audio ambisonics decode with Steam Audio HRTF data.
    #[default]
    SteamAudio,
    /// Use PetalSonic's native `.petalhrtf` table and FIR renderer.
    Native,
}

/// Configuration descriptor for a PetalSonic world
#[derive(Clone)]
pub struct PetalSonicWorldDesc {
    /// Sample rate for the world processing (may differ from device sample rate)
    pub sample_rate: u32,
    /// Block size in world sample rate (number of frames to generate per audio processing chunk).
    ///
    /// This is the fixed number of frames generated at the world's sample rate, which are then
    /// resampled to the device's sample rate (producing variable output based on the ratio).
    pub block_size: usize,
    /// Number of audio channels (typically 2 for stereo)
    pub channels: u16,
    /// Maximum number of concurrent audio sources
    pub max_sources: usize,
    /// Optional path to HRTF data.
    ///
    /// With [`HrtfBackend::SteamAudio`], this is a SOFA file path and `None` uses Steam Audio's
    /// default HRTF. With [`HrtfBackend::Native`], this must be a `.petalhrtf` file.
    pub hrtf_path: Option<String>,
    /// Backend used for binaural/HRTF rendering.
    pub hrtf_backend: HrtfBackend,
    /// HRTF gain compensation in decibels (default: 0.0 dB = no change)
    ///
    /// Different HRTF datasets can have different overall gain levels.
    /// Use this to compensate for volume differences between HRTFs.
    ///
    /// # Examples
    /// - `0.0`: No change (unity gain)
    /// - `6.0`: Approximately double the perceived loudness (+6 dB)
    /// - `-6.0`: Approximately half the perceived loudness (-6 dB)
    /// - `3.0`: Modest increase (~40% louder)
    /// - `-20.0`: Very quiet (1/10th perceived loudness)
    pub hrtf_gain: f32,
    /// Distance scale factor to convert world units to meters for spatialization.
    ///
    /// Steam Audio operates in meters. This factor controls how your application's
    /// coordinate system maps to real-world meters when running the spatial
    /// simulation.
    ///
    /// - `1.0`: 1 world unit = 1 meter
    /// - `10.0`: 1 world unit = 10 meters (larger-scale worlds)
    pub distance_scaler: f32,
    /// Backend used for direct-path processing.
    pub direct_path_backend: DirectPathBackend,
    /// Optional host-provided batched ray tracing backend for direct acoustics.
    pub batched_any_hit_ray_tracer: Option<Arc<dyn BatchedAnyHitRayTracer>>,
    /// Optional host-provided batched ray tracing backend for closest-hit reflections.
    pub batched_closest_hit_ray_tracer: Option<Arc<dyn BatchedClosestHitRayTracer>>,
}

impl Default for PetalSonicWorldDesc {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            block_size: 1024,
            channels: 2,
            max_sources: 2048,
            hrtf_path: None,
            hrtf_backend: HrtfBackend::default(),
            hrtf_gain: 0.0,
            distance_scaler: 10.0,
            direct_path_backend: DirectPathBackend::default(),
            batched_any_hit_ray_tracer: None,
            batched_closest_hit_ray_tracer: None,
        }
    }
}

impl std::fmt::Debug for PetalSonicWorldDesc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PetalSonicWorldDesc")
            .field("sample_rate", &self.sample_rate)
            .field("block_size", &self.block_size)
            .field("channels", &self.channels)
            .field("max_sources", &self.max_sources)
            .field("hrtf_path", &self.hrtf_path)
            .field("hrtf_backend", &self.hrtf_backend)
            .field("hrtf_gain", &self.hrtf_gain)
            .field("distance_scaler", &self.distance_scaler)
            .field("direct_path_backend", &self.direct_path_backend)
            .field(
                "batched_any_hit_ray_tracer",
                &self.batched_any_hit_ray_tracer.as_ref().map(|_| "<custom>"),
            )
            .field(
                "batched_closest_hit_ray_tracer",
                &self
                    .batched_closest_hit_ray_tracer
                    .as_ref()
                    .map(|_| "<custom>"),
            )
            .finish()
    }
}
