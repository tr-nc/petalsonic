use crate::audio_data::PetalSonicAudioData;
use crate::config::SourceConfig;
use crate::math::Pose;
use crate::playback::LoopMode;
use crate::world::SourceId;
use std::sync::Arc;
use std::time::Duration;

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

    pub(crate) fn from_audio_data(data: Arc<PetalSonicAudioData>) -> Self {
        Self { data }
    }

    /// Take ownership of predecoded interleaved PCM without copying its sample buffer.
    pub fn from_interleaved_pcm(
        samples: Vec<f32>,
        sample_rate: u32,
        channels: u16,
    ) -> crate::error::Result<Self> {
        if sample_rate == 0 {
            return Err(crate::error::PetalSonicError::InvalidConfiguration {
                field: "sample_rate",
                reason: "must be greater than zero".into(),
            });
        }
        if channels == 0 {
            return Err(crate::error::PetalSonicError::InvalidConfiguration {
                field: "channels",
                reason: "must be greater than zero".into(),
            });
        }
        if !samples.len().is_multiple_of(channels as usize) {
            return Err(crate::error::PetalSonicError::AudioFormat(format!(
                "interleaved sample count {} is not divisible by {channels} channels",
                samples.len()
            )));
        }
        if samples.iter().any(|sample| !sample.is_finite()) {
            return Err(crate::error::PetalSonicError::AudioFormat(
                "PCM samples must all be finite".into(),
            ));
        }

        let frame_count = samples.len() / channels as usize;
        let duration = Duration::from_secs_f64(frame_count as f64 / sample_rate as f64);
        Ok(Self {
            data: Arc::new(PetalSonicAudioData::new(
                samples,
                sample_rate,
                channels,
                duration,
            )),
        })
    }

    /// Take ownership of predecoded mono PCM without copying its sample buffer.
    pub fn from_mono_pcm(samples: Vec<f32>, sample_rate: u32) -> crate::error::Result<Self> {
        Self::from_interleaved_pcm(samples, sample_rate, 1)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resident_pcm_constructor_takes_valid_interleaved_data() {
        let clip =
            ResidentClip::from_interleaved_pcm(vec![0.0, 0.5, -0.5, 1.0], 48_000, 2).unwrap();
        assert_eq!(clip.sample_rate(), 48_000);
        assert_eq!(clip.channels(), 2);
        assert_eq!(clip.total_frames(), 2);
    }

    #[test]
    fn resident_pcm_constructor_rejects_invalid_shape_and_values() {
        assert!(ResidentClip::from_interleaved_pcm(vec![0.0], 48_000, 2).is_err());
        assert!(ResidentClip::from_mono_pcm(vec![f32::NAN], 48_000).is_err());
        assert!(ResidentClip::from_mono_pcm(vec![0.0], 0).is_err());
    }

    #[test]
    fn play_options_default_to_compatible_world_routing() {
        let options = PlayOptions::once();
        assert_eq!(options.direct_path(), DirectPath::world());
        assert_eq!(
            options.environment_send(),
            EnvironmentSend::follow_emitter()
        );
        assert!(!options.has_spatial_routing_override());
    }

    #[test]
    fn split_routing_is_copied_as_per_play_value_state() {
        let local_pose = Pose::from_position(crate::math::Vec3::new(0.0, -0.08, 0.0));
        let contact = Pose::from_position(crate::math::Vec3::new(3.0, 1.0, -2.0));
        let options = PlayOptions::once()
            .with_direct_path(
                DirectPath::listener_relative(local_pose)
                    .with_geometry(DirectGeometry::BypassTransmission),
            )
            .with_environment_send(EnvironmentSend::from_world_pose(contact).with_gain_db(-12.0));

        assert_eq!(
            options.direct_path().placement(),
            DirectPlacement::ListenerRelative(local_pose)
        );
        assert_eq!(
            options.direct_path().geometry(),
            DirectGeometry::BypassTransmission
        );
        assert_eq!(
            options.environment_send().origin(),
            EnvironmentOrigin::World(contact)
        );
        assert_eq!(options.environment_send().gain_db(), -12.0);
    }
}

