use crate::audio_data::PetalSonicAudioData;
use crate::config::{PetalSonicWorldDesc, SourceConfig};
use crate::error::Result;
use crate::math::{Pose, Vec3};
use crate::playback::{LoopMode, PlaybackCommand, PlaybackSource};
use crate::procedural::ProceduralAudioFactory;
use crate::spatial::DirectPathOverride;
use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Lightweight, type-safe handle for audio sources.
///
/// Returned when adding audio data to the world. Used to reference audio sources
/// for playback operations (play, pause, stop).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(u64);

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SourceId({})", self.0)
    }
}

impl From<u64> for SourceId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// Main world object that manages 3D audio sources and playback.
///
/// `PetalSonicWorld` is the central API for PetalSonic. It runs on the main thread
/// and provides a world-driven interface where you manage audio sources, listeners,
/// and playback commands. The actual audio processing happens on a separate thread
/// via the audio engine.
///
/// # Architecture
///
/// - **Main thread**: Owns the `PetalSonicWorld`, loads audio files, manages sources
/// - **Audio thread**: Receives commands via channels, performs spatialization and playback
pub struct PetalSonicWorld {
    desc: PetalSonicWorldDesc,
    source_storage: std::sync::Mutex<HashMap<SourceId, PlaybackSource>>,
    source_configs: std::sync::Mutex<HashMap<SourceId, SourceConfig>>,
    listener: std::sync::Mutex<PetalSonicAudioListener>,
    next_source_id: std::sync::Mutex<u64>,
    command_sender: Sender<PlaybackCommand>,
    command_receiver: Receiver<PlaybackCommand>,
    /// Optional reference to engine's listener pose for automatic synchronization
    /// This is set by the engine when it's created, allowing the world to
    /// automatically update the engine's listener when set_listener_pose is called
    engine_listener_pose: std::sync::Mutex<Option<Arc<Mutex<Pose>>>>,
}

impl PetalSonicWorld {
    pub fn new(config: PetalSonicWorldDesc) -> Result<Self> {
        let (command_sender, command_receiver) = crossbeam_channel::unbounded();
        Ok(Self {
            desc: config,
            source_storage: std::sync::Mutex::new(HashMap::new()),
            source_configs: std::sync::Mutex::new(HashMap::new()),
            listener: std::sync::Mutex::new(PetalSonicAudioListener::default()),
            next_source_id: std::sync::Mutex::new(0),
            command_sender,
            command_receiver,
            engine_listener_pose: std::sync::Mutex::new(None),
        })
    }

    /// Returns the sample rate of the audio world.
    pub fn sample_rate(&self) -> u32 {
        self.desc.sample_rate
    }

    /// Registers audio data in the world's internal storage and returns a SourceId handle.
    ///
    /// This pre-loads and prepares the audio for playback but does not start playing it.
    /// Call `play()` with the returned SourceId to actually start playback.
    ///
    /// The audio data is automatically resampled to match the world's sample rate if needed.
    ///
    /// # Arguments
    ///
    /// * `audio_data` - The audio data to register
    /// * `config` - Configuration for how the source should be processed (spatial or non-spatial)
    pub fn register_audio(
        &self,
        audio_data: Arc<PetalSonicAudioData>,
        config: SourceConfig,
    ) -> Result<SourceId> {
        // Automatically resample if the audio data sample rate doesn't match the world's sample rate
        let resampled_audio_data = if audio_data.sample_rate() != self.desc.sample_rate {
            Arc::new(audio_data.resample(self.desc.sample_rate)?)
        } else {
            audio_data
        };

        let mut next_id = self.next_source_id.lock().unwrap();
        let id = SourceId(*next_id);
        *next_id += 1;
        drop(next_id);

        self.source_storage
            .lock()
            .unwrap()
            .insert(id, PlaybackSource::Static(resampled_audio_data));
        self.source_configs.lock().unwrap().insert(id, config);
        Ok(id)
    }

    /// Registers a procedural audio source factory and returns a SourceId handle.
    ///
    /// The factory is retained on the world thread. When playback starts, PetalSonic
    /// creates a fresh render-thread-owned source instance at the world sample rate.
    pub fn register_procedural(
        &self,
        factory: Arc<dyn ProceduralAudioFactory>,
        config: SourceConfig,
    ) -> Result<SourceId> {
        let mut next_id = self.next_source_id.lock().unwrap();
        let id = SourceId(*next_id);
        *next_id += 1;
        drop(next_id);

        self.source_storage.lock().unwrap().insert(
            id,
            PlaybackSource::Procedural {
                factory,
                sample_rate: self.desc.sample_rate,
            },
        );
        self.source_configs.lock().unwrap().insert(id, config);
        Ok(id)
    }

