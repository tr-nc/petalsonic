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
    /// Use Steam Audio HRTF data and binaural rendering.
    #[default]
    SteamAudio,
    /// Use PetalSonic's native `.petalhrtf` table and FIR renderer.
    Native,
}

/// Backend used to encode sources into an Ambisonics field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AmbisonicsBackend {
    /// Use Steam Audio's Ambisonics encode effect.
    #[default]
    SteamAudio,
    /// Use PetalSonic's native real spherical-harmonic encoder.
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
    /// Maximum number of long-lived emitters.
    pub max_emitters: usize,
    /// Maximum number of simultaneous playback voices.
    pub max_voices: usize,
    /// Capacity of the bounded lifecycle/control queue.
    pub control_queue_capacity: usize,
    /// Capacity of the bounded event queue.
    pub event_queue_capacity: usize,
    /// Capacity of the bounded timing/diagnostics queue.
    pub timing_queue_capacity: usize,
    /// Optional case-insensitive substring used to select a CPAL output device by name.
    ///
    /// When `None` or empty, PetalSonic uses the system default output device.
    pub output_device_name_contains: Option<String>,
    /// Optional backend-specific path to HRTF data.
    ///
    /// Prefer [`Self::steam_hrtf_path`] and [`Self::native_hrtf_path`] when both backends may be
    /// used at runtime. This legacy field is still used as a fallback for the initially selected
    /// backend: SOFA for [`HrtfBackend::SteamAudio`], `.petalhrtf` for [`HrtfBackend::Native`].
    pub hrtf_path: Option<String>,
    /// Optional SOFA path used by Steam Audio HRTF rendering.
    pub steam_hrtf_path: Option<String>,
    /// Optional `.petalhrtf` path used by native HRTF rendering.
    pub native_hrtf_path: Option<String>,
    /// Backend used for binaural/HRTF rendering.
    pub hrtf_backend: HrtfBackend,
    /// Whether spatial sources should first be summed into an Ambisonics field before HRTF decode.
    pub use_ambisonics: bool,
    /// Backend used for Ambisonics encoding when [`Self::use_ambisonics`] is true.
    pub ambisonics_backend: AmbisonicsBackend,
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
            max_emitters: 2048,
            max_voices: 4096,
            control_queue_capacity: 4096,
            event_queue_capacity: 1024,
            timing_queue_capacity: 512,
            output_device_name_contains: None,
            hrtf_path: None,
            steam_hrtf_path: None,
            native_hrtf_path: None,
            hrtf_backend: HrtfBackend::default(),
            use_ambisonics: true,
            ambisonics_backend: AmbisonicsBackend::default(),
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
            .field("max_emitters", &self.max_emitters)
            .field("max_voices", &self.max_voices)
            .field("control_queue_capacity", &self.control_queue_capacity)
            .field("event_queue_capacity", &self.event_queue_capacity)
            .field("timing_queue_capacity", &self.timing_queue_capacity)
            .field(
                "output_device_name_contains",
                &self.output_device_name_contains,
            )
            .field("hrtf_path", &self.hrtf_path)
            .field("steam_hrtf_path", &self.steam_hrtf_path)
            .field("native_hrtf_path", &self.native_hrtf_path)
            .field("hrtf_backend", &self.hrtf_backend)
            .field("use_ambisonics", &self.use_ambisonics)
            .field("ambisonics_backend", &self.ambisonics_backend)
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
