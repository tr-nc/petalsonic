// Spatial audio module
//
// This module provides Steam Audio integration for 3D spatial audio processing.
// It includes effect management, HRTF loading, and the main spatial processor.

mod effects;
mod hrtf;
mod native_ambisonics;
mod native_hrtf;
mod processor;

pub use processor::{DirectPathOverride, DirectPathTransmission};
pub(crate) use processor::{SpatialProcessingMetrics, SpatialProcessor, SpatialProcessorConfig};
