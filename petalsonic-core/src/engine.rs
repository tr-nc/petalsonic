use crate::audio_data::{ResamplerType, StreamingResampler};
use crate::config::PetalSonicWorldDesc;
use crate::error::PetalSonicError;
use crate::error::Result;
use crate::mixer;
use crate::playback::{PlaybackCommand, PlaybackInstance};
use crate::spatial::SpatialProcessor;
use crate::world::{PetalSonicWorld, SourceId};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

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
    static WORLD_BUFFER: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
    static RESAMPLED_BUFFER: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}

/// Context for audio callback - groups related parameters to reduce argument count
struct AudioCallbackContext {
    is_running: Arc<AtomicBool>,
    frames_processed: Arc<AtomicUsize>,
    active_playback: Arc<Mutex<HashMap<SourceId, PlaybackInstance>>>,
    world: Arc<PetalSonicWorld>,
    ring_buffer_consumer: HeapCons<StereoFrame>,
    channels: u16,
}

/// Context for render thread
struct RenderThreadContext {
    shutdown: Arc<AtomicBool>,
    active_playback: Arc<Mutex<HashMap<SourceId, PlaybackInstance>>>,
    resampler: Arc<Mutex<StreamingResampler>>,
    ring_buffer_producer: HeapProd<StereoFrame>,
    channels: u16,
    block_size: usize,
    spatial_processor: Option<Arc<Mutex<SpatialProcessor>>>,
    world: Arc<PetalSonicWorld>,
}

/// Parameters for stream creation - groups related parameters to reduce argument count
struct StreamCreationParams {
    is_running: Arc<AtomicBool>,
    frames_processed: Arc<AtomicUsize>,
    world_sample_rate: u32,
    device_sample_rate: u32,
    channels: u16,
    active_playback: Arc<Mutex<HashMap<SourceId, PlaybackInstance>>>,
    world: Arc<PetalSonicWorld>,
    render_shutdown: Arc<AtomicBool>,
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
    /// Render thread handle
    render_thread: Option<thread::JoinHandle<()>>,
    /// Shutdown signal for render thread
    render_shutdown: Arc<AtomicBool>,
    /// Spatial audio processor
    spatial_processor: Option<Arc<Mutex<SpatialProcessor>>>,
}