    /// Retrieves audio data by its SourceId.
    ///
    /// # Arguments
    ///
    /// * `id` - The SourceId of the audio source
    ///
    /// # Returns
    ///
    /// `Some(Arc<PetalSonicAudioData>)` if found, `None` otherwise
    pub fn get_audio_data(&self, id: SourceId) -> Option<Arc<PetalSonicAudioData>> {
        match self.source_storage.lock().unwrap().get(&id) {
            Some(PlaybackSource::Static(audio_data)) => Some(audio_data.clone()),
            _ => None,
        }
    }

    /// Removes audio data from the world by its SourceId.
    ///
    /// # Arguments
    ///
    /// * `id` - The SourceId of the audio source to remove
    ///
    /// # Returns
    ///
    /// The removed audio data if it existed, `None` otherwise
    pub fn remove_audio_data(&self, id: SourceId) -> Option<Arc<PetalSonicAudioData>> {
        self.source_configs.lock().unwrap().remove(&id);
        match self.source_storage.lock().unwrap().remove(&id) {
            Some(PlaybackSource::Static(audio_data)) => Some(audio_data),
            _ => None,
        }
    }

    /// Removes any registered source type from the world by its SourceId.
    pub fn remove_source(&self, id: SourceId) -> bool {
        self.source_configs.lock().unwrap().remove(&id);
        self.source_storage.lock().unwrap().remove(&id).is_some()
    }

    /// Returns a list of all audio source IDs currently stored in the world.
    pub fn get_audio_source_ids(&self) -> Vec<SourceId> {
        self.source_storage
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect()
    }

    pub fn contains_audio(&self, id: SourceId) -> bool {
        self.source_storage.lock().unwrap().contains_key(&id)
    }

    /// Sets the listener pose (position and orientation) for spatial audio.
    ///
    /// The listener represents the position and orientation of the "ears" in the 3D world.
    /// All spatial audio sources will be spatialized relative to this listener.
    ///
    /// If an engine has been connected via `connect_engine_listener`, this automatically
    /// synchronizes the pose to the engine, ensuring the render thread uses the updated
    /// position without additional API calls.
    ///
    /// # Arguments
    ///
    /// * `pose` - The new pose for the listener
    pub fn set_listener_pose(&self, pose: Pose) {
        self.listener.lock().unwrap().pose = pose;

        // Automatically synchronize with engine if connected
        if let Ok(engine_pose_opt) = self.engine_listener_pose.lock()
            && let Some(engine_pose) = engine_pose_opt.as_ref()
            && let Ok(mut engine_listener) = engine_pose.lock()
        {
            *engine_listener = pose;
        }
    }

    /// Internal method to connect the engine's listener pose for automatic synchronization.
    ///
    /// This is called by the engine when it's created, establishing a link that allows
    /// the world to automatically update the engine's listener pose whenever
    /// `set_listener_pose` is called.
    ///
    /// # Arguments
    ///
    /// * `engine_pose` - Reference to the engine's listener pose
    pub(crate) fn connect_engine_listener(&self, engine_pose: Arc<Mutex<Pose>>) {
        *self.engine_listener_pose.lock().unwrap() = Some(engine_pose);
    }

    /// Returns a copy of the current listener.
    pub fn listener(&self) -> PetalSonicAudioListener {
        self.listener.lock().unwrap().clone()
    }

    /// Updates the configuration for a source (e.g., position, volume).
    ///
    /// This is useful for dynamically changing spatial audio properties without
    /// stopping and restarting playback.
    ///
    /// # Arguments
    ///
    /// * `audio_id` - SourceId of the audio source to update
    /// * `config` - New configuration for the source
    ///
    /// # Errors
    ///
    /// Returns an error if the audio source ID is not found or if the command
    /// fails to send to the audio engine.
    pub fn update_source_config(&self, audio_id: SourceId, config: SourceConfig) -> Result<()> {
        if !self.contains_audio(audio_id) {
            return Err(crate::error::PetalSonicError::Engine(format!(
                "Audio data with ID {:?} not found",
                audio_id
            )));
        }

        // Update the config in storage
        self.source_configs
            .lock()
            .unwrap()
            .insert(audio_id, config.clone());

        // Send command to update active playback instance if it exists
        self.command_sender
            .send(PlaybackCommand::UpdateConfig(audio_id, config))
            .map_err(|e| {
                crate::error::PetalSonicError::Engine(format!(
                    "Failed to send update config command: {}",
                    e
                ))
            })?;

        Ok(())
    }

