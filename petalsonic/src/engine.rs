use crate::audio_data::{ResamplerType, StreamingResampler};
use crate::config::PetalSonicWorldDesc;
use crate::error::PetalSonicError;
use crate::error::Result;
use crate::events::{PetalSonicEvent, RenderTimingEvent};
use crate::math::Pose;
use crate::mixer;
use crate::playback::{PlaybackCommand, PlaybackInstance};
use crate::spatial::SpatialProcessor;
use crate::world::{PetalSonicWorld, SourceId};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use crossbeam_channel::{Receiver, Sender};
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
use std::time::{Duration, Instant};

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

const MASTER_HEADROOM_DB: f32 = -6.0;

/// Context for audio callback - groups related parameters to reduce argument count
///
/// The audio callback runs on the real-time audio thread and must be extremely fast
/// and lock-free to avoid audio glitches. It simply consumes pre-rendered samples
/// from the ring buffer.
struct AudioCallbackContext {
    is_running: Arc<AtomicBool>,
    frames_processed: Arc<AtomicUsize>,
    /// Consumer end of ring buffer - reads pre-rendered audio samples (lock-free)
    ring_buffer_consumer: HeapCons<StereoFrame>,
    channels: u16,
}

/// Context for render thread
///
/// The render thread runs independently from the audio callback, generating audio
/// samples ahead of time and pushing them to the ring buffer. This decoupling allows
/// the audio callback to remain simple and fast (lock-free consumption only).
struct RenderThreadContext {
    shutdown: Arc<AtomicBool>,
    active_playback: Arc<Mutex<HashMap<SourceId, PlaybackInstance>>>,
    resampler: Arc<Mutex<StreamingResampler>>,
    /// Producer end of ring buffer - writes pre-rendered audio samples (lock-free)
    ring_buffer_producer: HeapProd<StereoFrame>,
    channels: u16,
    block_size: usize,
    spatial_processor: Option<Arc<Mutex<SpatialProcessor>>>,
    /// Command receiver for playback commands (decoupled from world)
    command_receiver: Receiver<PlaybackCommand>,
    /// Engine-owned listener pose (decoupled from world lock)
    listener_pose: Arc<Mutex<Pose>>,
    /// Event sender for emitting playback events (e.g., SourceCompleted)
    event_sender: Sender<PetalSonicEvent>,
    /// Timing event sender for performance profiling
    timing_sender: Sender<RenderTimingEvent>,
    master_gain_linear: f32,
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
    event_sender: Sender<PetalSonicEvent>,
    timing_sender: Sender<RenderTimingEvent>,
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
    /// Engine-owned listener pose (decoupled from world lock for render thread access)
    listener_pose: Arc<Mutex<Pose>>,
    /// Event channel for playback events (e.g., SourceCompleted)
    /// The sender is cloned to render thread, receiver stays here for polling
    event_sender: Sender<PetalSonicEvent>,
    event_receiver: Receiver<PetalSonicEvent>,
    /// Timing channel for performance profiling
    /// The sender is cloned to render thread, receiver stays here for polling
    timing_sender: Sender<RenderTimingEvent>,
    timing_receiver: Receiver<RenderTimingEvent>,
    master_headroom_db: f32,
    master_gain_linear: f32,
}

