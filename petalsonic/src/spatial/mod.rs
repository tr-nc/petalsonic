// Spatial audio module
//
// This module provides Steam Audio integration for 3D spatial audio processing.
// It includes effect management, HRTF loading, and the main spatial processor.

mod effects;
mod hrtf;
mod native_ambisonics;
mod native_hrtf;
mod processor;

// Public API
pub use native_ambisonics::{
    DEFAULT_NATIVE_AMBISONICS_ORDER, NativeAmbisonicsBinauralDecoder,
    NativeAmbisonicsBinauralState, NativeAmbisonicsEncoder, native_ambisonics_channel_count,
};
pub use native_hrtf::{
    NativeHrtfDirection, NativeHrtfRenderMetrics, NativeHrtfRenderer, NativeHrtfSourceState,
    NativeHrtfTable,
};
pub use processor::{
    DirectOcclusionDebugSnapshot, DirectPathOverride, DirectPathTransmission,
    SpatialProcessingMetrics, SpatialProcessingSummary, SpatialProcessor,
};