    /// Updates host-provided direct-path data for a spatial source.
    ///
    /// This lets applications feed externally computed occlusion / transmission
    /// into Steam Audio's direct effect stage.
    pub fn update_source_direct_path_override(
        &self,
        audio_id: SourceId,
        direct_path_override: Option<DirectPathOverride>,
    ) -> Result<()> {
        if !self.contains_audio(audio_id) {
            return Err(crate::error::PetalSonicError::Engine(format!(
                "Audio data with ID {:?} not found",
                audio_id
            )));
        }

        self.command_sender
            .send(PlaybackCommand::UpdateDirectPathOverride(
                audio_id,
                direct_path_override,
            ))
            .map_err(|e| {
                crate::error::PetalSonicError::Engine(format!(
                    "Failed to send direct-path override command: {}",
                    e
                ))
            })?;

        Ok(())
    }

    /// Starts playing an audio source by its SourceId.
    ///
    /// Sends a play command to the audio engine thread. The audio will begin playing
    /// from its current position (or from the beginning if not yet played).
    ///
    /// This method looks up the audio data and configuration on the world thread (where
    /// blocking on locks is acceptable) and sends both to the engine, eliminating the
    /// need for the engine to call back into world storage during playback processing.
    ///
    /// # Arguments
    ///
    /// * `audio_id` - SourceId of the audio source to play
    /// * `loop_mode` - How the audio should loop (Once, Infinite, or Count(n))
    ///
    /// # Errors
    ///
    /// Returns an error if the audio source ID is not found in the world storage
    /// or if the command fails to send to the audio engine.
    pub fn play(&self, audio_id: SourceId, loop_mode: LoopMode) -> Result<()> {
        // Look up source payload on the world thread (blocking is fine here)
        let source = self
            .source_storage
            .lock()
            .unwrap()
            .get(&audio_id)
            .cloned()
            .ok_or_else(|| {
                crate::error::PetalSonicError::Engine(format!(
                    "Audio source with ID {:?} not found",
                    audio_id
                ))
            })?;

        // Get the source config for this audio source
        let config = self
            .source_configs
            .lock()
            .unwrap()
            .get(&audio_id)
            .cloned()
            .unwrap_or_default();

        // Send command with source payload included - engine no longer needs to call back into world
        self.command_sender
            .send(PlaybackCommand::Play(audio_id, source, config, loop_mode))
            .map_err(|e| {
                crate::error::PetalSonicError::Engine(format!("Failed to send play command: {}", e))
            })?;

        Ok(())
    }

    /// Pauses a playing audio source by its SourceId.
    ///
    /// Sends a pause command to the audio engine thread. The audio will stop playing
    /// but retain its current playback position.
    ///
    /// # Arguments
    ///
    /// * `audio_id` - SourceId of the audio source to pause
    ///
    /// # Errors
    ///
    /// Returns an error if the command fails to send to the audio engine.
    pub fn pause(&self, audio_id: SourceId) -> Result<()> {
        self.command_sender
            .send(PlaybackCommand::Pause(audio_id))
            .map_err(|e| {
                crate::error::PetalSonicError::Engine(format!(
                    "Failed to send pause command: {}",
                    e
                ))
            })?;

        Ok(())
    }

    /// Stops a playing audio source by its SourceId.
    ///
    /// Sends a stop command to the audio engine thread. The audio will stop playing
    /// and reset its playback position to the beginning.
    ///
    /// # Arguments
    ///
    /// * `audio_id` - SourceId of the audio source to stop
    ///
    /// # Errors
    ///
    /// Returns an error if the command fails to send to the audio engine.
    pub fn stop(&self, audio_id: SourceId) -> Result<()> {
        self.command_sender
            .send(PlaybackCommand::Stop(audio_id))
            .map_err(|e| {
                crate::error::PetalSonicError::Engine(format!("Failed to send stop command: {}", e))
            })?;

        Ok(())
    }

    /// Stops all currently playing audio sources.
    ///
    /// Sends a stop-all command to the audio engine thread. All active audio playback
    /// will be stopped and reset.
    ///
    /// # Errors
    ///
    /// Returns an error if the command fails to send to the audio engine.
    pub fn stop_all(&self) -> Result<()> {
        self.command_sender
            .send(PlaybackCommand::StopAll)
            .map_err(|e| {
                crate::error::PetalSonicError::Engine(format!(
                    "Failed to send stop all command: {}",
                    e
                ))
            })?;

        Ok(())
    }

