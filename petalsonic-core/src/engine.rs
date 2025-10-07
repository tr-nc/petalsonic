use crate::audio_data::StreamingResampler;
use crate::config::PetalSonicWorldDesc;
use crate::error::PetalSonicError;
use crate::error::Result;
use crate::playback::{PlaybackCommand, PlaybackInstance};
use crate::world::{PetalSonicWorld, SourceId};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use ringbuf::{
    HeapRb,
    traits::{Consumer, Observer, Producer, SplitRef},
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// Stereo frame for ring buffer
#[derive(Clone, Copy, Debug)]
struct StereoFrame {
    left: f32,
    right: f32,
}

impl Default for StereoFrame {
    fn default() -> Self {
        Self {
            left: 0.0,
            right: 0.0,
        }
    }
}

// Thread-local buffers to avoid allocations in audio callback
thread_local! {
    static WORLD_BUFFER: RefCell<Vec<f32>> = RefCell::new(Vec::new());
    static RESAMPLED_BUFFER: RefCell<Vec<f32>> = RefCell::new(Vec::new());
}

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

        // Use default buffer size - let the device decide
        let buffer_size = cpal::BufferSize::Default;
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
        let world_block_size = self.desc.block_size;
        let resampler = Self::create_resampler_if_needed(
            world_sample_rate,
            device_sample_rate,
            channels,
            world_block_size,
        )?;

        // Calculate ring buffer size: enough to store several blocks
        let ring_buffer_size = world_block_size * 8; // 8x the block size for safety
        let ring_buffer = Arc::new(Mutex::new(HeapRb::<StereoFrame>::new(ring_buffer_size)));

        log::info!("Created ring buffer with size: {} frames", ring_buffer_size);

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    Self::audio_callback(
                        data,
                        &is_running,
                        &frames_processed,
                        channels as usize,
                        &active_playback,
                        &world,
                        &resampler,
                        channels,
                        world_block_size,
                        &ring_buffer,
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
        world_block_size: usize,
    ) -> Result<Option<Arc<Mutex<StreamingResampler>>>> {
        if world_sample_rate == device_sample_rate {
            return Ok(None);
        }

        let resampler = StreamingResampler::new(
            world_sample_rate,
            device_sample_rate,
            channels,
            world_block_size,
            None, // Use default (Sinc) resampler
        )?;

        log::info!(
            "Created streaming resampler: {} Hz -> {} Hz (world block size: {})",
            world_sample_rate,
            device_sample_rate,
            world_block_size
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
        world_block_size: usize,
        ring_buffer: &Arc<Mutex<HeapRb<StereoFrame>>>,
    ) where
        T: SizedSample + FromSample<f32>,
    {
        // if not running
        if !is_running.load(Ordering::Relaxed) {
            Self::fill_silence(data);
            return;
        }

        Self::process_playback_commands(world, active_playback);

        let device_frames = data.len() / channels_usize;

        log::info!(
            "[CALLBACK] Audio callback requested {} frames",
            device_frames,
        );

        // Try to lock the ring buffer
        let Ok(mut ring_buf) = ring_buffer.try_lock() else {
            log::warn!("Failed to acquire ring buffer lock in audio callback");
            Self::fill_silence(data);
            return;
        };

        // Split the ring buffer to get producer and consumer
        let (mut producer, mut consumer) = ring_buf.split_ref();

        // Check if we need to generate more samples
        // We try to consume from the ring buffer, and if there's not enough, generate more
        loop {
            // Try to peek if we can consume at least one sample
            if consumer.is_empty() {
                // Generate more samples
                if let Some(resampler_arc) = resampler {
                    Self::generate_resampled_samples(
                        &mut producer,
                        device_frames,
                        channels_usize,
                        channels,
                        resampler_arc,
                        active_playback,
                        world_block_size,
                    );
                } else {
                    Self::generate_direct_samples(
                        &mut producer,
                        device_frames,
                        channels,
                        active_playback,
                        world_block_size,
                    );
                }
            }

            // If still empty after generation, break
            if consumer.is_empty() {
                Self::fill_silence(data);
                drop(consumer);
                drop(producer);
                drop(ring_buf);
                return;
            }

            // Now we have some samples, try to consume them
            break;
        }

        // Consume samples from ring buffer to fill output
        let mut samples_consumed = 0;
        for i in 0..device_frames {
            if let Some(frame) = consumer.try_pop() {
                let left_idx = i * channels_usize;
                let right_idx = left_idx + 1;
                if left_idx < data.len() {
                    data[left_idx] = T::from_sample(frame.left);
                }
                if right_idx < data.len() {
                    data[right_idx] = T::from_sample(frame.right);
                }
                samples_consumed += 1;
            } else {
                // Not enough samples, generate more
                if let Some(resampler_arc) = resampler {
                    Self::generate_resampled_samples(
                        &mut producer,
                        device_frames - samples_consumed,
                        channels_usize,
                        channels,
                        resampler_arc,
                        active_playback,
                        world_block_size,
                    );
                } else {
                    Self::generate_direct_samples(
                        &mut producer,
                        device_frames - samples_consumed,
                        channels,
                        active_playback,
                        world_block_size,
                    );
                }

                // Try again to consume
                if let Some(frame) = consumer.try_pop() {
                    let left_idx = i * channels_usize;
                    let right_idx = left_idx + 1;
                    if left_idx < data.len() {
                        data[left_idx] = T::from_sample(frame.left);
                    }
                    if right_idx < data.len() {
                        data[right_idx] = T::from_sample(frame.right);
                    }
                    samples_consumed += 1;
                } else {
                    // Still not enough, fill rest with silence
                    for j in i..device_frames {
                        let left_idx = j * channels_usize;
                        let right_idx = left_idx + 1;
                        if left_idx < data.len() {
                            data[left_idx] = T::from_sample(0.0f32);
                        }
                        if right_idx < data.len() {
                            data[right_idx] = T::from_sample(0.0f32);
                        }
                    }
                    break;
                }
            }
        }

        drop(consumer);
        drop(producer);
        drop(ring_buf);

        log::info!(
            "[CALLBACK] Consumed {} frames from ring buffer (requested: {})",
            samples_consumed,
            device_frames
        );

        frames_processed.fetch_add(samples_consumed, Ordering::Relaxed);
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

    /// Process playback commands from the world and updates the active playback instances.
    fn process_playback_commands(
        world: &Arc<PetalSonicWorld>,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
    ) {
        while let Ok(command) = world.command_receiver().try_recv() {
            let Ok(mut active_playback) = active_playback.try_lock() else {
                continue;
            };

            match command {
                PlaybackCommand::Play(audio_id) => {
                    let Some(audio_data) = world.get_audio_data(audio_id) else {
                        continue;
                    };

                    active_playback
                        .entry(audio_id)
                        .or_insert_with(|| PlaybackInstance::new(audio_id, audio_data.clone()))
                        .play();
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

    /// Generate resampled samples and push to ring buffer
    fn generate_resampled_samples(
        producer: &mut impl Producer<Item = StereoFrame>,
        samples_needed: usize,
        channels_usize: usize,
        channels: u16,
        resampler_arc: &Arc<Mutex<StreamingResampler>>,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
        world_block_size: usize,
    ) {
        log::info!(
            "[GENERATE] Starting resampled generation: need {} frames",
            samples_needed
        );

        let Ok(mut resampler) = resampler_arc.try_lock() else {
            log::warn!("Failed to acquire resampler lock in generate_resampled_samples");
            return;
        };

        // Generate samples in world_block_size chunks
        let mut total_generated = 0;
        while total_generated < samples_needed {
            // Use thread-local buffers to avoid allocations
            WORLD_BUFFER.with(|buf| {
                let mut world_buffer = buf.borrow_mut();
                let world_buffer_size = world_block_size * channels_usize;
                world_buffer.resize(world_buffer_size, 0.0f32);
                world_buffer.fill(0.0f32);

                let frames_mixed =
                    Self::mix_playback_instances(&mut world_buffer, channels, active_playback);

                log::info!(
                    "[GENERATE] Mixed {} frames from audio files (world_block_size: {})",
                    frames_mixed,
                    world_block_size
                );

                RESAMPLED_BUFFER.with(|rbuf| {
                    let mut resampled_buffer = rbuf.borrow_mut();
                    // Allocate enough space for resampled output
                    let max_output_size = world_buffer_size * 2; // Conservative estimate
                    resampled_buffer.resize(max_output_size, 0.0f32);

                    match resampler.process_interleaved(&world_buffer, &mut resampled_buffer) {
                        Ok((frames_out, frames_in)) => {
                            log::info!(
                                "[GENERATE] Resampler: input {} frames → output {} frames",
                                frames_in,
                                frames_out
                            );

                            // Push all generated frames to ring buffer
                            let mut pushed = 0;
                            for i in 0..frames_out {
                                let left_idx = i * channels_usize;
                                let right_idx = left_idx + 1;
                                let frame = StereoFrame {
                                    left: *resampled_buffer.get(left_idx).unwrap_or(&0.0),
                                    right: *resampled_buffer.get(right_idx).unwrap_or(&0.0),
                                };
                                if producer.try_push(frame).is_ok() {
                                    pushed += 1;
                                } else {
                                    // Ring buffer is full
                                    log::warn!(
                                        "[GENERATE] Ring buffer full! Pushed {}/{} frames",
                                        pushed,
                                        frames_out
                                    );
                                    break;
                                }
                            }

                            log::info!(
                                "[GENERATE] Pushed {}/{} frames to ring buffer (total so far: {})",
                                pushed,
                                frames_out,
                                total_generated + pushed
                            );

                            total_generated += pushed;

                            // If we couldn't push any frames, ring buffer is full, stop trying
                            if pushed == 0 {
                                return;
                            }
                        }
                        Err(e) => {
                            log::error!("Resampling error: {}", e);
                            return;
                        }
                    }
                });
            });

            // If we've generated enough or can't push more, stop
            if total_generated >= samples_needed {
                break;
            }
        }

        log::info!(
            "[GENERATE] Finished resampled generation: generated {} frames (needed: {})",
            total_generated,
            samples_needed
        );
    }

    /// Generate direct samples (no resampling) and push to ring buffer
    fn generate_direct_samples(
        producer: &mut impl Producer<Item = StereoFrame>,
        samples_needed: usize,
        channels: u16,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
        world_block_size: usize,
    ) {
        log::info!(
            "[GENERATE] Starting direct generation (no resampling): need {} frames",
            samples_needed
        );

        let channels_usize = channels as usize;

        // Generate samples in world_block_size chunks
        let mut total_generated = 0;
        while total_generated < samples_needed {
            WORLD_BUFFER.with(|buf| {
                let mut world_buffer = buf.borrow_mut();
                let world_buffer_size = world_block_size * channels_usize;
                world_buffer.resize(world_buffer_size, 0.0f32);
                world_buffer.fill(0.0f32);

                let frames_mixed =
                    Self::mix_playback_instances(&mut world_buffer, channels, active_playback);

                log::info!(
                    "[GENERATE] Mixed {} frames from audio files (world_block_size: {})",
                    frames_mixed,
                    world_block_size
                );

                // Push all generated frames to ring buffer
                let mut pushed = 0;
                for i in 0..world_block_size {
                    let left_idx = i * channels_usize;
                    let right_idx = left_idx + 1;
                    let frame = StereoFrame {
                        left: *world_buffer.get(left_idx).unwrap_or(&0.0),
                        right: *world_buffer.get(right_idx).unwrap_or(&0.0),
                    };
                    if producer.try_push(frame).is_ok() {
                        pushed += 1;
                    } else {
                        // Ring buffer is full
                        log::warn!(
                            "[GENERATE] Ring buffer full! Pushed {}/{} frames",
                            pushed,
                            world_block_size
                        );
                        break;
                    }
                }

                log::info!(
                    "[GENERATE] Pushed {}/{} frames to ring buffer (total so far: {})",
                    pushed,
                    world_block_size,
                    total_generated + pushed
                );

                total_generated += pushed;

                // If we couldn't push any frames, ring buffer is full, stop trying
                if pushed == 0 {
                    return;
                }
            });

            // If we've generated enough or can't push more, stop
            if total_generated >= samples_needed {
                break;
            }
        }

        log::info!(
            "[GENERATE] Finished direct generation: generated {} frames (needed: {})",
            total_generated,
            samples_needed
        );
    }

    /// Mix all active playback instances into the buffer
    fn mix_playback_instances(
        world_buffer: &mut [f32],
        channels: u16,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
    ) -> usize {
        let Ok(mut active_playback) = active_playback.try_lock() else {
            log::warn!("Failed to acquire active playback lock in audio callback");
            return 0;
        };

        // only keep the instances that are not finished
        active_playback.retain(|_, instance| !instance.info.is_finished());

        let mut frames_filled_max = 0;
        for instance in active_playback.values_mut() {
            let frames_filled = instance.fill_buffer(world_buffer, channels);
            frames_filled_max = frames_filled_max.max(frames_filled);
        }

        frames_filled_max
    }
}

impl Drop for PetalSonicEngine {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
