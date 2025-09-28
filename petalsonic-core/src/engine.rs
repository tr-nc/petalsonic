use crate::config::PetalSonicWorldDesc;
use crate::error::Result;
use crate::playback::{PlaybackCommand, PlaybackInstance};
use crate::world::PetalSonicWorld;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use uuid::Uuid;

/// Callback function type for filling audio samples
///
/// The callback receives:
/// - `buffer`: mutable slice to fill with audio samples
/// - `sample_rate`: target sample rate for the samples
/// - `channels`: number of audio channels
///
/// Returns the number of frames actually filled (frames = samples / channels)
pub type AudioFillCallback = dyn Fn(&mut [f32], u32, u16) -> usize + Send + Sync;

/// Audio engine that manages real-time audio processing and output
pub struct PetalSonicEngine {
    desc: PetalSonicWorldDesc,
    stream: Option<cpal::Stream>,
    is_running: Arc<AtomicBool>,
    frames_processed: Arc<AtomicUsize>,
    fill_callback: Option<Arc<AudioFillCallback>>,
    world: Arc<PetalSonicWorld>,
    active_playback: Arc<std::sync::Mutex<HashMap<Uuid, PlaybackInstance>>>,
}

impl PetalSonicEngine {
    /// Create a new audio engine with the given configuration and world
    pub fn new(desc: PetalSonicWorldDesc, world: Arc<PetalSonicWorld>) -> Result<Self> {
        Ok(Self {
            desc,
            stream: None,
            is_running: Arc::new(AtomicBool::new(false)),
            frames_processed: Arc::new(AtomicUsize::new(0)),
            fill_callback: None,
            world,
            active_playback: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    /// Set the callback function that will be called to fill audio buffers
    /// This is the non-blocking callback required by the TODO
    pub fn set_fill_callback<F>(&mut self, callback: F)
    where
        F: Fn(&mut [f32], u32, u16) -> usize + Send + Sync + 'static,
    {
        self.fill_callback = Some(Arc::new(callback));
    }

    /// Start the audio engine with automatic playback management
    pub fn start(&mut self) -> Result<()> {
        if self.is_running.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Get the default audio device
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| {
            crate::error::PetalSonicError::AudioDevice("No default output device available".into())
        })?;

        // Configure the stream with the world's sample rate and settings
        let config = cpal::StreamConfig {
            channels: self.desc.channels,
            sample_rate: cpal::SampleRate(self.desc.sample_rate),
            buffer_size: cpal::BufferSize::Fixed(self.desc.block_size as u32),
        };

        let is_running = self.is_running.clone();
        let frames_processed = self.frames_processed.clone();
        let sample_rate = self.desc.sample_rate;
        let channels = self.desc.channels;
        let active_playback = self.active_playback.clone();
        let world = self.world.clone();

        // Create the stream based on the device's default format
        let default_config = device.default_output_config().map_err(|e| {
            crate::error::PetalSonicError::AudioDevice(format!(
                "Failed to get default config: {}",
                e
            ))
        })?;

        let stream = match default_config.sample_format() {
            cpal::SampleFormat::F32 => self.create_stream::<f32>(
                &device,
                &config,
                is_running,
                frames_processed,
                sample_rate,
                channels,
                active_playback,
                world,
            )?,
            cpal::SampleFormat::I16 => self.create_stream::<i16>(
                &device,
                &config,
                is_running,
                frames_processed,
                sample_rate,
                channels,
                active_playback,
                world,
            )?,
            cpal::SampleFormat::U16 => self.create_stream::<u16>(
                &device,
                &config,
                is_running,
                frames_processed,
                sample_rate,
                channels,
                active_playback,
                world,
            )?,
            _ => {
                return Err(crate::error::PetalSonicError::AudioFormat(
                    "Unsupported sample format".into(),
                ));
            }
        };

        stream.play().map_err(|e| {
            crate::error::PetalSonicError::AudioDevice(format!("Failed to start stream: {}", e))
        })?;

        self.stream = Some(stream);
        self.is_running.store(true, Ordering::Relaxed);

        Ok(())
    }

    /// Stop the audio engine
    pub fn stop(&mut self) -> Result<()> {
        if let Some(stream) = self.stream.take() {
            self.is_running.store(false, Ordering::Relaxed);
            drop(stream); // This stops the stream
        }
        Ok(())
    }

    /// Check if the engine is currently running
    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Relaxed)
    }