impl PetalSonicEngine {
    /// Create a new audio engine with the given configuration and world
    pub fn new(desc: PetalSonicWorldDesc, world: Arc<PetalSonicWorld>) -> Result<Self> {
        // Initialize spatial processor
        // Use distance_scaler of 10.0 (converts game units to meters, as in reference)
        let spatial_processor = match SpatialProcessor::new(
            desc.sample_rate,
            desc.block_size,
            10.0,
            desc.hrtf_path.as_deref(),
        ) {
            Ok(processor) => {
                log::info!("Spatial audio processor initialized");
                Some(Arc::new(Mutex::new(processor)))
            }
            Err(e) => {
                log::warn!("Failed to initialize spatial audio processor: {}", e);
                log::warn!("Spatial audio will be disabled");
                None
            }
        };

        Ok(Self {
            device_sample_rate: desc.sample_rate, // Will be updated when stream starts
            desc,
            stream: None,
            is_running: Arc::new(AtomicBool::new(false)),
            frames_processed: Arc::new(AtomicUsize::new(0)),
            fill_callback: None,
            world,
            active_playback: Arc::new(std::sync::Mutex::new(HashMap::new())),
            render_thread: None,
            render_shutdown: Arc::new(AtomicBool::new(false)),
            spatial_processor,
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

        let (stream, render_thread) =
            self.build_and_start_stream(&device, &device_config, &config, device_sample_rate)?;

        self.stream = Some(stream);
        self.render_thread = Some(render_thread);
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
        &mut self,
        device: &cpal::Device,
        device_config: &cpal::SupportedStreamConfig,
        config: &cpal::StreamConfig,
        device_sample_rate: u32,
    ) -> Result<(cpal::Stream, thread::JoinHandle<()>)> {
        let is_running = self.is_running.clone();
        let frames_processed = self.frames_processed.clone();
        let world_sample_rate = self.desc.sample_rate;
        let channels = self.desc.channels;
        let active_playback = self.active_playback.clone();
        let world = self.world.clone();

        // Reset shutdown signal
        self.render_shutdown.store(false, Ordering::Relaxed);
        let render_shutdown = self.render_shutdown.clone();

        let result = match device_config.sample_format() {
            cpal::SampleFormat::F32 => self.create_stream::<f32>(
                device,
                config,
                StreamCreationParams {
                    is_running,
                    frames_processed,
                    world_sample_rate,
                    device_sample_rate,
                    channels,
                    active_playback,
                    world,
                    render_shutdown,
                },
            )?,
            cpal::SampleFormat::I16 => self.create_stream::<i16>(
                device,
                config,
                StreamCreationParams {
                    is_running,
                    frames_processed,
                    world_sample_rate,
                    device_sample_rate,
                    channels,
                    active_playback,
                    world,
                    render_shutdown,
                },
            )?,
            cpal::SampleFormat::U16 => self.create_stream::<u16>(
                device,
                config,
                StreamCreationParams {
                    is_running,
                    frames_processed,
                    world_sample_rate,
                    device_sample_rate,
                    channels,
                    active_playback,
                    world,
                    render_shutdown,
                },
            )?,
            _ => {
                return Err(PetalSonicError::AudioFormat(
                    "Unsupported sample format".into(),
                ));
            }
        };

        let (stream, render_thread) = result;

        stream
            .play()
            .map_err(|e| PetalSonicError::AudioDevice(format!("Failed to start stream: {}", e)))?;

        Ok((stream, render_thread))
    }

    /// Stop the audio engine
    pub fn stop(&mut self) -> Result<()> {
        // Signal render thread to shutdown
        self.render_shutdown.store(true, Ordering::Relaxed);

        // Stop the audio stream
        if let Some(stream) = self.stream.take() {
            self.is_running.store(false, Ordering::Relaxed);
            drop(stream); // This stops the stream
        }

        // Wait for render thread to finish
        if let Some(thread) = self.render_thread.take() {
            if let Err(e) = thread.join() {
                log::error!("Error joining render thread: {:?}", e);
            }
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

    /// Render thread loop that continuously fills the ring buffer
    fn render_thread_loop(mut ctx: RenderThreadContext) {
        log::info!("Render thread started");

        let target_buffer_fill = ctx.block_size * 4;

        while !ctx.shutdown.load(Ordering::Relaxed) {
            // Update listener pose in spatial processor if available
            if let Some(ref spatial_processor) = ctx.spatial_processor {
                if let Ok(mut processor) = spatial_processor.try_lock() {
                    let listener_pose = ctx.world.listener().pose();
                    if let Err(e) = processor.set_listener_pose(listener_pose) {
                        log::error!("Failed to update listener pose: {}", e);
                    }
                }
            }

            // Check ring buffer occupancy (lock-free!)
            let occupied = ctx.ring_buffer_producer.occupied_len();
            let should_generate = occupied < target_buffer_fill;

            if should_generate {
                // Generate samples to fill the buffer (lock-free!)
                let free_space = ctx.ring_buffer_producer.vacant_len();

                if free_space > 0 {
                    let samples_to_generate = free_space.min(ctx.block_size * 2);
                    Self::generate_samples(
                        &mut ctx.ring_buffer_producer,
                        samples_to_generate,
                        ctx.channels as usize,
                        ctx.channels,
                        &ctx.resampler,
                        &ctx.active_playback,
                        ctx.block_size,
                        ctx.spatial_processor.as_ref(),
                    );
                }
            }

            // Small sleep to avoid busy-waiting
            thread::sleep(Duration::from_micros(500));
        }

        log::info!("Render thread stopped");
    }

    /// Create a typed audio stream
    fn create_stream<T>(
        &self,
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        params: StreamCreationParams,
    ) -> Result<(cpal::Stream, thread::JoinHandle<()>)>
    where
        T: SizedSample + FromSample<f32>,
    {
        let block_size = self.desc.block_size;
        let resampler = Self::create_resampler(
            params.world_sample_rate,
            params.device_sample_rate,
            params.channels,
            block_size,
        )?;

        // TODO: the audio callback may need even more samples at a time, we should consider that too,
        // otherwise when that exceeds the ring buffer size, we will never be able to fill enough samples
        const RING_BUFFER_SIZE_MIN: usize = 100000;
        let ring_buffer_size = RING_BUFFER_SIZE_MIN.max(block_size * 8);
        let ring_buffer = HeapRb::<StereoFrame>::new(ring_buffer_size);

        log::info!("Created ring buffer with size: {} frames", ring_buffer_size);

        // Split ring buffer into producer (for render thread) and consumer (for audio callback)
        // This is lock-free! Each thread gets exclusive ownership of its half.
        let (producer, consumer) = ring_buffer.split();

        // Create context for render thread
        let render_ctx = RenderThreadContext {
            shutdown: params.render_shutdown,
            active_playback: params.active_playback.clone(),
            resampler: resampler.clone(),
            ring_buffer_producer: producer,
            channels: params.channels,
            block_size,
            spatial_processor: self.spatial_processor.clone(),
            world: params.world.clone(),
        };

        // Spawn render thread
        let render_thread = thread::Builder::new()
            .name("petalsonic-render".to_string())
            .spawn(move || {
                Self::render_thread_loop(render_ctx);
            })
            .map_err(|e| {
                PetalSonicError::AudioDevice(format!("Failed to spawn render thread: {}", e))
            })?;

        log::info!("Spawned render thread");

        // Create context for audio callback (simplified - just consumes from ring buffer)
        let mut context = AudioCallbackContext {
            is_running: params.is_running,
            frames_processed: params.frames_processed,
            active_playback: params.active_playback,
            world: params.world,
            ring_buffer_consumer: consumer,
            channels: params.channels,
        };

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    Self::audio_callback(data, &mut context);
                },
                move |err| {
                    log::error!("Audio stream error: {}", err);
                },
                None,
            )
            .map_err(|e| PetalSonicError::AudioDevice(format!("Failed to build stream: {}", e)))?;

        Ok((stream, render_thread))
    }

    /// Create a resampler (always created, handles identical sample rates internally)
    fn create_resampler(
        world_sample_rate: u32,
        device_sample_rate: u32,
        channels: u16,
        world_block_size: usize,
    ) -> Result<Arc<Mutex<StreamingResampler>>> {
        let resampler = StreamingResampler::new(
            world_sample_rate,
            device_sample_rate,
            channels,
            world_block_size,
            Some(ResamplerType::Fast),
        )?;

        if world_sample_rate == device_sample_rate {
            log::info!(
                "Created streaming resampler in bypass mode: {} Hz (world block size: {} frames)",
                world_sample_rate,
                world_block_size
            );
        } else {
            log::info!(
                "Created streaming resampler: {} Hz -> {} Hz (world block size: {} frames)",
                world_sample_rate,
                device_sample_rate,
                world_block_size
            );
        }

        Ok(Arc::new(Mutex::new(resampler)))
    }

    /// Main audio callback that fills the output buffer
    /// This is a real-time safe callback that only consumes from the ring buffer (lock-free!)
    fn audio_callback<T>(data: &mut [T], ctx: &mut AudioCallbackContext)
    where
        T: SizedSample + FromSample<f32>,
    {
        let channels_usize = ctx.channels as usize;

        // If not running, fill silence
        if !ctx.is_running.load(Ordering::Relaxed) {
            Self::fill_silence(data);
            return;
        }

        // Process playback commands (stop/pause/play)
        Self::process_playback_commands(&ctx.world, &ctx.active_playback);

        let device_frames = data.len() / channels_usize;

        // Consume samples from ring buffer to fill output (lock-free!)
        let mut samples_consumed = 0;
        for i in 0..device_frames {
            if let Some(frame) = ctx.ring_buffer_consumer.try_pop() {
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
                // Not enough samples in ring buffer, fill rest with silence
                // This indicates the render thread is falling behind
                log::warn!(
                    "Ring buffer underrun: only {} of {} frames available",
                    samples_consumed,
                    device_frames
                );
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

        ctx.frames_processed
            .fetch_add(samples_consumed, Ordering::Relaxed);
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
                PlaybackCommand::Play(audio_id, config, loop_mode) => {
                    let Some(audio_data) = world.get_audio_data(audio_id) else {
                        continue;
                    };

                    let instance = active_playback.entry(audio_id).or_insert_with(|| {
                        PlaybackInstance::new(
                            audio_id,
                            audio_data.clone(),
                            config.clone(),
                            loop_mode,
                        )
                    });

                    // Always update config and loop_mode when playing
                    instance.config = config;
                    instance.set_loop_mode(loop_mode);
                    instance.play();
                }
                PlaybackCommand::Pause(audio_id) => {
                    if let Some(instance) = active_playback.get_mut(&audio_id) {
                        instance.pause();
                    }
                }
                PlaybackCommand::Stop(audio_id) => {
                    active_playback.remove(&audio_id);
                }
                PlaybackCommand::UpdateConfig(audio_id, config) => {
                    if let Some(instance) = active_playback.get_mut(&audio_id) {
                        instance.config = config;
                    }
                }
                PlaybackCommand::StopAll => {
                    active_playback.clear();
                }
            }
        }
    }

    /// Generate resampled samples and push to ring buffer
    fn generate_samples(
        producer: &mut impl Producer<Item = StereoFrame>,
        samples_needed: usize,
        channels_usize: usize,
        channels: u16,
        resampler_arc: &Arc<Mutex<StreamingResampler>>,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
        block_size: usize,
        spatial_processor: Option<&Arc<Mutex<SpatialProcessor>>>,
    ) {
        let Ok(mut resampler) = resampler_arc.try_lock() else {
            log::warn!("Failed to acquire resampler lock in generate_resampled_samples");
            return;
        };

        // Generate samples in fixed world block_size chunks, output is variable
        let mut total_generated = 0;
        while total_generated < samples_needed {
            // Use thread-local buffers to avoid allocations
            WORLD_BUFFER.with(|buf| {
                let mut world_buffer = buf.borrow_mut();
                // Generate exactly block_size frames at world sample rate
                let world_buffer_size = block_size * channels_usize;

                world_buffer.resize(world_buffer_size, 0.0f32);
                world_buffer.fill(0.0f32);

                // Use the mixer module to mix all playback instances
                // Pass spatial processor if available
                let mut spatial_processor_guard =
                    spatial_processor.and_then(|sp| sp.try_lock().ok());

                mixer::mix_playback_instances(
                    &mut world_buffer,
                    channels,
                    active_playback,
                    spatial_processor_guard.as_deref_mut(),
                );

                RESAMPLED_BUFFER.with(|rbuf| {
                    let mut resampled_buffer = rbuf.borrow_mut();
                    // Calculate expected output size based on ratio, with some margin
                    let ratio = resampler.target_sample_rate() as f64
                        / resampler.source_sample_rate() as f64;
                    let expected_output =
                        ((block_size as f64 * ratio) as usize + 10) * channels_usize;
                    resampled_buffer.resize(expected_output, 0.0f32);

                    match resampler.process_interleaved(&world_buffer, &mut resampled_buffer) {
                        Ok((frames_out, _frames_in)) => {
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
                                    break;
                                }
                            }

                            total_generated += pushed;

                            // If we couldn't push any frames, ring buffer is full
                            if pushed == 0 {}
                        }
                        Err(e) => {
                            log::error!("Resampling error: {}", e);
                        }
                    }
                });
            });

            // If we've generated enough or can't push more, stop
            if total_generated >= samples_needed {
                break;
            }
        }
    }
}

impl Drop for PetalSonicEngine {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
