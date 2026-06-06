// Spatial audio module
//
// This module provides Steam Audio integration for 3D spatial audio processing.
// It includes effect management, HRTF loading, and the main spatial processor.

mod effects;
mod hrtf;
mod native_hrtf;
mod processor;

// Public API
pub use native_hrtf::{
    NativeHrtfDirection, NativeHrtfRenderer, NativeHrtfSourceState, NativeHrtfTable,
};
pub use processor::{
    DirectOcclusionDebugSnapshot, DirectPathOverride, DirectPathTransmission,
    SpatialProcessingMetrics, SpatialProcessingSummary, SpatialProcessor,
};
