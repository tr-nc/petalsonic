use crate::audio_data::PetalSonicAudioData;
use crate::config::SourceConfig;
use crate::math::Pose;
pub use crate::occlusion::{DistributedOcclusionProfile, MAX_DIRECT_LOBES, OcclusionProfile};
use crate::playback::LoopMode;
pub use crate::source_extent::{
    ExtentSample, ExtentSampleId, MAX_EXTENT_RADIUS_WORLD_UNITS, MAX_EXTENT_SAMPLES, SourceExtent,
    WeightedSamples,
};
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

    #[test]
    fn source_extent_defaults_to_a_compatible_point() {
        assert_eq!(SourceExtent::default(), SourceExtent::Point);
        assert_eq!(SourceExtent::Point.sample_count(), 1);
        assert_eq!(ExtentSampleId::POINT, ExtentSampleId(u64::MAX));
        assert_eq!(
            EmitterDesc::spatial(Pose::identity()).extent(),
            &SourceExtent::Point
        );
    }

    #[test]
    fn weighted_extent_normalizes_power_and_orders_stable_ids() {
        let extent = SourceExtent::weighted_samples(vec![
            ExtentSample::new(ExtentSampleId(9), crate::math::Vec3::X, 3.0).unwrap(),
            ExtentSample::new(ExtentSampleId(2), -crate::math::Vec3::X, 1.0).unwrap(),
        ])
        .unwrap();
        let samples = extent.weighted().unwrap().samples();

        assert_eq!(samples[0].id(), ExtentSampleId(2));
        assert_eq!(samples[1].id(), ExtentSampleId(9));
        assert!((samples[0].power_weight() - 0.25).abs() < 1.0e-6);
        assert!((samples[1].power_weight() - 0.75).abs() < 1.0e-6);
        assert!((samples.iter().map(ExtentSample::power_weight).sum::<f32>() - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn weighted_extent_rejects_invalid_samples() {
        assert!(SourceExtent::weighted_samples(Vec::new()).is_err());
        assert!(
            ExtentSample::new(ExtentSampleId(1), crate::math::Vec3::splat(f32::NAN), 1.0).is_err()
        );
        assert!(ExtentSample::new(ExtentSampleId(1), crate::math::Vec3::ZERO, f32::NAN).is_err());
        assert!(ExtentSample::new(ExtentSampleId(1), crate::math::Vec3::ZERO, 0.0).is_err());

        let duplicate = ExtentSample::new(ExtentSampleId(4), crate::math::Vec3::ZERO, 1.0).unwrap();
        assert!(SourceExtent::weighted_samples(vec![duplicate.clone(), duplicate]).is_err());

        let too_many = (0..=MAX_EXTENT_SAMPLES)
            .map(|id| {
                ExtentSample::new(ExtentSampleId(id as u64), crate::math::Vec3::ZERO, 1.0).unwrap()
            })
            .collect();
        assert!(SourceExtent::weighted_samples(too_many).is_err());

        assert!(
            ExtentSample::new(
                ExtentSampleId(1),
                crate::math::Vec3::X * (MAX_EXTENT_RADIUS_WORLD_UNITS + 1.0),
                1.0,
            )
            .is_err()
        );
    }

    #[test]
    fn occlusion_profile_defaults_to_compatible_point_exact() {
        assert_eq!(OcclusionProfile::default(), OcclusionProfile::PointExact);
        assert_eq!(
            EmitterDesc::spatial(Pose::identity()).occlusion_profile(),
            OcclusionProfile::PointExact
        );
    }

    #[test]
    fn distributed_occlusion_profile_validates_all_stability_controls() {
        let profile = DistributedOcclusionProfile::default()
            .with_gain_floor([0.5, 0.25, 0.125])
            .unwrap()
            .with_response_times(0.2, 0.1)
            .unwrap()
            .with_classification(0.2, 0.6, 0.15)
            .unwrap()
            .with_max_response_age(0.4)
            .unwrap()
            .with_lobe_count(4)
            .unwrap();

        assert_eq!(profile.gain_floor(), [0.5, 0.25, 0.125]);
        assert_eq!(profile.response_times_seconds(), (0.2, 0.1));
        assert_eq!(profile.classification(), (0.2, 0.6, 0.15));
        assert_eq!(profile.max_response_age_seconds(), 0.4);
        assert_eq!(profile.lobe_count(), 4);

        assert!(
            DistributedOcclusionProfile::default()
                .with_gain_floor([0.0, 1.0, 1.0])
                .is_err()
        );
        assert!(
            DistributedOcclusionProfile::default()
                .with_gain_floor([1.1, 1.0, 1.0])
                .is_err()
        );
        assert!(
            DistributedOcclusionProfile::default()
                .with_response_times(f32::NAN, 0.1)
                .is_err()
        );
        assert!(
            DistributedOcclusionProfile::default()
                .with_classification(0.6, 0.2, 0.1)
                .is_err()
        );
        assert!(
            DistributedOcclusionProfile::default()
                .with_max_response_age(0.0)
                .is_err()
        );
        assert!(
            DistributedOcclusionProfile::default()
                .with_lobe_count(0)
                .is_err()
        );
        assert!(
            DistributedOcclusionProfile::default()
                .with_lobe_count((MAX_DIRECT_LOBES + 1) as u8)
                .is_err()
        );
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

/// Caller-owned correlation value for opt-in per-Voice render telemetry.
///
/// PetalSonic preserves this value but does not assign or enforce uniqueness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlayCommandId(pub u64);

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
#[derive(Clone, Debug, PartialEq)]
pub struct EmitterDesc {
    placement: EmitterPlacement,
    gain_db: f32,
    bus: Option<Bus>,
    extent: SourceExtent,
    occlusion_profile: OcclusionProfile,
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
            extent: SourceExtent::Point,
            occlusion_profile: OcclusionProfile::PointExact,
        }
    }

    pub fn spatial(pose: Pose) -> Self {
        Self {
            placement: EmitterPlacement::Spatial(pose),
            gain_db: 0.0,
            bus: None,
            extent: SourceExtent::Point,
            occlusion_profile: OcclusionProfile::PointExact,
        }
    }

    pub fn with_gain_db(mut self, gain_db: f32) -> Self {
        self.gain_db = gain_db;
        self
    }

    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// Sets the initial local source extent. Later extent changes must arrive in a complete
    /// [`SpatialFrame`] and are captured only by subsequently accepted Voices.
    pub fn with_extent(mut self, extent: SourceExtent) -> Self {
        self.extent = extent;
        self
    }

    pub fn extent(&self) -> &SourceExtent {
        &self.extent
    }

    /// Sets the default occlusion policy captured by subsequently accepted Voices.
    pub fn with_occlusion_profile(mut self, profile: OcclusionProfile) -> Self {
        self.occlusion_profile = profile;
        self
    }

    pub fn occlusion_profile(&self) -> OcclusionProfile {
        self.occlusion_profile
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

    pub(crate) fn source_config(&self, voice_gain_db: f32) -> SourceConfig {
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

    pub(crate) fn set_extent(&mut self, extent: SourceExtent) {
        self.extent = extent;
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
    play_command_id: Option<PlayCommandId>,
    occlusion_profile: Option<OcclusionProfile>,
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

    /// Requests one-shot spatial render and environment-response telemetry correlated by this
    /// value. This option requires a spatial emitter.
    pub fn with_play_command_id(mut self, play_command_id: PlayCommandId) -> Self {
        self.play_command_id = Some(play_command_id);
        self
    }

    /// Overrides the Emitter's occlusion policy for this Voice without changing its extent.
    pub fn with_occlusion_profile(mut self, profile: OcclusionProfile) -> Self {
        self.occlusion_profile = Some(profile);
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
        self.direct_path.is_some()
            || self.environment_send.is_some()
            || self.occlusion_profile.is_some()
    }

    pub(crate) fn play_command_id(self) -> Option<PlayCommandId> {
        self.play_command_id
    }

    pub(crate) fn occlusion_profile(self, emitter_default: OcclusionProfile) -> OcclusionProfile {
        self.occlusion_profile.unwrap_or(emitter_default)
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
            play_command_id: None,
            occlusion_profile: None,
        }
    }
}

/// One spatial emitter transform in a complete game-frame snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct EmitterSpatialState {
    pub emitter: Emitter,
    pub pose: Pose,
    acoustic_priority: f32,
    extent: SourceExtent,
}

impl EmitterSpatialState {
    pub fn new(emitter: Emitter, pose: Pose) -> Self {
        Self {
            emitter,
            pose,
            acoustic_priority: 1.0,
            extent: SourceExtent::Point,
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

    /// Publishes the emitter's latest local extent in this complete spatial generation.
    pub fn with_extent(mut self, extent: SourceExtent) -> Self {
        self.extent = extent;
        self
    }

    pub fn extent(&self) -> &SourceExtent {
        &self.extent
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
