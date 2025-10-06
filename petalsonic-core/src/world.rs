use crate::audio_data::PetalSonicAudioData;
use crate::config::PetalSonicWorldDesc;
use crate::error::Result;
use crate::math::{Pose, Vec3};
use crate::playback::PlaybackCommand;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

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
    audio_data_storage: HashMap<Uuid, Arc<PetalSonicAudioData>>,
    command_sender: Sender<PlaybackCommand>,
    command_receiver: Receiver<PlaybackCommand>,
}

impl PetalSonicWorld {
    pub fn new(config: PetalSonicWorldDesc) -> Result<Self> {
        let (command_sender, command_receiver) = unbounded();
        Ok(Self {
            desc: config,
            audio_data_storage: HashMap::new(),
            command_sender,
            command_receiver,
        })
    }

    /// Returns the sample rate of the audio world.
    pub fn sample_rate(&self) -> u32 {
        self.desc.sample_rate
    }

    /// Adds audio data to the world's internal storage and returns a UUID handle.
    ///
    /// The audio data is automatically resampled to match the world's sample rate if needed.
    pub fn add_source(&mut self, audio_data: Arc<PetalSonicAudioData>) -> Result<Uuid> {
        // Automatically resample if the audio data sample rate doesn't match the world's sample rate
        let resampled_audio_data = if audio_data.sample_rate() != self.desc.sample_rate {
            Arc::new(audio_data.resample(self.desc.sample_rate)?)
        } else {
            audio_data
        };

        let uuid = Uuid::new_v4();
        self.audio_data_storage.insert(uuid, resampled_audio_data);
        Ok(uuid)
    }

    /// Retrieves audio data by its UUID.
    ///
    /// # Arguments
    ///
    /// * `uuid` - The UUID of the audio source
    ///
    /// # Returns
    ///
    /// `Some(&Arc<PetalSonicAudioData>)` if found, `None` otherwise
    pub fn get_audio_data(&self, uuid: Uuid) -> Option<&Arc<PetalSonicAudioData>> {
        self.audio_data_storage.get(&uuid)
    }

    /// Removes audio data from the world by its UUID.
    ///
    /// # Arguments
    ///
    /// * `uuid` - The UUID of the audio source to remove
    ///
    /// # Returns
    ///
    /// The removed audio data if it existed, `None` otherwise
    pub fn remove_audio_data(&mut self, uuid: Uuid) -> Option<Arc<PetalSonicAudioData>> {
        self.audio_data_storage.remove(&uuid)
    }

    /// Returns a list of all audio source UUIDs currently stored in the world.
    pub fn get_audio_data_uuids(&self) -> Vec<Uuid> {
        self.audio_data_storage.keys().copied().collect()
    }

    /// Starts playing an audio source by its UUID.
    ///
    /// Sends a play command to the audio engine thread. The audio will begin playing
    /// from its current position (or from the beginning if not yet played).
    ///
    /// # Arguments
    ///
    /// * `audio_id` - UUID of the audio source to play
    ///
    /// # Errors
    ///
    /// Returns an error if the audio source UUID is not found in the world storage
    /// or if the command fails to send to the audio engine.
    pub fn play(&self, audio_id: Uuid) -> Result<()> {
        if !self.audio_data_storage.contains_key(&audio_id) {
            return Err(crate::error::PetalSonicError::Engine(
                format!("Audio data with UUID {} not found", audio_id).into(),
            ));
        }

        self.command_sender
            .send(PlaybackCommand::Play(audio_id))
            .map_err(|e| {
                crate::error::PetalSonicError::Engine(
                    format!("Failed to send play command: {}", e).into(),
                )
            })?;

        Ok(())
    }

    /// Pauses a playing audio source by its UUID.
    ///
    /// Sends a pause command to the audio engine thread. The audio will stop playing
    /// but retain its current playback position.
    ///
    /// # Arguments
    ///
    /// * `audio_id` - UUID of the audio source to pause
    ///
    /// # Errors
    ///
    /// Returns an error if the command fails to send to the audio engine.
    pub fn pause(&self, audio_id: Uuid) -> Result<()> {
        self.command_sender
            .send(PlaybackCommand::Pause(audio_id))
            .map_err(|e| {
                crate::error::PetalSonicError::Engine(
                    format!("Failed to send pause command: {}", e).into(),
                )
            })?;

        Ok(())
    }

    /// Stops a playing audio source by its UUID.
    ///
    /// Sends a stop command to the audio engine thread. The audio will stop playing
    /// and reset its playback position to the beginning.
    ///
    /// # Arguments
    ///
    /// * `audio_id` - UUID of the audio source to stop
    ///
    /// # Errors
    ///
    /// Returns an error if the command fails to send to the audio engine.
    pub fn stop(&self, audio_id: Uuid) -> Result<()> {
        self.command_sender
            .send(PlaybackCommand::Stop(audio_id))
            .map_err(|e| {
                crate::error::PetalSonicError::Engine(
                    format!("Failed to send stop command: {}", e).into(),
                )
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
                crate::error::PetalSonicError::Engine(
                    format!("Failed to send stop all command: {}", e).into(),
                )
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

pub struct PetalSonicAudioSource {
    pub(crate) _id: u64,
    pub(crate) position: Vec3,
    pub(crate) volume: f32,
}

impl PetalSonicAudioSource {
    pub fn position(&self) -> Vec3 {
        self.position
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }
}

pub struct PetalSonicAudioListener {
    pub(crate) pose: Pose,
}

impl PetalSonicAudioListener {
    pub fn pose(&self) -> Pose {
        self.pose
    }
}
