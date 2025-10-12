//! Event types for PetalSonic

use crate::math::Vec3;
use crate::world::SourceId;
use std::time::Duration;

/// Timing information for a single render iteration
/// Used for performance profiling and stress testing
#[derive(Debug, Clone, Copy)]
pub struct RenderTimingEvent {
    /// Time spent mixing audio sources (microseconds)
    pub mixing_time_us: u64,
    /// Time spent on spatial processing (microseconds)
    pub spatial_time_us: u64,
    /// Time spent on resampling (microseconds)
    pub resampling_time_us: u64,
    /// Total time for the entire render iteration (microseconds)
    pub total_time_us: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PetalSonicEvent {
    SourceCompleted {
        source_id: SourceId,
    },
    SourceLooped {
        source_id: SourceId,
        loop_count: u32,
    },
    SourceStarted {
        source_id: SourceId,
    },
    SourceStopped {
        source_id: SourceId,
    },
    BufferUnderrun {
        source_id: Option<SourceId>,
    },
    BufferOverrun {
        source_id: Option<SourceId>,
    },
    DeviceChanged {
        device_name: String,
    },
    SpatializationError {
        source_id: SourceId,
        error: String,
    },
    SourceReachedEnd {
        source_id: SourceId,
        remaining_duration: Duration,
    },
    SourceVolumeChanged {
        source_id: SourceId,
        old_volume: f32,
        new_volume: f32,
    },
    SourcePoseChanged {
        source_id: SourceId,
        old_position: Vec3,
        new_position: Vec3,
    },
    ListenerPoseChanged {
        old_position: Vec3,
        new_position: Vec3,
    },
    EngineStarted,
    EngineStopped,
    EngineError {
        error: String,
    },
}

impl PetalSonicEvent {
    pub fn source_id(&self) -> Option<SourceId> {
        match self {
            Self::SourceCompleted { source_id }
            | Self::SourceLooped { source_id, .. }
            | Self::SourceStarted { source_id }
            | Self::SourceStopped { source_id }
            | Self::SpatializationError { source_id, .. }
            | Self::SourceReachedEnd { source_id, .. }
            | Self::SourceVolumeChanged { source_id, .. }
            | Self::SourcePoseChanged { source_id, .. } => Some(*source_id),
            Self::BufferUnderrun { source_id } | Self::BufferOverrun { source_id } => *source_id,
            _ => None,
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(
            self,
            Self::BufferUnderrun { .. }
                | Self::BufferOverrun { .. }
                | Self::SpatializationError { .. }
                | Self::EngineError { .. }
        )
    }

    pub fn is_source_event(&self) -> bool {
        matches!(
            self,
            Self::SourceCompleted { .. }
                | Self::SourceLooped { .. }
                | Self::SourceStarted { .. }
                | Self::SourceStopped { .. }
                | Self::SourceReachedEnd { .. }
                | Self::SourceVolumeChanged { .. }
                | Self::SourcePoseChanged { .. }
        )
    }
}