/// Opaque, generational handle for a long-lived logical sound emitter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Emitter {
    pub(crate) world_id: u64,
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

impl std::fmt::Display for Emitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Emitter({}:{}:{})",
            self.world_id, self.index, self.generation
        )
    }
}

/// Opaque handle for one explicitly controlled playback voice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlaybackControl {
    pub(crate) world_id: u64,
    pub(crate) voice_id: SourceId,
}

impl std::fmt::Display for PlaybackControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Playback({}:{})", self.world_id, self.voice_id)
    }
}

/// Caller-owned correlation value returned with controlled playback events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlaybackTag(pub u64);

/// Placement of the audible direct path for one playback Voice.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DirectPlacement {
    /// Use the Voice's current world-space emitter pose.
    World,
    /// Preserve this pose directly in listener-local coordinates.
    ///
    /// The position convention is x=right, y=up, z=front. The renderer does not convert this
    /// through a cached world pose, so listener translation and rotation cannot age the offset.
    ListenerRelative(Pose),
    /// Do not render an audible direct path. An EnvironmentSend may remain active.
    Disabled,
}

/// Geometry policy for the direct path, independent of its placement and propagation timing.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectGeometry {
    /// Apply the latest bounded asynchronous transmission response.
    SimulatedTransmission,
    /// Bypass geometry transmission while retaining placement, HRTF, distance, and air effects.
    BypassTransmission,
}

/// Propagation timing policy for the direct path.
///
/// PetalSonic currently renders direct paths immediately. This explicit policy keeps timing
/// independent from geometry so future propagation models do not overload obstruction controls.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectPropagation {
    Immediate,
}

/// Immutable direct-path routing captured by one playback Voice.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirectPath {
    placement: DirectPlacement,
    geometry: DirectGeometry,
    propagation: DirectPropagation,
}

impl DirectPath {
    /// Current-version-compatible world placement with simulated transmission.
    pub fn world() -> Self {
        Self {
            placement: DirectPlacement::World,
            geometry: DirectGeometry::SimulatedTransmission,
            propagation: DirectPropagation::Immediate,
        }
    }

    /// A direct path whose pose remains invariant in listener-local coordinates.
    pub fn listener_relative(local_pose: Pose) -> Self {
        Self {
            placement: DirectPlacement::ListenerRelative(local_pose),
            geometry: DirectGeometry::SimulatedTransmission,
            propagation: DirectPropagation::Immediate,
        }
    }

    /// No audible direct contribution. Environment routing remains an independent choice.
    pub fn disabled() -> Self {
        Self {
            placement: DirectPlacement::Disabled,
            geometry: DirectGeometry::BypassTransmission,
            propagation: DirectPropagation::Immediate,
        }
    }

    pub fn with_geometry(mut self, geometry: DirectGeometry) -> Self {
        self.geometry = geometry;
        self
    }

    pub fn with_propagation(mut self, propagation: DirectPropagation) -> Self {
        self.propagation = propagation;
        self
    }

    pub fn placement(self) -> DirectPlacement {
        self.placement
    }

    pub fn geometry(self) -> DirectGeometry {
        self.geometry
    }

    pub fn propagation(self) -> DirectPropagation {
        self.propagation
    }
}

impl Default for DirectPath {
    fn default() -> Self {
        Self::world()
    }
}

/// Immutable acoustic origin used by one Voice's environment send.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EnvironmentOrigin {
    /// Follow the Voice's current world-space emitter pose.
    FollowEmitter,
    /// Keep a world-space origin captured when the Voice is created.
    World(Pose),
    Disabled,
}

/// Routing from one Voice cursor into environmental responses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvironmentSend {
    origin: EnvironmentOrigin,
    gain_db: f32,
}

impl EnvironmentSend {
    /// Current-version-compatible environment routing that follows the emitter at unity gain.
    pub fn follow_emitter() -> Self {
        Self {
            origin: EnvironmentOrigin::FollowEmitter,
            gain_db: 0.0,
        }
    }

