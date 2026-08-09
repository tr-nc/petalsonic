//! Event types for PetalSonic

use crate::domain::{Emitter, PlaybackControl, PlaybackTag};

/// Timing information for a single render iteration
/// Used for performance profiling and stress testing
#[derive(Debug, Clone, Copy)]
pub struct RenderTimingEvent {
    /// Time spent mixing audio sources (microseconds)
    pub mixing_time_us: u64,
    /// Time spent on spatial processing (microseconds)
    pub spatial_time_us: u64,
    /// Time spent mixing non-spatial (direct) sources (microseconds)
    pub direct_mixing_time_us: u64,
    /// Number of spatial sources processed.
    pub spatial_source_count: usize,
    /// Time spent running the spatial physics simulation step (microseconds)
    pub spatial_simulation_time_us: u64,
    /// Time spent applying direct-path processing (microseconds)
    pub direct_processing_time_us: u64,
    /// Time spent encoding sources into the ambisonics field (microseconds)
    pub ambisonics_encoding_time_us: u64,
    /// Time spent decoding ambisonics back to listener channels (microseconds)
    pub ambisonics_decoding_time_us: u64,
    /// Time spent rendering HRTF/binaural output (microseconds)
    pub hrtf_rendering_time_us: u64,
    /// Time spent selecting native HRTF directions (microseconds)
    pub native_hrtf_direction_lookup_time_us: u64,
    /// Time spent in native HRTF FIR convolution (microseconds)
    pub native_hrtf_convolution_time_us: u64,
    /// Time spent on resampling (microseconds)
    pub resampling_time_us: u64,
    /// Total time for the entire render iteration (microseconds)
    pub total_time_us: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PetalSonicEvent {
    /// A controlled one-shot reached its natural end.
    PlaybackCompleted {
        emitter: Emitter,
        control: PlaybackControl,
        tag: PlaybackTag,
    },
}
