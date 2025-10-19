use crate::math::Vec3;

/// Configuration for how an audio source should be processed
#[derive(Debug, Clone)]
pub enum SourceConfig {
    /// Non-spatial audio - plays directly without 3D spatialization
    NonSpatial {
        /// Volume multiplier (0.0 = silent, 1.0 = full volume)
        volume: f32,
    },
    /// Spatial audio - uses 3D position and Steam Audio for spatialization
    Spatial {
        /// 3D position of the audio source
        position: Vec3,
        /// Volume multiplier (0.0 = silent, 1.0 = full volume)
        volume: f32,
    },
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self::NonSpatial { volume: 1.0 }
    }
}

impl SourceConfig {
    /// Create a non-spatial source configuration with default volume
    pub fn non_spatial() -> Self {
        Self::NonSpatial { volume: 1.0 }
    }

    /// Create a non-spatial source configuration with custom volume
    pub fn non_spatial_with_volume(volume: f32) -> Self {
        Self::NonSpatial { volume }
    }

    /// Create a spatial source configuration with the given position
    pub fn spatial(position: Vec3) -> Self {
        Self::Spatial {
            position,
            volume: 1.0,
        }
    }

    /// Create a spatial source configuration with position and volume
    pub fn spatial_with_volume(position: Vec3, volume: f32) -> Self {
        Self::Spatial { position, volume }
    }

    /// Returns true if this is a spatial source
    pub fn is_spatial(&self) -> bool {
        matches!(self, Self::Spatial { .. })
    }

    /// Returns the position if this is a spatial source
    pub fn position(&self) -> Option<Vec3> {
        match self {
            Self::Spatial { position, .. } => Some(*position),
            Self::NonSpatial { .. } => None,
        }
    }

    /// Returns the volume for both spatial and non-spatial sources
    pub fn volume(&self) -> f32 {
        match self {
            Self::Spatial { volume, .. } => *volume,
            Self::NonSpatial { volume } => *volume,
        }
    }
}