    /// Capture an independent world-space acoustic origin for this playback Voice.
    pub fn from_world_pose(origin: Pose) -> Self {
        Self {
            origin: EnvironmentOrigin::World(origin),
            gain_db: 0.0,
        }
    }

    pub fn disabled() -> Self {
        Self {
            origin: EnvironmentOrigin::Disabled,
            gain_db: 0.0,
        }
    }

    pub fn with_gain_db(mut self, gain_db: f32) -> Self {
        self.gain_db = gain_db;
        self
    }

    pub fn origin(self) -> EnvironmentOrigin {
        self.origin
    }

    pub fn gain_db(self) -> f32 {
        self.gain_db
    }
}

impl Default for EnvironmentSend {
    fn default() -> Self {
        Self::follow_emitter()
    }
}

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
    pub(crate) loop_mode: LoopMode,
    pub(crate) gain_db: f32,
    pub(crate) detached: bool,
    bus: Option<Bus>,
    playback_rate: f32,
    direct_path: Option<DirectPath>,
    environment_send: Option<EnvironmentSend>,
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

    /// Captures direct-path routing for the new Voice without changing the reusable emitter.
    pub fn with_direct_path(mut self, direct_path: DirectPath) -> Self {
        self.direct_path = Some(direct_path);
        self
    }

    /// Captures environment routing, including any fixed world origin, for the new Voice.
    pub fn with_environment_send(mut self, environment_send: EnvironmentSend) -> Self {
        self.environment_send = Some(environment_send);
        self
    }

    pub(crate) fn bus(self) -> Option<Bus> {
        self.bus
    }

    pub(crate) fn playback_rate(self) -> f32 {
        self.playback_rate
    }

    pub(crate) fn direct_path(self) -> DirectPath {
        self.direct_path.unwrap_or_default()
    }

    pub(crate) fn environment_send(self) -> EnvironmentSend {
        self.environment_send.unwrap_or_default()
    }

    pub(crate) fn has_spatial_routing_override(self) -> bool {
        self.direct_path.is_some() || self.environment_send.is_some()
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
            direct_path: None,
            environment_send: None,
        }
    }
}

/// One spatial emitter transform in a complete game-frame snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EmitterSpatialState {
    pub emitter: Emitter,
    pub pose: Pose,
    acoustic_priority: f32,
}

impl EmitterSpatialState {
    pub fn new(emitter: Emitter, pose: Pose) -> Self {
        Self {
            emitter,
            pose,
            acoustic_priority: 1.0,
        }
    }

    /// Sets relative priority for bounded acoustics solves. Non-finite and negative values are
    /// rejected when the complete spatial frame is published.
    pub fn with_acoustic_priority(mut self, acoustic_priority: f32) -> Self {
        self.acoustic_priority = acoustic_priority;
        self
    }

    pub fn acoustic_priority(&self) -> f32 {
        self.acoustic_priority
    }
}

/// Atomic listener + emitter transform snapshot consumed at render-quantum edges.
///
/// Revisions must increase for each published frame. Simulation time is expressed in seconds and
/// may stay equal between revisions, but must never move backwards.
#[derive(Clone, Debug)]
pub struct SpatialFrame {
    revision: u64,
    sim_time_seconds: f64,
    listener: Pose,
    emitters: Vec<EmitterSpatialState>,
}

impl SpatialFrame {
    /// Creates one complete, versioned spatial state generation.
    pub fn new(
        revision: u64,
        sim_time_seconds: f64,
        listener: Pose,
        emitters: Vec<EmitterSpatialState>,
    ) -> Self {
        Self {
            revision,
            sim_time_seconds,
            listener,
            emitters,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn sim_time_seconds(&self) -> f64 {
        self.sim_time_seconds
    }

    pub fn listener(&self) -> Pose {
        self.listener
    }

    pub fn emitters(&self) -> &[EmitterSpatialState] {
        &self.emitters
    }
}
