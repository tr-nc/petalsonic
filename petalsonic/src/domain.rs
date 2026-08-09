use crate::audio_data::PetalSonicAudioData;
use crate::config::SourceConfig;
use crate::math::Pose;
use crate::playback::LoopMode;
use crate::world::SourceId;
use std::sync::Arc;

/// Immutable, predecoded PCM shared by any number of playback voices.
#[derive(Clone, Debug)]
pub struct ResidentClip {
    pub(crate) data: Arc<PetalSonicAudioData>,
}

impl ResidentClip {
    pub fn from_path(path: impl AsRef<std::path::Path>) -> crate::error::Result<Self> {
        let path = path.as_ref().to_str().ok_or_else(|| {
            crate::error::PetalSonicError::AudioLoading("Audio path is not valid UTF-8".to_string())
        })?;
        Ok(Self {
            data: PetalSonicAudioData::from_path(path)?,
        })
    }

    pub fn from_audio_data(data: Arc<PetalSonicAudioData>) -> Self {
        Self { data }
    }

    pub fn sample_rate(&self) -> u32 {
        self.data.sample_rate()
    }

    pub fn channels(&self) -> u16 {
        self.data.channels()
    }

    pub fn total_frames(&self) -> usize {
        self.data.total_frames()
    }
}

/// Opaque, generational handle for a long-lived logical sound emitter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Emitter {
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

impl std::fmt::Display for Emitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Emitter({}:{})", self.index, self.generation)
    }
}

/// Opaque handle for one explicitly controlled playback voice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlaybackControl {
    pub(crate) voice_id: SourceId,
}

impl std::fmt::Display for PlaybackControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Playback({})", self.voice_id)
    }
}

/// Caller-owned correlation value returned with controlled playback events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlaybackTag(pub u64);

/// Initial, low-frequency properties of an emitter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EmitterDesc {
    placement: EmitterPlacement,
    gain_db: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum EmitterPlacement {
    NonSpatial,
    Spatial(Pose),
}

impl EmitterDesc {
    pub fn non_spatial() -> Self {
        Self {
            placement: EmitterPlacement::NonSpatial,
            gain_db: 0.0,
        }
    }

    pub fn spatial(pose: Pose) -> Self {
        Self {
            placement: EmitterPlacement::Spatial(pose),
            gain_db: 0.0,
        }
    }

    pub fn with_gain_db(mut self, gain_db: f32) -> Self {
        self.gain_db = gain_db;
        self
    }

    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    pub fn pose(&self) -> Option<Pose> {
        match self.placement {
            EmitterPlacement::NonSpatial => None,
            EmitterPlacement::Spatial(pose) => Some(pose),
        }
    }

    pub(crate) fn source_config(self, voice_gain_db: f32) -> SourceConfig {
        let gain_db = self.gain_db + voice_gain_db;
        match self.placement {
            EmitterPlacement::NonSpatial => SourceConfig::non_spatial_with_volume_db(gain_db),
            EmitterPlacement::Spatial(pose) => SourceConfig::spatial_with_volume_db(pose, gain_db),
        }
    }
}

impl Default for EmitterDesc {
    fn default() -> Self {
        Self::non_spatial()
    }
}

/// Per-play properties. A normal play is automatically reclaimed and returns no
/// second handle to the caller.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayOptions {
    pub loop_mode: LoopMode,
    pub gain_db: f32,
    pub detached: bool,
}

impl PlayOptions {
    pub fn once() -> Self {
        Self::default()
    }

    pub fn looping() -> Self {
        Self {
            loop_mode: LoopMode::Infinite,
            ..Self::default()
        }
    }

    pub fn with_gain_db(mut self, gain_db: f32) -> Self {
        self.gain_db = gain_db;
        self
    }

    pub fn detached(mut self) -> Self {
        self.detached = true;
        self
    }
}

impl Default for PlayOptions {
    fn default() -> Self {
        Self {
            loop_mode: LoopMode::Once,
            gain_db: 0.0,
            detached: false,
        }
    }
}