impl PetalSonicEngine {
    /// Create a new audio engine with the given configuration and world
    pub fn new(desc: PetalSonicWorldDesc, world: Arc<PetalSonicWorld>) -> Result<Self> {
        // Initialize spatial processor
        // Use distance_scaler from world configuration (converts world units to meters)
        let spatial_processor = match SpatialProcessor::new(
            desc.sample_rate,
            desc.block_size,
            desc.distance_scaler,
            desc.hrtf_path.as_deref(),
            desc.hrtf_gain,
        ) {
            Ok(processor) => Some(Arc::new(Mutex::new(processor))),
            Err(e) => {
                log::warn!("Failed to initialize spatial audio processor: {}", e);
                log::warn!("Spatial audio will be disabled");
                None
            }
        };

        // Create event channel for playback events
        // Unbounded channel to ensure event emission never blocks the audio thread
        let (event_sender, event_receiver) = crossbeam_channel::unbounded();

        // Create timing channel for performance profiling
        // Unbounded channel to ensure timing emission never blocks the render thread
        let (timing_sender, timing_receiver) = crossbeam_channel::unbounded();

        // Initialize engine-owned listener pose from world
        // This decouples the render thread from world locks
        let initial_pose = world.listener().pose();
        let listener_pose = Arc::new(Mutex::new(initial_pose));

        // Connect the engine's listener pose to the world for automatic synchronization
        // This allows world.set_listener_pose() to automatically update the engine
        world.connect_engine_listener(listener_pose.clone());

        let master_headroom_db = MASTER_HEADROOM_DB;
        let master_gain_linear = crate::gain::db_to_linear(master_headroom_db);

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
            listener_pose,
            event_sender,
            event_receiver,
            timing_sender,
            timing_receiver,
            master_headroom_db,
            master_gain_linear,
        })
    }

    /// Set the listener pose for spatial audio processing
    ///
    /// **Note**: In most cases, you should use `world.set_listener_pose()` instead,
    /// which automatically synchronizes with the engine. This method is provided for
    /// advanced use cases where the engine is used without a world.
    ///
    /// # Arguments
    ///
    /// * `pose` - The new pose for the listener
    pub fn set_listener_pose(&self, pose: Pose) {
        if let Ok(mut listener) = self.listener_pose.lock() {
            *listener = pose;
        }
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

        log::info!(
            "PetalSonic master headroom: {} dB (linear gain {:.3})",
            self.master_headroom_db,
            self.master_gain_linear
        );

        let buffer_size = Self::select_buffer_size(&device_config);
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

        let device_name = device
            .name()
            .unwrap_or_else(|_| "Unknown output device".to_string());
        let buffer_size = match device_config.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => {
                format!("range {}..{} frames", min, max)
            }
            cpal::SupportedBufferSize::Unknown => "unknown".to_string(),
        };

        log::info!(
            "PetalSonic output device: host={:?}, name='{}', sample_rate={} Hz, channels={}, format={:?}, buffer_size={}",
            host.id(),
            device_name,
            device_config.sample_rate().0,
            device_config.channels(),
            device_config.sample_format(),
            buffer_size
        );

        Ok((device, device_config))
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

    fn select_buffer_size(device_config: &cpal::SupportedStreamConfig) -> cpal::BufferSize {
        let Some(requested) = Self::requested_buffer_size() else {
            log::info!("PetalSonic using default output buffer size");
            return cpal::BufferSize::Default;
        };

        let chosen = match device_config.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => {
                let clamped = requested.clamp(*min, *max);
                if clamped != requested {
                    log::warn!(
                        "PetalSonic output buffer size {} frames outside supported range {}..{}, clamping to {}",
                        requested,
                        min,
                        max,
                        clamped
                    );
                }
                clamped
            }
            cpal::SupportedBufferSize::Unknown => requested,
        };

        log::info!("PetalSonic forcing output buffer size: {} frames", chosen);
        cpal::BufferSize::Fixed(chosen)
    }

    fn requested_buffer_size() -> Option<u32> {
        const ENV_KEY: &str = "PETALSONIC_BUFFER_SIZE";

        if let Ok(value) = std::env::var(ENV_KEY) {
            if let Ok(parsed) = value.parse::<u32>() {
                if parsed > 0 {
                    return Some(parsed);
                }
            }

            log::warn!(
                "Invalid {} value '{}', falling back to platform defaults",
                ENV_KEY,
                value
            );
        }

        Self::platform_default_buffer_size()
    }

    #[cfg(target_os = "linux")]
    fn platform_default_buffer_size() -> Option<u32> {
        Some(128)
    }

    #[cfg(not(target_os = "linux"))]
    fn platform_default_buffer_size() -> Option<u32> {
        None
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

        // Clone event sender for passing to render thread
        let event_sender = self.event_sender.clone();

        // Clone timing sender for passing to render thread
        let timing_sender = self.timing_sender.clone();

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
                    event_sender,
                    timing_sender,
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
                    event_sender,
                    timing_sender,
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
                    event_sender,
                    timing_sender,
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
        if let Some(thread) = self.render_thread.take()
            && let Err(e) = thread.join()
        {
            log::error!("Error joining render thread: {:?}", e);
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

    /// Poll for playback events (non-blocking)
    ///
    /// Returns a vector of all events that have occurred since the last poll.
    /// This should be called regularly (e.g., each frame) to receive events like
    /// `SourceCompleted` which indicate when audio sources finish playing.
    ///
    /// # Example Flow
    ///
    /// 1. Audio finishes playing in render thread
    /// 2. `SourceCompleted` event is emitted to the channel
    /// 3. Source is auto-removed from `active_playback` (stops mixing)
    /// 4. Source remains in world storage for potential replay
    /// 5. GUI calls `poll_events()` and receives the event
    /// 6. GUI removes from UI and optionally calls `world.remove_audio_data(id)`
    pub fn poll_events(&self) -> Vec<PetalSonicEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_receiver.try_recv() {
            events.push(event);
        }
        events
    }

    /// Poll for timing events (non-blocking)
    ///
    /// Returns a vector of all timing events that have occurred since the last poll.
    /// This should be called regularly (e.g., each frame) for performance profiling.
    ///
    /// Each event contains timing information for a single render iteration:
    /// - Mixing time (microseconds)
    /// - Spatial processing time (microseconds)
    /// - Resampling time (microseconds)
    /// - Total render time (microseconds)
    pub fn poll_timing_events(&self) -> Vec<RenderTimingEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.timing_receiver.try_recv() {
            events.push(event);
        }
        events
    }

    /// Render thread loop that continuously fills the ring buffer
    ///
    /// This thread runs independently from the audio callback, generating audio samples
    /// ahead of time. It monitors the ring buffer fill level and generates more samples
    /// when space is available, keeping the buffer topped up to prevent audio underruns.
    fn render_thread_loop(mut ctx: RenderThreadContext) {
        // Watermark policy (in frames): keep buffer between 1-3 blocks,
        // refilling in 2-block chunks when under the low watermark.
        // This further reduces queue latency while retaining burst headroom.
        let low_watermark = ctx.block_size;
        let high_watermark = ctx.block_size * 3;
        let refill_chunk = ctx.block_size * 2;

        while !ctx.shutdown.load(Ordering::Relaxed) {
            // Update listener pose in spatial processor if available
            // Uses engine-owned listener pose (decoupled from world lock)
            if let Some(ref spatial_processor) = ctx.spatial_processor
                && let Ok(mut processor) = spatial_processor.try_lock()
                && let Ok(listener_pose) = ctx.listener_pose.try_lock()
                && let Err(e) = processor.set_listener_pose(*listener_pose)
            {
                log::error!("Failed to update listener pose: {}", e);
            }

            // Check ring buffer occupancy (lock-free!)
            let occupied = ctx.ring_buffer_producer.occupied_len();
            let should_generate = occupied < low_watermark;

            // Process playback commands (moved from audio_callback for realtime safety)
            // This is safe to do here because the render thread can afford to block briefly
            // on world locks, whereas the audio callback cannot
            // Now uses command receiver directly - no world dependency
            Self::process_playback_commands(&ctx.command_receiver, &ctx.active_playback);

            if should_generate {
                // Generate samples to fill the buffer (lock-free!)
                let free_space = ctx.ring_buffer_producer.vacant_len();

                if free_space > 0 {
                    let frames_to_high_watermark = high_watermark.saturating_sub(occupied);
                    let samples_to_generate =
                        free_space.min(refill_chunk).min(frames_to_high_watermark);

                    if samples_to_generate == 0 {
                        thread::sleep(Duration::from_micros(500));
                        continue;
                    }

                    let (completed_sources, looped_sources, timing) = Self::generate_samples(
                        &mut ctx.ring_buffer_producer,
                        samples_to_generate,
                        ctx.channels as usize,
                        ctx.channels,
                        ctx.master_gain_linear,
                        &ctx.resampler,
                        &ctx.active_playback,
                        ctx.block_size,
                        ctx.spatial_processor.as_ref(),
                    );

                    // Send timing event (non-blocking)
                    if let Err(e) = ctx.timing_sender.send(timing) {
                        log::error!("Failed to send timing event: {}", e);
                    }

                    // Emit SourceCompleted events for sources that finished (LoopMode::Once)
                    // This is lock-free and non-blocking since we use an unbounded channel
                    for source_id in completed_sources {
                        if let Err(e) = ctx
                            .event_sender
                            .send(PetalSonicEvent::SourceCompleted { source_id })
                        {
                            log::error!("Failed to send SourceCompleted event: {}", e);
                        }
                    }

                    // Emit SourceLooped events for sources that looped (LoopMode::Infinite)
                    for source_id in looped_sources {
                        if let Err(e) = ctx.event_sender.send(PetalSonicEvent::SourceLooped {
                            source_id,
                            loop_count: 0, // Could track actual loop count if needed
                        }) {
                            log::error!("Failed to send SourceLooped event: {}", e);
                        }
                    }
                }
            }

            // Small sleep to avoid busy-waiting
            thread::sleep(Duration::from_micros(500));
        }
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

        // ============================================================================
        // Ring Buffer Setup: Lock-Free Audio Thread Communication
        // ============================================================================
        // The ring buffer decouples audio generation from audio consumption:
        //
        // - RENDER THREAD (producer): Generates audio samples at its own pace, mixing
        //   and spatializing audio sources, then pushes samples to the ring buffer.
        //   Can take locks and perform complex processing without blocking audio.
        //
        // - AUDIO CALLBACK (consumer): Runs on real-time audio thread with strict
        //   timing requirements. Simply pops pre-rendered samples from ring buffer
        //   (lock-free operation). If buffer is empty, outputs silence (underrun).
        //
        // Benefits:
        // - Real-time safety: Audio callback never blocks on locks or complex processing
        // - Buffer against timing jitter: Render thread can work ahead to prevent underruns
        // - Performance isolation: Expensive processing happens off the audio thread
        //
        // The ring buffer stores frames at the device sample rate (after
        // resampling), not the world sample rate.
        //
        // Size calculation: Must be large enough to buffer during render thread delays,
        // but not so large that it introduces noticeable latency.

        // Capacity target: 8 blocks of headroom.
        // Steady-state fill is controlled separately by the render thread's 2-3 block
        // low/high watermarks.
        let ring_buffer_size = block_size * 8;
        let ring_buffer = HeapRb::<StereoFrame>::new(ring_buffer_size);

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
            command_receiver: params.world.command_receiver().clone(),
            listener_pose: self.listener_pose.clone(),
            event_sender: params.event_sender,
            timing_sender: params.timing_sender,
            master_gain_linear: self.master_gain_linear,
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

        // Create context for audio callback (simplified - just consumes from ring buffer)
        let mut context = AudioCallbackContext {
            is_running: params.is_running,
            frames_processed: params.frames_processed,
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
        Ok(Arc::new(Mutex::new(resampler)))
    }

    /// Main audio callback that fills the output buffer
    ///
    /// CRITICAL: This runs on the real-time audio thread with strict timing requirements.
    /// It MUST complete quickly and MUST NOT block on locks or perform heavy processing.
    ///
    /// This callback only consumes pre-rendered samples from the ring buffer (lock-free
    /// operation). If the ring buffer is empty, it outputs silence and logs an underrun
    /// warning. All actual audio processing happens in the separate render thread.
    ///
    /// Playback command processing has been moved to the render thread to avoid blocking
    /// on world locks in this realtime-critical callback.
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

        let device_frames = data.len() / channels_usize;

        // Consume samples from ring buffer to fill output (lock-free!)
        // This is the only audio generation that happens on the real-time thread
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
    ///
    /// Now takes the command receiver directly instead of the world, eliminating the need
    /// for the render thread to access world locks.
    fn process_playback_commands(
        command_receiver: &Receiver<PlaybackCommand>,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
    ) {
        // Important real-time rule:
        // - Never dequeue a command unless we *already* hold the active_playback lock.
        //   Otherwise, if locking fails after dequeue, the command would be lost.
        let Ok(mut active_playback) = active_playback.try_lock() else {
            // Can't safely mutate playback map this callback; leave commands queued.
            // They'll be processed on a later callback when the lock is available.
            log::debug!("Engine: Skipping command processing - active playback lock busy");
            return;
        };

        while let Ok(command) = command_receiver.try_recv() {
            match command {
                PlaybackCommand::Play(audio_id, audio_data, config, loop_mode) => {
                    log::debug!(
                        "Engine: Received Play command for source {} (loop mode: {:?})",
                        audio_id,
                        loop_mode
                    );

                    // Audio data is now passed directly in the command - no need to call back into world
                    let instance = active_playback.entry(audio_id).or_insert_with(|| {
                        log::debug!(
                            "Engine: Creating new PlaybackInstance for source {}",
                            audio_id
                        );
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
                    instance.play_from_beginning();
                }
                PlaybackCommand::Pause(audio_id) => {
                    log::debug!("Engine: Received Pause command for source {}", audio_id);
                    if let Some(instance) = active_playback.get_mut(&audio_id) {
                        instance.pause();
                    } else {
                        log::warn!(
                            "Engine: Cannot pause, source {} not in active playback",
                            audio_id
                        );
                    }
                }
                PlaybackCommand::Stop(audio_id) => {
                    log::debug!("Engine: Received Stop command for source {}", audio_id);
                    if active_playback.remove(&audio_id).is_some() {
                        log::debug!("Engine: Removed source {} from active playback", audio_id);
                    } else {
                        log::warn!(
                            "Engine: Cannot stop, source {} not in active playback",
                            audio_id
                        );
                    }
                }
                PlaybackCommand::UpdateConfig(audio_id, config) => {
                    log::debug!(
                        "Engine: Received UpdateConfig command for source {}",
                        audio_id
                    );
                    if let Some(instance) = active_playback.get_mut(&audio_id) {
                        instance.config = config;
                    } else {
                        log::warn!(
                            "Engine: Cannot update config, source {} not in active playback",
                            audio_id
                        );
                    }
                }
                PlaybackCommand::Seek(audio_id, progress) => {
                    log::debug!(
                        "Engine: Received Seek command for source {} to {:.2}%",
                        audio_id,
                        progress * 100.0
                    );
                    if let Some(instance) = active_playback.get_mut(&audio_id) {
                        instance.seek(progress);
                    } else {
                        log::warn!(
                            "Engine: Cannot seek, source {} not in active playback",
                            audio_id
                        );
                    }
                }
                PlaybackCommand::StopAll => {
                    active_playback.clear();
                }
            }
        }
    }

    /// Generate resampled samples and push to ring buffer
    /// Returns a tuple of (completed_sources, looped_sources, timing_event)
    #[allow(clippy::too_many_arguments)] // All parameters are necessary for this complex function
    fn generate_samples(
        producer: &mut impl Producer<Item = StereoFrame>,
        samples_needed: usize,
        channels_usize: usize,
        channels: u16,
        master_gain_linear: f32,
        resampler_arc: &Arc<Mutex<StreamingResampler>>,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
        block_size: usize,
        spatial_processor: Option<&Arc<Mutex<SpatialProcessor>>>,
    ) -> (Vec<SourceId>, Vec<SourceId>, RenderTimingEvent) {
        let total_start = Instant::now();
        let mut total_mixing_time_us = 0u64;
        let mut total_spatial_time_us = 0u64;
        let mut total_direct_mixing_time_us = 0u64;
        let mut total_spatial_simulation_time_us = 0u64;
        let mut total_ambisonics_encoding_time_us = 0u64;
        let mut total_ambisonics_decoding_time_us = 0u64;
        let mut total_resampling_time_us = 0u64;

        let Ok(mut resampler) = resampler_arc.try_lock() else {
            log::warn!("Failed to acquire resampler lock in generate_resampled_samples");
            return (
                Vec::new(),
                Vec::new(),
                RenderTimingEvent {
                    mixing_time_us: 0,
                    spatial_time_us: 0,
                    direct_mixing_time_us: 0,
                    spatial_simulation_time_us: 0,
                    ambisonics_encoding_time_us: 0,
                    ambisonics_decoding_time_us: 0,
                    resampling_time_us: 0,
                    total_time_us: 0,
                },
            );
        };

        // Track all completed and looped sources across all mixing iterations
        let mut all_completed_sources = Vec::new();
        let mut all_looped_sources = Vec::new();

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

                // Measure mixing time (includes both spatial and non-spatial)
                let mixing_start = Instant::now();

                // Use the mixer module to mix all playback instances
                // Pass spatial processor if available
                let mut spatial_processor_guard =
                    spatial_processor.and_then(|sp| sp.try_lock().ok());

                // Mix returns MixResult with completed and looped sources
                let (mix_result, mix_profiling) = mixer::mix_playback_instances_with_metrics(
                    &mut world_buffer,
                    channels,
                    active_playback,
                    spatial_processor_guard.as_deref_mut(),
                );

                let mixing_elapsed = mixing_start.elapsed();

                // Collect completed and looped sources for event emission
                all_completed_sources.extend(mix_result.completed_sources);
                all_looped_sources.extend(mix_result.looped_sources);

                // Capture coarse mixing time plus the detailed stage breakdown reported by the mixer
                total_mixing_time_us += mixing_elapsed.as_micros() as u64;
                total_direct_mixing_time_us += mix_profiling.direct_mix_time_us;
                total_spatial_time_us += mix_profiling.spatial_mix_time_us;
                if let Some(spatial_metrics) = mix_profiling.spatial_metrics {
                    total_spatial_simulation_time_us += spatial_metrics.physics_simulation_time_us;
                    total_ambisonics_encoding_time_us +=
                        spatial_metrics.ambisonics_encoding_time_us;
                    total_ambisonics_decoding_time_us +=
                        spatial_metrics.ambisonics_decoding_time_us;
                }

                RESAMPLED_BUFFER.with(|rbuf| {
                    let mut resampled_buffer = rbuf.borrow_mut();
                    // Calculate expected output size based on ratio, with some margin
                    let ratio = resampler.target_sample_rate() as f64
                        / resampler.source_sample_rate() as f64;
                    let expected_output =
                        ((block_size as f64 * ratio) as usize + 10) * channels_usize;
                    resampled_buffer.resize(expected_output, 0.0f32);

                    // Measure resampling time
                    let resampling_start = Instant::now();

                    match resampler.process_interleaved(&world_buffer, &mut resampled_buffer) {
                        Ok((frames_out, _frames_in)) => {
                            let resampling_elapsed = resampling_start.elapsed();
                            total_resampling_time_us += resampling_elapsed.as_micros() as u64;

                            apply_master_gain_and_limit(
                                &mut resampled_buffer,
                                frames_out,
                                channels_usize,
                                master_gain_linear,
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

        let total_elapsed = total_start.elapsed();

        (
            all_completed_sources,
            all_looped_sources,
            RenderTimingEvent {
                mixing_time_us: total_mixing_time_us,
                spatial_time_us: total_spatial_time_us,
                direct_mixing_time_us: total_direct_mixing_time_us,
                spatial_simulation_time_us: total_spatial_simulation_time_us,
                ambisonics_encoding_time_us: total_ambisonics_encoding_time_us,
                ambisonics_decoding_time_us: total_ambisonics_decoding_time_us,
                resampling_time_us: total_resampling_time_us,
                total_time_us: total_elapsed.as_micros() as u64,
            },
        )
    }
}

impl Drop for PetalSonicEngine {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn apply_master_gain_and_limit(
    buffer: &mut [f32],
    frames_out: usize,
    channels_usize: usize,
    master_gain_linear: f32,
) {
    let sample_count = frames_out * channels_usize;
    if sample_count == 0 {
        return;
    }

    let mut peak_after_gain = 0.0f32;
    for sample in buffer.iter_mut().take(sample_count) {
        let scaled = *sample * master_gain_linear;
        let abs = scaled.abs();
        if abs > peak_after_gain {
            peak_after_gain = abs;
        }

        if scaled > 1.0 {
            *sample = 1.0;
        } else if scaled < -1.0 {
            *sample = -1.0;
        } else {
            *sample = scaled;
        }
    }

    if peak_after_gain > 1.0 {
        log::warn!(
            "PetalSonic output clipped after master gain, peak amplitude: {:.6}",
            peak_after_gain
        );
    }
}
