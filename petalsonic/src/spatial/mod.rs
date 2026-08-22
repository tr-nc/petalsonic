// Native spatial audio processing and HRTF rendering.

mod late_reverb;
mod native_ambisonics;
mod native_hrtf;
mod processor;

pub(crate) use processor::{
    RetiredSpatialSource, SpatialProcessingMetrics, SpatialProcessor, SpatialProcessorConfig,
};