    /// Get the number of audio frames processed since start
    pub fn frames_processed(&self) -> usize {
        self.frames_processed.load(Ordering::Relaxed)
    }

    /// Get the engine configuration
    pub fn config(&self) -> &PetalSonicWorldDesc {
        &self.desc
    }

    /// Create a typed audio stream
    fn create_stream<T>(
        &self,
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        is_running: Arc<AtomicBool>,
        frames_processed: Arc<AtomicUsize>,
        _sample_rate: u32,
        channels: u16,
        active_playback: Arc<std::sync::Mutex<HashMap<Uuid, PlaybackInstance>>>,
        world: Arc<PetalSonicWorld>,
    ) -> Result<cpal::Stream>
    where
        T: SizedSample + FromSample<f32>,
    {
        let _channels_usize = channels as usize;

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    if !is_running.load(Ordering::Relaxed) {
                        // Fill with silence if not running
                        for sample in data.iter_mut() {
                            *sample = T::from_sample(0.0f32);
                        }
                        return;
                    }

                    // Process any pending commands from the world
                    while let Ok(command) = world.command_receiver().try_recv() {
                        if let Ok(mut active_playback) = active_playback.try_lock() {
                            match command {
                                PlaybackCommand::Play(audio_id) => {
                                    if let Some(audio_data) = world.get_audio_data(audio_id) {
                                        if let Some(instance) = active_playback.get_mut(&audio_id) {
                                            // Resume existing instance
                                            instance.play();
                                        } else {
                                            // Create new playback instance
                                            let mut instance =
                                                PlaybackInstance::new(audio_id, audio_data.clone());
                                            instance.play();
                                            active_playback.insert(audio_id, instance);
                                        }
                                    }
                                }
                                PlaybackCommand::Pause(audio_id) => {
                                    if let Some(instance) = active_playback.get_mut(&audio_id) {
                                        instance.pause();
                                    }
                                }
                                PlaybackCommand::Stop(audio_id) => {
                                    active_playback.remove(&audio_id);
                                }
                                PlaybackCommand::StopAll => {
                                    active_playback.clear();
                                }
                            }
                        }
                    }

                    // Create a temporary f32 buffer for mixing
                    let mut temp_buffer = vec![0.0f32; data.len()];
                    let mut total_frames = 0;

                    // Mix all active playback instances
                    if let Ok(mut active_playback) = active_playback.try_lock() {
                        // Remove finished instances
                        active_playback.retain(|_, instance| !instance.info.is_finished());

                        // Mix all active instances
                        for instance in active_playback.values_mut() {
                            let frames_filled = instance.fill_buffer(&mut temp_buffer, channels);
                            total_frames = total_frames.max(frames_filled);
                        }
                    }

                    // Convert and copy to the output buffer
                    for (i, sample) in data.iter_mut().enumerate() {
                        let sample_value = if i < temp_buffer.len() {
                            temp_buffer[i]
                        } else {
                            0.0f32
                        };
                        *sample = T::from_sample(sample_value);
                    }

                    // Update frame counter
                    frames_processed.fetch_add(total_frames, Ordering::Relaxed);
                },
                move |err| {
                    log::error!("Audio stream error: {}", err);
                },
                None,
            )
            .map_err(|e| {
                crate::error::PetalSonicError::AudioDevice(format!("Failed to build stream: {}", e))
            })?;

        Ok(stream)
    }
}

impl Drop for PetalSonicEngine {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
