use crate::audio_data::StreamingResampler;
use crate::config::PetalSonicWorldDesc;
use crate::error::PetalSonicError;
use crate::error::Result;
use crate::playback::{PlaybackCommand, PlaybackInstance};
use crate::world::{PetalSonicWorld, SourceId};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
    active_playback: Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
    /// The actual sample rate used by the audio device (may differ from desc.sample_rate)
    device_sample_rate: u32,
}

impl PetalSonicEngine {
    /// Create a new audio engine with the given configuration and world
    pub fn new(desc: PetalSonicWorldDesc, world: Arc<PetalSonicWorld>) -> Result<Self> {
        Ok(Self {
            device_sample_rate: desc.sample_rate, // Will be updated when stream starts
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

    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Relaxed)
    }

    /// Start the audio engine with automatic playback management
    pub fn start(&mut self) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }

        // Get the default audio device
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| {
            PetalSonicError::AudioDevice("No default output device available".into())
        })?;

        let device_default_config = device.default_output_config().map_err(|e| {
            PetalSonicError::AudioDevice(format!("Failed to get default config: {}", e))
        })?;

        // Use the device's native sample rate
        let device_sample_rate = device_default_config.sample_rate().0;
        self.device_sample_rate = device_sample_rate;

        log::info!(
            "Audio engine: world sample rate = {} Hz, device sample rate = {} Hz",
            self.desc.sample_rate,
            device_sample_rate
        );

        if self.desc.sample_rate != device_sample_rate {
            log::info!(
                "Sample rate mismatch detected. Will use real-time resampling: {} Hz -> {} Hz",
                self.desc.sample_rate,
                device_sample_rate
            );
        }

        // FIXME:
        // [Default] is used when no specific buffer size is set and uses the default behavior of the given host. Note, the default buffer size may be surprisingly large, leading to latency issues. If low latency is desired, [Fixed(FrameCount)] should be used in accordance with the SupportedBufferSize range produced by the [SupportedStreamConfig] API.
        // you should add check for if the buffer size is valid, based on the SupportedStreamConfig.

        let config = cpal::StreamConfig {
            channels: self.desc.channels,
            sample_rate: cpal::SampleRate(device_sample_rate),
            buffer_size: cpal::BufferSize::Fixed(self.desc.block_size as u32),
        };

        let is_running = self.is_running.clone();
        let frames_processed = self.frames_processed.clone();
        let world_sample_rate = self.desc.sample_rate;
        let channels = self.desc.channels;
        let active_playback = self.active_playback.clone();
        let world = self.world.clone();

        // Create the stream based on the device's default format
        let stream = match device_default_config.sample_format() {
            cpal::SampleFormat::F32 => self.create_stream::<f32>(
                &device,
                &config,
                is_running,
                frames_processed,
                world_sample_rate,
                device_sample_rate,
                channels,
                active_playback,
                world,
            )?,
            cpal::SampleFormat::I16 => self.create_stream::<i16>(
                &device,
                &config,
                is_running,
                frames_processed,
                world_sample_rate,
                device_sample_rate,
                channels,
                active_playback,
                world,
            )?,
            cpal::SampleFormat::U16 => self.create_stream::<u16>(
                &device,
                &config,
                is_running,
                frames_processed,
                world_sample_rate,
                device_sample_rate,
                channels,
                active_playback,
                world,
            )?,
            _ => {
                return Err(PetalSonicError::AudioFormat(
                    "Unsupported sample format".into(),
                ));
            }
        };

        stream
            .play()
            .map_err(|e| PetalSonicError::AudioDevice(format!("Failed to start stream: {}", e)))?;

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
        world_sample_rate: u32,
        device_sample_rate: u32,
        channels: u16,
        active_playback: Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
        world: Arc<PetalSonicWorld>,
    ) -> Result<cpal::Stream>
    where
        T: SizedSample + FromSample<f32>,
    {
        let channels_usize = channels as usize;

        // Create resampler if needed (wrapped in Arc<Mutex<>> for thread-safe sharing)
        let resampler: Option<Arc<Mutex<StreamingResampler>>> =
            if world_sample_rate != device_sample_rate {
                let output_frames = config.buffer_size.clone();
                let output_frames_usize = match output_frames {
                    cpal::BufferSize::Fixed(size) => size as usize,
                    cpal::BufferSize::Default => 512, // Fallback default
                };

                match StreamingResampler::new(
                    world_sample_rate,
                    device_sample_rate,
                    channels,
                    output_frames_usize,
                ) {
                    Ok(r) => {
                        log::info!(
                            "Created streaming resampler: {} Hz -> {} Hz (output frames: {})",
                            world_sample_rate,
                            device_sample_rate,
                            output_frames_usize
                        );
                        Some(Arc::new(Mutex::new(r)))
                    }
                    Err(e) => {
                        log::error!("Failed to create resampler: {}", e);
                        return Err(e);
                    }
                }
            } else {
                None
            };

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

                    // Determine how many frames we need at the world sample rate
                    let device_frames = data.len() / channels_usize;

                    // Now convert world_buffer to device sample rate if needed
                    if let Some(ref resampler_arc) = resampler {
                        if let Ok(mut resampler) = resampler_arc.try_lock() {
                            // Query the resampler for exactly how many input frames it needs
                            // to produce enough output to fill the device buffer
                            let mut total_output_written = 0;
                            let mut resampled_buffer = vec![0.0f32; data.len()];

                            // Keep feeding input until we fill the output buffer
                            while total_output_written < device_frames {
                                let input_frames_needed = resampler.input_frames_needed();

                                // Generate exactly the amount of input the resampler needs
                                let world_buffer_size = input_frames_needed * channels_usize;
                                let mut world_buffer = vec![0.0f32; world_buffer_size];

                                // Mix all active playback instances at world sample rate
                                if let Ok(mut active_playback) = active_playback.try_lock() {
                                    // Remove finished instances
                                    active_playback
                                        .retain(|_, instance| !instance.info.is_finished());

                                    // Mix all active instances
                                    for instance in active_playback.values_mut() {
                                        instance.fill_buffer(&mut world_buffer, channels);
                                    }
                                }

                                // Feed input to the resampler and get output
                                match resampler
                                    .process_interleaved(&world_buffer, &mut resampled_buffer)
                                {
                                    Ok((frames_out, _frames_consumed)) => {
                                        total_output_written += frames_out;

                                        // If we didn't get enough output, we need more input
                                        // The resampler will accumulate the input and try again next iteration
                                        if frames_out == 0 {
                                            // Not enough input buffered yet, break and we'll get it next callback
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        log::error!("Resampling error: {}", e);
                                        // Fill with silence on error
                                        for sample in data.iter_mut() {
                                            *sample = T::from_sample(0.0f32);
                                        }
                                        return;
                                    }
                                }

                                // If we've filled the buffer, we're done
                                if total_output_written >= device_frames {
                                    break;
                                }
                            }

                            // Convert and copy to the output buffer
                            for (i, sample) in data.iter_mut().enumerate() {
                                let sample_value = if i < resampled_buffer.len() {
                                    resampled_buffer[i]
                                } else {
                                    0.0f32
                                };
                                *sample = T::from_sample(sample_value);
                            }

                            // Update frame counter (using output frames)
                            frames_processed.fetch_add(total_output_written, Ordering::Relaxed);
                        } else {
                            // Can't lock resampler, fill with silence
                            for sample in data.iter_mut() {
                                *sample = T::from_sample(0.0f32);
                            }
                        }
                    } else {
                        // No resampling needed
                        // Create a temporary f32 buffer for mixing at world sample rate
                        let world_buffer_size = device_frames * channels_usize;
                        let mut world_buffer = vec![0.0f32; world_buffer_size];
                        let mut total_frames = 0;

                        // Mix all active playback instances at world sample rate
                        if let Ok(mut active_playback) = active_playback.try_lock() {
                            // Remove finished instances
                            active_playback.retain(|_, instance| !instance.info.is_finished());

                            // Mix all active instances
                            for instance in active_playback.values_mut() {
                                let frames_filled =
                                    instance.fill_buffer(&mut world_buffer, channels);
                                total_frames = total_frames.max(frames_filled);
                            }
                        }

                        // Directly convert world buffer to output
                        for (i, sample) in data.iter_mut().enumerate() {
                            let sample_value = if i < world_buffer.len() {
                                world_buffer[i]
                            } else {
                                0.0f32
                            };
                            *sample = T::from_sample(sample_value);
                        }

                        // Update frame counter
                        frames_processed.fetch_add(total_frames, Ordering::Relaxed);
                    }
                },
                move |err| {
                    log::error!("Audio stream error: {}", err);
                },
                None,
            )
            .map_err(|e| PetalSonicError::AudioDevice(format!("Failed to build stream: {}", e)))?;

        Ok(stream)
    }
}

impl Drop for PetalSonicEngine {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
