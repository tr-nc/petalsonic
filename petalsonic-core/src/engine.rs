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

        let (device, device_config) = Self::init_audio_device()?;
        let device_sample_rate = device_config.sample_rate().0;

        self.device_sample_rate = device_sample_rate;
        self.log_sample_rate_info(device_sample_rate);

        let buffer_size = Self::validate_buffer_size(&device_config, self.desc.block_size)?;
        let config =
            Self::create_stream_config(self.desc.channels, device_sample_rate, buffer_size);

        let stream =
            self.build_and_start_stream(&device, &device_config, &config, device_sample_rate)?;

        self.stream = Some(stream);
        self.is_running.store(true, Ordering::Relaxed);

        Ok(())
    }

    /// Initialize the audio device and retrieve its configuration
    fn init_audio_device() -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| {
            PetalSonicError::AudioDevice("No default output device available".into())
        })?;

        let device_config = device.default_output_config().map_err(|e| {
            PetalSonicError::AudioDevice(format!("Failed to get default config: {}", e))
        })?;

        Ok((device, device_config))
    }

    /// Log information about sample rates
    fn log_sample_rate_info(&self, device_sample_rate: u32) {
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
    }

    /// Validate the requested buffer size against the device's supported range
    fn validate_buffer_size(
        device_config: &cpal::SupportedStreamConfig,
        block_size: usize,
    ) -> Result<cpal::BufferSize> {
        let requested_buffer_size = block_size as u32;

        match device_config.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => {
                if requested_buffer_size < *min || requested_buffer_size > *max {
                    return Err(PetalSonicError::AudioDevice(format!(
                        "Requested buffer size {} is outside device's supported range [{}, {}]",
                        requested_buffer_size, min, max
                    )));
                }
                log::info!(
                    "Using fixed buffer size: {} frames (device supports: {} to {})",
                    requested_buffer_size,
                    min,
                    max
                );
                Ok(cpal::BufferSize::Fixed(requested_buffer_size))
            }
            cpal::SupportedBufferSize::Unknown => {
                log::warn!(
                    "Device buffer size range unknown, using requested size: {} frames",
                    requested_buffer_size
                );
                Ok(cpal::BufferSize::Fixed(requested_buffer_size))
            }
        }
    }

    /// Create the stream configuration
    fn create_stream_config(
        channels: u16,
        device_sample_rate: u32,
        buffer_size: cpal::BufferSize,
    ) -> cpal::StreamConfig {
        cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(device_sample_rate),
            buffer_size,
        }
    }

    /// Build and start the audio stream
    fn build_and_start_stream(
        &self,
        device: &cpal::Device,
        device_config: &cpal::SupportedStreamConfig,
        config: &cpal::StreamConfig,
        device_sample_rate: u32,
    ) -> Result<cpal::Stream> {
        let is_running = self.is_running.clone();
        let frames_processed = self.frames_processed.clone();
        let world_sample_rate = self.desc.sample_rate;
        let channels = self.desc.channels;
        let active_playback = self.active_playback.clone();
        let world = self.world.clone();

        let stream = match device_config.sample_format() {
            cpal::SampleFormat::F32 => self.create_stream::<f32>(
                device,
                config,
                is_running,
                frames_processed,
                world_sample_rate,
                device_sample_rate,
                channels,
                active_playback,
                world,
            )?,
            cpal::SampleFormat::I16 => self.create_stream::<i16>(
                device,
                config,
                is_running,
                frames_processed,
                world_sample_rate,
                device_sample_rate,
                channels,
                active_playback,
                world,
            )?,
            cpal::SampleFormat::U16 => self.create_stream::<u16>(
                device,
                config,
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

        Ok(stream)
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
        let resampler = Self::create_resampler_if_needed(
            world_sample_rate,
            device_sample_rate,
            channels,
            &config.buffer_size,
        )?;

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    Self::audio_callback(
                        data,
                        &is_running,
                        &frames_processed,
                        channels_usize,
                        &active_playback,
                        &world,
                        &resampler,
                        channels,
                    );
                },
                move |err| {
                    log::error!("Audio stream error: {}", err);
                },
                None,
            )
            .map_err(|e| PetalSonicError::AudioDevice(format!("Failed to build stream: {}", e)))?;

        Ok(stream)
    }

    /// Create a resampler if sample rates differ
    fn create_resampler_if_needed(
        world_sample_rate: u32,
        device_sample_rate: u32,
        channels: u16,
        buffer_size: &cpal::BufferSize,
    ) -> Result<Option<Arc<Mutex<StreamingResampler>>>> {
        if world_sample_rate == device_sample_rate {
            return Ok(None);
        }

        let output_frames_usize = match buffer_size {
            cpal::BufferSize::Fixed(size) => *size as usize,
            cpal::BufferSize::Default => 512, // Fallback default
        };

        let resampler = StreamingResampler::new(
            world_sample_rate,
            device_sample_rate,
            channels,
            output_frames_usize,
        )?;

        log::info!(
            "Created streaming resampler: {} Hz -> {} Hz (output frames: {})",
            world_sample_rate,
            device_sample_rate,
            output_frames_usize
        );

        Ok(Some(Arc::new(Mutex::new(resampler))))
    }

    /// Main audio callback that fills the output buffer
    fn audio_callback<T>(
        data: &mut [T],
        is_running: &Arc<AtomicBool>,
        frames_processed: &Arc<AtomicUsize>,
        channels_usize: usize,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
        world: &Arc<PetalSonicWorld>,
        resampler: &Option<Arc<Mutex<StreamingResampler>>>,
        channels: u16,
    ) where
        T: SizedSample + FromSample<f32>,
    {
        if !is_running.load(Ordering::Relaxed) {
            Self::fill_silence(data);
            return;
        }

        Self::process_playback_commands(world, active_playback);

        let device_frames = data.len() / channels_usize;

        if let Some(resampler_arc) = resampler {
            Self::process_with_resampling(
                data,
                device_frames,
                channels_usize,
                channels,
                resampler_arc,
                active_playback,
                frames_processed,
            );
        } else {
            Self::process_without_resampling(
                data,
                device_frames,
                channels_usize,
                channels,
                active_playback,
                frames_processed,
            );
        }
    }

    /// Fill buffer with silence
    fn fill_silence<T>(data: &mut [T])
    where
        T: SizedSample + FromSample<f32>,
    {
        for sample in data.iter_mut() {
            *sample = T::from_sample(0.0f32);
        }
    }

    /// Process pending playback commands from the world
    fn process_playback_commands(
        world: &Arc<PetalSonicWorld>,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
    ) {
        while let Ok(command) = world.command_receiver().try_recv() {
            if let Ok(mut active_playback) = active_playback.try_lock() {
                match command {
                    PlaybackCommand::Play(audio_id) => {
                        if let Some(audio_data) = world.get_audio_data(audio_id) {
                            if let Some(instance) = active_playback.get_mut(&audio_id) {
                                instance.play();
                            } else {
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
    }

    /// Process audio with resampling
    fn process_with_resampling<T>(
        data: &mut [T],
        device_frames: usize,
        channels_usize: usize,
        channels: u16,
        resampler_arc: &Arc<Mutex<StreamingResampler>>,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
        frames_processed: &Arc<AtomicUsize>,
    ) where
        T: SizedSample + FromSample<f32>,
    {
        let Ok(mut resampler) = resampler_arc.try_lock() else {
            Self::fill_silence(data);
            return;
        };

        let mut total_output_written = 0;
        let mut resampled_buffer = vec![0.0f32; data.len()];

        while total_output_written < device_frames {
            let input_frames_needed = resampler.input_frames_needed();
            let world_buffer_size = input_frames_needed * channels_usize;
            let mut world_buffer = vec![0.0f32; world_buffer_size];

            Self::mix_playback_instances(&mut world_buffer, channels, active_playback);

            match resampler.process_interleaved(&world_buffer, &mut resampled_buffer) {
                Ok((frames_out, _)) => {
                    total_output_written += frames_out;
                    if frames_out == 0 {
                        break;
                    }
                }
                Err(e) => {
                    log::error!("Resampling error: {}", e);
                    Self::fill_silence(data);
                    return;
                }
            }

            if total_output_written >= device_frames {
                break;
            }
        }

        Self::copy_to_output(data, &resampled_buffer);
        frames_processed.fetch_add(total_output_written, Ordering::Relaxed);
    }

    /// Process audio without resampling (direct path)
    fn process_without_resampling<T>(
        data: &mut [T],
        device_frames: usize,
        channels_usize: usize,
        channels: u16,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
        frames_processed: &Arc<AtomicUsize>,
    ) where
        T: SizedSample + FromSample<f32>,
    {
        let world_buffer_size = device_frames * channels_usize;
        let mut world_buffer = vec![0.0f32; world_buffer_size];

        let total_frames =
            Self::mix_playback_instances(&mut world_buffer, channels, active_playback);

        Self::copy_to_output(data, &world_buffer);
        frames_processed.fetch_add(total_frames, Ordering::Relaxed);
    }

    /// Mix all active playback instances into the buffer
    fn mix_playback_instances(
        world_buffer: &mut [f32],
        channels: u16,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
    ) -> usize {
        let Ok(mut active_playback) = active_playback.try_lock() else {
            return 0;
        };

        active_playback.retain(|_, instance| !instance.info.is_finished());

        let mut total_frames = 0;
        for instance in active_playback.values_mut() {
            let frames_filled = instance.fill_buffer(world_buffer, channels);
            total_frames = total_frames.max(frames_filled);
        }

        total_frames
    }

    /// Copy f32 buffer to typed output buffer
    fn copy_to_output<T>(data: &mut [T], source: &[f32])
    where
        T: SizedSample + FromSample<f32>,
    {
        for (i, sample) in data.iter_mut().enumerate() {
            let sample_value = source.get(i).copied().unwrap_or(0.0f32);
            *sample = T::from_sample(sample_value);
        }
    }
}

impl Drop for PetalSonicEngine {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
