use crate::audio_data::{LoadOptions, PetalSonicAudioData, load_audio_file};
use crate::config::PetalSonicWorldDesc;
use crate::error::Result;
use crate::events::PetalSonicEvent;
use crate::math::{Pose, Vec3};
use crate::playback::PlaybackCommand;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

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

    pub fn sample_rate(&self) -> u32 {
        self.desc.sample_rate
    }

    /// Load an audio file using its original sample rate
    pub fn load_audio_file(&self, path: &str) -> Result<Arc<PetalSonicAudioData>> {
        let load_options = LoadOptions::default();

        let audio_data = load_audio_file(path, &load_options)?;
        Ok(audio_data)
    }

    /// Load an audio file with custom options using its original sample rate
    pub fn load_audio_file_with_options(
        &self,
        path: &str,
        options: LoadOptions,
    ) -> Result<Arc<PetalSonicAudioData>> {
        let audio_data = load_audio_file(path, &options)?;
        Ok(audio_data)
    }

    pub fn poll_events(&mut self) -> Vec<PetalSonicEvent> {
        Vec::new()
    }

    /// Add an audio source to the world storage and return its UUID
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

    /// Get audio data by UUID
    pub fn get_audio_data(&self, uuid: Uuid) -> Option<&Arc<PetalSonicAudioData>> {
        self.audio_data_storage.get(&uuid)
    }

    /// Remove audio data by UUID
    pub fn remove_audio_data(&mut self, uuid: Uuid) -> Option<Arc<PetalSonicAudioData>> {
        self.audio_data_storage.remove(&uuid)
    }

    /// Get all stored audio data UUIDs
    pub fn get_audio_data_uuids(&self) -> Vec<Uuid> {
        self.audio_data_storage.keys().copied().collect()
    }

    /// Start playing audio by UUID
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

    /// Pause playing audio by UUID
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

    /// Stop playing audio by UUID
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

    /// Stop all currently playing audio
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

    /// Get the command receiver for the audio engine
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