    /// Seeks to a specific position in an audio source's playback.
    ///
    /// Sends a seek command to the audio engine thread. The playback position will
    /// be updated to the specified progress value. This works for both playing and
    /// paused sources.
    ///
    /// # Arguments
    ///
    /// * `audio_id` - SourceId of the audio source to seek
    /// * `progress` - Playback progress in range [0.0, 1.0] where 0.0 is the start and 1.0 is the end
    ///
    /// # Behavior
    ///
    /// - Progress is automatically clamped to [0.0, 1.0]
    /// - Only affects sources currently in active playback
    /// - Position is updated immediately on the next audio callback
    ///
    /// # Errors
    ///
    /// Returns an error if the command fails to send to the audio engine.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use petalsonic::*;
    /// # let world = PetalSonicWorld::new(PetalSonicWorldDesc::default()).unwrap();
    /// # let source_id = SourceId::from(0);
    /// // Seek to 50% through the audio
    /// world.seek(source_id, 0.5)?;
    ///
    /// // Jump to the beginning
    /// world.seek(source_id, 0.0)?;
    ///
    /// // Jump to near the end
    /// world.seek(source_id, 0.95)?;
    /// # Ok::<(), PetalSonicError>(())
    /// ```
    pub fn seek(&self, audio_id: SourceId, progress: f32) -> Result<()> {
        self.command_sender
            .send(PlaybackCommand::Seek(audio_id, progress))
            .map_err(|e| {
                crate::error::PetalSonicError::Engine(format!("Failed to send seek command: {}", e))
            })?;

        Ok(())
    }

    /// Returns a reference to the command receiver for the audio engine.
    ///
    /// This receiver is used by the audio engine thread to poll for playback commands
    /// sent from the main thread. This method is primarily used internally when
    /// initializing the audio engine.
    ///
    /// # Returns
    ///
    /// A reference to the `Receiver<PlaybackCommand>` channel
    pub fn command_receiver(&self) -> &Receiver<PlaybackCommand> {
        &self.command_receiver
    }
}

/// Represents a 3D audio source in the world.
///
/// `PetalSonicAudioSource` contains the spatial properties and state of an audio source.
/// This struct is primarily used for querying source state rather than direct manipulation.
/// To update source properties during playback, use [`PetalSonicWorld::update_source_config`].
///
/// # Properties
///
/// - Position in 3D space (`Vec3`)
/// - Volume level in decibels (0.0 dB = unity, negative attenuates, positive amplifies)
pub struct PetalSonicAudioSource {
    pub(crate) _id: u64,
    pub(crate) position: Vec3,
    pub(crate) volume_db: f32,
}

impl PetalSonicAudioSource {
    /// Returns the 3D position of the audio source.
    ///
    /// # Returns
    ///
    /// The position as a `Vec3` (x, y, z coordinates).
    pub fn position(&self) -> Vec3 {
        self.position
    }

    /// Returns the volume level of the audio source in decibels.
    ///
    /// # Returns
    ///
    /// Volume in decibels where 0.0 dB is unity, negative values attenuate, and
    /// positive values amplify.
    pub fn volume_db(&self) -> f32 {
        self.volume_db
    }
}

/// Represents the listener (the "ears") in the 3D audio world.
///
/// `PetalSonicAudioListener` defines the position and orientation from which all spatial
/// audio is perceived. In a typical game or application, this would represent the player's
/// camera or character position.
///
/// # Usage
///
/// The listener's pose determines how spatial audio sources are spatialized:
/// - **Position**: Where the listener is located in 3D space
/// - **Orientation**: Which direction the listener is facing (affects left/right, front/back audio)
///
/// Update the listener position using [`PetalSonicWorld::set_listener_pose`] as the player
/// or camera moves through the world.
///
/// # Example
///
/// ```no_run
/// # use petalsonic::*;
/// # use petalsonic::math::{Pose, Vec3};
/// # let world = PetalSonicWorld::new(PetalSonicWorldDesc::default()).unwrap();
/// // Move listener to position (10, 0, 5) facing forward
/// let pose = Pose::from_position(Vec3::new(10.0, 0.0, 5.0));
/// world.set_listener_pose(pose);
/// ```
#[derive(Clone, Default)]
pub struct PetalSonicAudioListener {
    pub(crate) pose: Pose,
}

impl PetalSonicAudioListener {
    /// Creates a new audio listener with the given pose.
    ///
    /// # Arguments
    ///
    /// * `pose` - The initial position and orientation of the listener
    pub fn new(pose: Pose) -> Self {
        Self { pose }
    }

    /// Returns the current pose (position and orientation) of the listener.
    ///
    /// # Returns
    ///
    /// The listener's `Pose` containing position and rotation.
    pub fn pose(&self) -> Pose {
        self.pose
    }

    /// Sets the pose (position and orientation) of the listener.
    ///
    /// # Arguments
    ///
    /// * `pose` - The new pose for the listener
    pub fn set_pose(&mut self, pose: Pose) {
        self.pose = pose;
    }
}
