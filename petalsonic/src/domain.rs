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

/// Stable handle for one world-owned mix bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bus {
    pub(crate) world_id: u64,
    pub(crate) index: u16,
}

/// Initial declaration for a bus. Bus topology is fixed when the world is created.
#[derive(Clone, Debug, PartialEq)]
pub struct BusDesc {
    name: String,
    params: BusParams,
}

impl BusDesc {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            params: BusParams::default(),
        }
    }

    pub fn with_params(mut self, params: BusParams) -> Self {
        self.params = params;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn params(&self) -> BusParams {
        self.params
    }
}

/// Mutable controls for a fixed bus. Every declared bus feeds Master directly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BusParams {
    pub gain_db: f32,
    pub muted: bool,
    pub paused: bool,
    /// Playback speed with matching pitch change. High-quality time stretching is not provided.
    pub playback_rate: f32,
}

impl Default for BusParams {
    fn default() -> Self {
        Self {
            gain_db: 0.0,
            muted: false,
            paused: false,
            playback_rate: 1.0,
        }
    }
}

/// Initial, low-frequency properties of an emitter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EmitterDesc {
    placement: EmitterPlacement,
    gain_db: f32,
    bus: Option<Bus>,
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
            bus: None,
        }
    }

    pub fn spatial(pose: Pose) -> Self {
        Self {
            placement: EmitterPlacement::Spatial(pose),
            gain_db: 0.0,
            bus: None,
        }
    }

    pub fn with_gain_db(mut self, gain_db: f32) -> Self {
        self.gain_db = gain_db;
        self
    }

    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// Routes this emitter to a declared bus. Without this, it feeds Master directly.
    pub fn with_bus(mut self, bus: Bus) -> Self {
        self.bus = Some(bus);
        self
    }

    pub fn bus(&self) -> Option<Bus> {
        self.bus
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

    pub(crate) fn is_spatial(&self) -> bool {
        matches!(self.placement, EmitterPlacement::Spatial(_))
    }

    pub(crate) fn set_pose(&mut self, pose: Pose) -> bool {
        match &mut self.placement {
            EmitterPlacement::NonSpatial => false,
            EmitterPlacement::Spatial(current) => {
                *current = pose;
                true
            }
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
    bus: Option<Bus>,
    playback_rate: f32,
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

    /// Overrides the emitter's default bus for this playback.
    pub fn with_bus(mut self, bus: Bus) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Sets playback speed with a corresponding pitch change.
    pub fn with_playback_rate(mut self, playback_rate: f32) -> Self {
        self.playback_rate = playback_rate;
        self
    }

    pub(crate) fn bus(self) -> Option<Bus> {
        self.bus
    }

    pub(crate) fn playback_rate(self) -> f32 {
        self.playback_rate
    }
}

impl Default for PlayOptions {
    fn default() -> Self {
        Self {
            loop_mode: LoopMode::Once,
            gain_db: 0.0,
            detached: false,
            bus: None,
            playback_rate: 1.0,
        }
    }
}

/// One spatial emitter transform in a complete game-frame snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EmitterSpatialState {
    pub emitter: Emitter,
    pub pose: Pose,
}

impl EmitterSpatialState {
    pub fn new(emitter: Emitter, pose: Pose) -> Self {
        Self { emitter, pose }
    }
}

/// Atomic listener + emitter transform snapshot consumed at render-quantum edges.
#[derive(Clone, Debug)]
pub struct SpatialFrame {
    listener: Pose,
    emitters: Vec<EmitterSpatialState>,
}

impl SpatialFrame {
    pub fn new(listener: Pose, emitters: Vec<EmitterSpatialState>) -> Self {
        Self { listener, emitters }
    }

    pub fn listener(&self) -> Pose {
        self.listener
    }

    pub fn emitters(&self) -> &[EmitterSpatialState] {
        &self.emitters
    }
}
