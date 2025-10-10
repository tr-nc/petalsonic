// Spatial audio module
//
// This module provides Steam Audio integration for 3D spatial audio processing.
// It includes effect management, HRTF loading, and the main spatial processor.

mod effects;
mod hrtf;
mod processor;

// Public API
pub use processor::SpatialProcessor;
