use crate::gain;
use crate::math::{Pose, Vec3};

/// Configuration for how an audio source should be processed
#[derive(Debug, Clone)]
pub enum SourceConfig {
    /// Non-spatial audio - plays directly without 3D spatialization
    NonSpatial {
        /// Volume in decibels relative to unity (0.0 dB = 1.0 linear).
        volume_db: f32,
    },
    /// Spatial audio - uses 3D pose (position + orientation) and Steam Audio for spatialization
    Spatial {
        /// 3D pose (position and orientation) of the audio source
        pose: Pose,
        /// Volume in decibels relative to unity (0.0 dB = 1.0 linear).
        volume_db: f32,
    },
}

impl Default for SourceConfig {
    fn default() -> Self {
        // 0 dB = unity gain
        Self::NonSpatial { volume_db: 0.0 }
    }
}

impl SourceConfig {
    /// Create a non-spatial source configuration at 0 dB (unity gain).
    pub fn non_spatial() -> Self {
        Self::NonSpatial { volume_db: 0.0 }
    }

    /// Create a non-spatial source configuration with a volume in decibels.
    ///
    /// `0.0` dB is unity, negative values attenuate, positive values amplify.
    pub fn non_spatial_with_volume_db(volume_db: f32) -> Self {
        Self::NonSpatial { volume_db }
    }

    /// Create a non-spatial source configuration with a volume as linear gain.
    ///
    /// `1.0` is unity, `0.0` is silent, values > 1.0 amplify.
    pub fn non_spatial_with_volume_linear(volume: f32) -> Self {
        Self::NonSpatial {
            volume_db: gain::linear_to_db(volume),
        }
    }

    /// Create a spatial source configuration with the given pose at 0 dB (unity gain).
    pub fn spatial(pose: Pose) -> Self {
        Self::Spatial {
            pose,
            volume_db: 0.0,
        }
    }

    /// Create a spatial source configuration with pose and volume in decibels.
    ///
    /// `0.0` dB is unity, negative values attenuate, positive values amplify.
    pub fn spatial_with_volume_db(pose: Pose, volume_db: f32) -> Self {
        Self::Spatial { pose, volume_db }
    }

    /// Create a spatial source configuration with pose and volume as linear gain.
    ///
    /// `1.0` is unity, `0.0` is silent, values > 1.0 amplify.
    pub fn spatial_with_volume_linear(pose: Pose, volume: f32) -> Self {
        Self::Spatial {
            pose,
            volume_db: gain::linear_to_db(volume),
        }
    }

    /// Create a spatial source configuration from a position (with identity rotation)
    pub fn spatial_from_position(position: Vec3) -> Self {
        Self::Spatial {
            pose: Pose::from_position(position),
            volume_db: 0.0,
        }
    }

    /// Create a spatial source configuration from a position and volume in decibels
    /// (with identity rotation).
    pub fn spatial_from_position_with_volume_db(position: Vec3, volume_db: f32) -> Self {
        Self::Spatial {
            pose: Pose::from_position(position),
            volume_db,
        }
    }

    /// Create a spatial source configuration from a position and volume as linear gain
    /// (with identity rotation).
    pub fn spatial_from_position_with_volume_linear(position: Vec3, volume: f32) -> Self {
        Self::Spatial {
            pose: Pose::from_position(position),
            volume_db: gain::linear_to_db(volume),
        }
    }

    /// Returns true if this is a spatial source
    pub fn is_spatial(&self) -> bool {
        matches!(self, Self::Spatial { .. })
    }

    /// Returns the pose if this is a spatial source
    pub fn pose(&self) -> Option<Pose> {
        match self {
            Self::Spatial { pose, .. } => Some(*pose),
            Self::NonSpatial { .. } => None,
        }
    }

    pub(crate) fn set_pose(&mut self, next_pose: Pose) -> bool {
        match self {
            Self::Spatial { pose, .. } => {
                *pose = next_pose;
                true
            }
            Self::NonSpatial { .. } => false,
        }
    }

    /// Returns the position if this is a spatial source
    pub fn position(&self) -> Option<Vec3> {
        match self {
            Self::Spatial { pose, .. } => Some(pose.position),
            Self::NonSpatial { .. } => None,
        }
    }

    /// Returns the volume in decibels for both spatial and non-spatial sources.
    ///
    /// `0.0` dB is unity, negative values attenuate, positive values amplify.
    pub fn volume_db(&self) -> f32 {
        match self {
            Self::Spatial { volume_db, .. } => *volume_db,
            Self::NonSpatial { volume_db } => *volume_db,
        }
    }

    /// Returns the volume as linear gain for both spatial and non-spatial sources.
    ///
    /// `1.0` is unity, `0.0` is silent, values > 1.0 amplify.
    pub fn volume_linear(&self) -> f32 {
        gain::db_to_linear(self.volume_db())
    }

    /// Backwards-compatible convenience: returns the volume as linear gain.
    ///
    /// Equivalent to [`Self::volume_linear`].
    pub fn volume(&self) -> f32 {
        self.volume_linear()
    }
}
