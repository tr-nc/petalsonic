//! Event types for PetalSonic

use crate::math::Vec3;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum PetalSonicEvent {
    SourceCompleted {
        source_id: u64,
    },
    SourceLooped {
        source_id: u64,
        loop_count: u32,
    },
    SourceStarted {
        source_id: u64,
    },
    SourceStopped {
        source_id: u64,
    },
    BufferUnderrun {
        source_id: Option<u64>,
    },
    BufferOverrun {
        source_id: Option<u64>,
    },
    DeviceChanged {
        device_name: String,
    },
    SpatializationError {
        source_id: u64,
        error: String,
    },
    SourceReachedEnd {
        source_id: u64,
        remaining_duration: Duration,
    },
    SourceVolumeChanged {
        source_id: u64,
        old_volume: f32,
        new_volume: f32,
    },
    SourcePoseChanged {
        source_id: u64,
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
    pub fn source_id(&self) -> Option<u64> {
        match self {
            Self::SourceCompleted { source_id }
            | Self::SourceLooped { source_id, .. }
            | Self::SourceStarted { source_id }
            | Self::SourceStopped { source_id }
            | Self::SpatializationError { source_id, .. }
            | Self::SourceReachedEnd { source_id, .. }
            | Self::SourceVolumeChanged { source_id, .. }
            | Self::SourcePoseChanged { source_id, .. } => Some(*source_id),
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
