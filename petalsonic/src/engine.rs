use crate::acoustic_propagation::{AcousticResponse, AcousticVoice, AcousticVoiceInput};
use crate::audio_data::{ResamplerType, StreamingResampler};
use crate::config::{
    LatencyProfile, OutputDevicePolicy, PetalSonicWorldDesc, SourceConfig, SpatialQuality,
};
use crate::domain::{BusParams, PlaybackControl, SpatialFrame};
use crate::error::PetalSonicError;
use crate::error::Result;
use crate::events::{PetalSonicEvent, RenderTimingEvent, RuntimeCounters, RuntimeState};
use crate::math::Pose;
use crate::mixer;
use crate::playback::{PlayState, PlaybackCommand, PlaybackInstance, VoiceStart};
use crate::spatial::{
    RetiredSpatialSource, SpatialProcessor, SpatialProcessorConfig, SpatialRenderContext,
};
use crate::world::{OutputPreparation, SourceId};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
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

const MASTER_HEADROOM_DB: f32 = -6.0;
const STARTUP_UNDERRUN_GRACE_CALLBACKS: usize = 8;
const OUTPUT_FADE_IN_MILLISECONDS: usize = 10;
const LOGICAL_CHANNELS: u16 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpatialBackendPlan {
    use_ambisonics: bool,
}

#[derive(Clone, Copy, Debug)]
struct RenderSchedule {
    ring_blocks: usize,
    low_water_blocks: usize,
    high_water_blocks: usize,
    normal_chunk_blocks: usize,
    catch_up_chunk_blocks: usize,
    wake_divisor: u32,
}

impl RenderSchedule {
    fn for_profile(profile: LatencyProfile) -> Self {
        match profile {
            LatencyProfile::Responsive => Self {
                ring_blocks: 4,
                low_water_blocks: 1,
                high_water_blocks: 2,
                normal_chunk_blocks: 1,
                catch_up_chunk_blocks: 1,
                wake_divisor: 8,
            },
            LatencyProfile::Balanced => Self {
                ring_blocks: 8,
                low_water_blocks: 2,
                high_water_blocks: 3,
                normal_chunk_blocks: 1,
                catch_up_chunk_blocks: 2,
                wake_divisor: 4,
            },
            LatencyProfile::Robust => Self {
                ring_blocks: 12,
                low_water_blocks: 3,
                high_water_blocks: 5,
                normal_chunk_blocks: 2,
                catch_up_chunk_blocks: 3,
                wake_divisor: 3,
            },
        }
    }
}

/// Context for audio callback - groups related parameters to reduce argument count
///
/// The audio callback runs on the real-time audio thread and must be extremely fast
/// and lock-free to avoid audio glitches. It simply consumes pre-rendered samples
/// from the ring buffer.
struct AudioCallbackContext {
    is_running: Arc<AtomicBool>,
    frames_processed: Arc<AtomicUsize>,
    underrun_count: Arc<AtomicUsize>,
    /// Consumer end of ring buffer - reads pre-rendered audio samples (lock-free)
    ring_buffer_consumer: HeapCons<StereoFrame>,
    channels: u16,
    startup_underrun_callbacks_remaining: usize,
    fade_in_remaining_frames: usize,
    fade_in_total_frames: usize,
}

#[derive(Clone)]
pub(crate) struct EngineCommandReceivers {
    regular: Receiver<PlaybackCommand>,
    lifecycle: Receiver<PlaybackCommand>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputRecoveryReason {
    StreamFailure,
    SelectionChanged,
}

impl EngineCommandReceivers {
    pub(crate) fn new(
        regular: Receiver<PlaybackCommand>,
        lifecycle: Receiver<PlaybackCommand>,
    ) -> Self {
        Self { regular, lifecycle }
    }
}

struct PumpState {
    active_playback: Arc<Mutex<HashMap<SourceId, PlaybackInstance>>>,
    active_voice_count: Arc<AtomicUsize>,
    retirement_sender: Sender<SourceId>,
    latest_spatial_frame: Arc<Mutex<Option<Arc<SpatialFrame>>>>,
    current_spatial_frame: Option<Arc<SpatialFrame>>,
    pending_spatial_retirement: Option<Arc<SpatialFrame>>,
    spatial_retirement_sender: Sender<Arc<SpatialFrame>>,
    latest_acoustic_response: Arc<Mutex<Option<Arc<AcousticResponse>>>>,
    pending_acoustic_response_retirement: Option<Arc<AcousticResponse>>,
    acoustic_response_retirement_sender: Sender<Arc<AcousticResponse>>,
    acoustic_voice_input: AcousticVoiceInput,
    resampler: Arc<Mutex<StreamingResampler>>,
    /// Producer end of ring buffer - writes pre-rendered audio samples (lock-free)
    ring_buffer_producer: HeapProd<StereoFrame>,
    channels: u16,
    block_size: usize,
    spatial_processor: Option<Arc<Mutex<SpatialProcessor>>>,
    /// Command receiver for playback commands (decoupled from world)
    command_receivers: EngineCommandReceivers,
    /// Engine-owned listener pose (decoupled from world lock)
    listener_pose: Arc<Mutex<Pose>>,
    /// Event sender for emitting playback events (e.g., SourceCompleted)
    event_sender: Sender<PetalSonicEvent>,
    /// Timing event sender for performance profiling
    timing_sender: Sender<RenderTimingEvent>,
    master_gain_linear: f32,
    buses: Vec<BusParams>,
    schedule: RenderSchedule,
    mixer_scratch: mixer::MixerScratch,
    completed_playbacks: Vec<mixer::CompletedPlayback>,
    world_buffer: Vec<f32>,
    resampled_buffer: Vec<f32>,
    counters: Arc<RuntimeCounters>,
    backend_retirement_sender: Sender<RetiredSpatialSource>,
    pending_backend_retirements: Vec<(SourceId, RetiredSpatialSource)>,
    render_block_index: u64,
}

/// Parameters for stream creation - groups related parameters to reduce argument count
struct StreamCreationParams {
    is_running: Arc<AtomicBool>,
    frames_processed: Arc<AtomicUsize>,
    world_sample_rate: u32,
    device_sample_rate: u32,
    channels: u16,
    active_playback: Arc<Mutex<HashMap<SourceId, PlaybackInstance>>>,
    command_receivers: EngineCommandReceivers,
    event_sender: Sender<PetalSonicEvent>,
    timing_sender: Sender<RenderTimingEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AudioOutputDeviceInfo {
    pub name: String,
    pub is_default: bool,
    pub aliases: Vec<String>,
}

struct AudioOutputDeviceCandidate {
    device: cpal::Device,
    info: AudioOutputDeviceInfo,
}

struct PreparedOutputDevice {
    device: cpal::Device,
    config: cpal::SupportedStreamConfig,
    _probe_stream: cpal::Stream,
}

pub(crate) struct EngineObservability {
    pub frames_processed: Arc<AtomicUsize>,
    pub underrun_count: Arc<AtomicUsize>,
    pub active_device_name: Arc<Mutex<Option<String>>>,
    pub event_receiver: Receiver<PetalSonicEvent>,
    pub timing_receiver: Receiver<RenderTimingEvent>,
    pub counters: Arc<RuntimeCounters>,
}

pub(crate) struct EngineRuntimePorts {
    frames_processed: Arc<AtomicUsize>,
    underrun_count: Arc<AtomicUsize>,
    active_device_name: Arc<Mutex<Option<String>>>,
    event_sender: Sender<PetalSonicEvent>,
    timing_sender: Sender<RenderTimingEvent>,
    counters: Arc<RuntimeCounters>,
}

pub(crate) struct EngineStartup {
    pub desc: PetalSonicWorldDesc,
    pub listener_pose: Arc<Mutex<Pose>>,
    pub active_voice_count: Arc<AtomicUsize>,
    pub retirement_sender: Sender<SourceId>,
    pub latest_spatial_frame: Arc<Mutex<Option<Arc<SpatialFrame>>>>,
    pub spatial_retirement_sender: Sender<Arc<SpatialFrame>>,
    pub latest_acoustic_response: Arc<Mutex<Option<Arc<AcousticResponse>>>>,
    pub acoustic_response_retirement_sender: Sender<Arc<AcousticResponse>>,
    pub acoustic_voice_input: AcousticVoiceInput,
    pub environmental_acoustics_enabled: Arc<AtomicBool>,
    pub ports: EngineRuntimePorts,
}

/// Audio engine that manages real-time audio processing and output
pub(crate) struct PetalSonicEngine {
    desc: PetalSonicWorldDesc,
    stream: Option<cpal::Stream>,
    prepared_output_device: Option<PreparedOutputDevice>,
    is_running: Arc<AtomicBool>,
    frames_processed: Arc<AtomicUsize>,
    underrun_count: Arc<AtomicUsize>,
    stream_error: Arc<AtomicBool>,
    current_device_name: Arc<Mutex<Option<String>>>,
    active_playback: Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
    active_voice_count: Arc<AtomicUsize>,
    retirement_sender: Sender<SourceId>,
    latest_spatial_frame: Arc<Mutex<Option<Arc<SpatialFrame>>>>,
    spatial_retirement_sender: Sender<Arc<SpatialFrame>>,
    latest_acoustic_response: Arc<Mutex<Option<Arc<AcousticResponse>>>>,
    acoustic_response_retirement_sender: Sender<Arc<AcousticResponse>>,
    acoustic_voice_input: AcousticVoiceInput,
    /// The actual sample rate used by the audio device (may differ from desc.sample_rate)
    device_sample_rate: u32,
    pump_state: Option<Arc<Mutex<PumpState>>>,
    render_thread: Option<JoinHandle<()>>,
    /// Spatial audio processor
    spatial_processor: Option<Arc<Mutex<SpatialProcessor>>>,
    /// Engine-owned listener pose (decoupled from world lock for render thread access)
    listener_pose: Arc<Mutex<Pose>>,
    /// Event channel for playback events (e.g., SourceCompleted)
    /// The sender is cloned to render thread, receiver stays here for polling
    event_sender: Sender<PetalSonicEvent>,
    /// Timing channel for performance profiling
    /// The sender is cloned to render thread, receiver stays here for polling
    timing_sender: Sender<RenderTimingEvent>,
    master_headroom_db: f32,
    master_gain_linear: f32,
    schedule: RenderSchedule,
    starting_buses: Vec<BusParams>,
    recovery_completed_playbacks: Vec<mixer::CompletedPlayback>,
    counters: Arc<RuntimeCounters>,
    backend_retirement_sender: Sender<RetiredSpatialSource>,
    backend_retirement_receiver: Receiver<RetiredSpatialSource>,
}

impl PetalSonicEngine {
    /// Create the internal engine owned by a [`PetalSonicWorld`](crate::PetalSonicWorld).
    pub(crate) fn new(startup: EngineStartup) -> Result<Self> {
        let EngineStartup {
            desc,
            listener_pose,
            active_voice_count,
            retirement_sender,
            latest_spatial_frame,
            spatial_retirement_sender,
            latest_acoustic_response,
            acoustic_response_retirement_sender,
            acoustic_voice_input,
            environmental_acoustics_enabled,
            ports,
        } = startup;
        let backend_plan = Self::resolve_spatial_backend_plan(&desc);
        let schedule = RenderSchedule::for_profile(desc.latency_profile);
        let max_voices = desc.max_voices;
        let (backend_retirement_sender, backend_retirement_receiver) =
            crossbeam_channel::bounded(max_voices);
        // Initialize spatial processor
        // Use distance_scaler from world configuration (converts world units to meters)
        let spatial_processor = SpatialProcessor::new(SpatialProcessorConfig {
            sample_rate: desc.sample_rate,
            frame_size: desc.block_size,
            max_voices: desc.max_voices,
            distance_scaler: desc.distance_scaler,
            native_hrtf_path: desc.native_hrtf_path.clone(),
            hrtf_gain: desc.hrtf_gain,
            use_ambisonics: backend_plan.use_ambisonics,
            environmental_acoustics_enabled,
        })
        .map_err(|error| PetalSonicError::BackendUnavailable {
            backend: "spatial renderer",
            reason: error.to_string(),
        })?;
        let spatial_processor = Some(Arc::new(Mutex::new(spatial_processor)));

        let master_headroom_db = MASTER_HEADROOM_DB;
        let master_gain_linear = crate::gain::db_to_linear(master_headroom_db);

        Ok(Self {
            device_sample_rate: desc.sample_rate, // Will be updated when stream starts
            desc,
            stream: None,
            prepared_output_device: None,
            is_running: Arc::new(AtomicBool::new(false)),
            frames_processed: ports.frames_processed,
            underrun_count: ports.underrun_count,
            stream_error: Arc::new(AtomicBool::new(false)),
            current_device_name: ports.active_device_name,
            active_playback: Arc::new(std::sync::Mutex::new(HashMap::with_capacity(max_voices))),
            active_voice_count,
            retirement_sender,
            latest_spatial_frame,
            spatial_retirement_sender,
            latest_acoustic_response,
            acoustic_response_retirement_sender,
            acoustic_voice_input,
            pump_state: None,
            render_thread: None,
            spatial_processor,
            listener_pose,
            event_sender: ports.event_sender,
            timing_sender: ports.timing_sender,
            counters: ports.counters,
            master_headroom_db,
            master_gain_linear,
            schedule,
            starting_buses: Vec::new(),
            recovery_completed_playbacks: Vec::with_capacity(max_voices),
            backend_retirement_sender,
            backend_retirement_receiver,
        })
    }

    fn resolve_spatial_backend_plan(desc: &PetalSonicWorldDesc) -> SpatialBackendPlan {
        match desc.spatial_quality {
            SpatialQuality::LowLatency => SpatialBackendPlan {
                use_ambisonics: false,
            },
            SpatialQuality::Balanced => SpatialBackendPlan {
                use_ambisonics: true,
            },
            SpatialQuality::HighQuality => SpatialBackendPlan {
                use_ambisonics: true,
            },
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Relaxed)
    }

    pub(crate) fn create_runtime_ports(
        desc: &PetalSonicWorldDesc,
    ) -> (EngineRuntimePorts, EngineObservability) {
        let frames_processed = Arc::new(AtomicUsize::new(0));
        let underrun_count = Arc::new(AtomicUsize::new(0));
        let active_device_name = Arc::new(Mutex::new(None));
        let counters = Arc::new(RuntimeCounters::default());
        let (event_sender, event_receiver) = crossbeam_channel::bounded(desc.event_queue_capacity);
        let (timing_sender, timing_receiver) =
            crossbeam_channel::bounded(desc.timing_queue_capacity);
        (
            EngineRuntimePorts {
                frames_processed: frames_processed.clone(),
                underrun_count: underrun_count.clone(),
                active_device_name: active_device_name.clone(),
                event_sender,
                timing_sender,
                counters: counters.clone(),
            },
            EngineObservability {
                frames_processed,
                underrun_count,
                active_device_name,
                event_receiver,
                timing_receiver,
                counters,
            },
        )
    }

    /// Start the audio engine with automatic playback management
    pub(crate) fn start(
        &mut self,
        command_receivers: EngineCommandReceivers,
        buses: Vec<BusParams>,
    ) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }

        let requested_device = match &self.desc.output_device {
            OutputDevicePolicy::FollowSystemDefault => None,
            OutputDevicePolicy::PinnedNameContains(name) => Some(name.as_str()),
        };
        let (device, device_config) = if let Some(prepared) = self.prepared_output_device.take() {
            let PreparedOutputDevice {
                device,
                config,
                _probe_stream,
            } = prepared;
            // Keep the selected device open across old-stream shutdown, then release
            // the silent probe immediately before constructing the real stream.
            drop(_probe_stream);
            (device, config)
        } else {
            Self::init_audio_device(requested_device)?
        };
        let device_name = device
            .name()
            .unwrap_or_else(|_| "Unknown output device".to_string());
        let device_sample_rate = device_config.sample_rate().0;

        self.device_sample_rate = device_sample_rate;
        self.starting_buses = buses;

        log::info!(
            "PetalSonic master headroom: {} dB (linear gain {:.3})",
            self.master_headroom_db,
            self.master_gain_linear
        );

        let buffer_size = Self::select_buffer_size(&device_config);
        let physical_channels = device_config.channels();
        let config = Self::create_stream_config(physical_channels, device_sample_rate, buffer_size);

        self.is_running.store(true, Ordering::Release);
        self.stream_error.store(false, Ordering::Release);

        let stream_result = self.build_stream(
            &device,
            &device_config,
            &config,
            device_sample_rate,
            command_receivers.clone(),
        );
        let (stream, pump_state) = match stream_result {
            Ok(result) => result,
            Err(err) if !matches!(config.buffer_size, cpal::BufferSize::Default) => {
                log::warn!(
                    "PetalSonic failed to start stream with requested output buffer size ({}); retrying with the device default buffer size",
                    err
                );
                let default_config = Self::create_stream_config(
                    physical_channels,
                    device_sample_rate,
                    cpal::BufferSize::Default,
                );
                match self.build_stream(
                    &device,
                    &device_config,
                    &default_config,
                    device_sample_rate,
                    command_receivers,
                ) {
                    Ok(result) => result,
                    Err(error) => {
                        self.is_running.store(false, Ordering::Release);
                        return Err(error);
                    }
                }
            }
            Err(err) => {
                self.is_running.store(false, Ordering::Release);
                return Err(err);
            }
        };

        // Prefill before starting the device callback so normal startup does not begin
        // with a guaranteed underrun.
        if let Ok(mut state) = pump_state.lock() {
            Self::pump_render_state(&mut state);
            Self::pump_render_state(&mut state);
        }

        if let Err(error) = stream.play() {
            self.is_running.store(false, Ordering::Release);
            return Err(PetalSonicError::AudioDevice(format!(
                "Failed to start stream: {error}"
            )));
        }

        let render_thread = match Self::spawn_render_thread(
            pump_state.clone(),
            self.is_running.clone(),
            self.desc.block_size,
            self.desc.sample_rate,
            self.schedule.wake_divisor,
        ) {
            Ok(thread) => thread,
            Err(error) => {
                self.is_running.store(false, Ordering::Release);
                drop(stream);
                return Err(error);
            }
        };

        self.stream = Some(stream);
        self.pump_state = Some(pump_state);
        self.render_thread = Some(render_thread);
        if let Ok(mut current) = self.current_device_name.lock() {
            *current = Some(device_name);
        }
        self.counters
            .output_sample_rate
            .store(device_sample_rate as usize, Ordering::Relaxed);
        self.counters
            .output_channels
            .store(physical_channels as usize, Ordering::Relaxed);
        self.counters
            .device_generation
            .fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    fn spawn_render_thread(
        pump_state: Arc<Mutex<PumpState>>,
        is_running: Arc<AtomicBool>,
        block_size: usize,
        sample_rate: u32,
        wake_divisor: u32,
    ) -> Result<JoinHandle<()>> {
        let block_duration = Duration::from_secs_f64(block_size as f64 / sample_rate as f64);
        let wake_interval = (block_duration / wake_divisor)
            .clamp(Duration::from_micros(250), Duration::from_millis(2));

        std::thread::Builder::new()
            .name("petalsonic-render".into())
            .spawn(move || {
                while is_running.load(Ordering::Acquire) {
                    if let Ok(mut state) = pump_state.lock() {
                        Self::pump_render_state(&mut state);
                    }
                    std::thread::park_timeout(wake_interval);
                }
            })
            .map_err(|error| {
                PetalSonicError::Engine(format!("Failed to start render thread: {error}"))
            })
    }

    /// Initialize the audio device and retrieve its configuration
    fn init_audio_device(
        output_device_name_contains: Option<&str>,
    ) -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
        let host = cpal::default_host();
        let device = match output_device_name_contains
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            Some(name_contains) => Self::find_output_device_by_name_contains(&host, name_contains)?,
            None => host.default_output_device().ok_or_else(|| {
                PetalSonicError::AudioDevice("No default output device available".into())
            })?,
        };

        let device_name = device
            .name()
            .unwrap_or_else(|_| "Unknown output device".to_string());
        let device_config = device.default_output_config().map_err(|e| {
            PetalSonicError::AudioDevice(format!(
                "Failed to get default config for output device '{}': {}",
                device_name, e
            ))
        })?;
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

    fn find_output_device_by_name_contains(
        host: &cpal::Host,
        name_contains: &str,
    ) -> Result<cpal::Device> {
        let candidates = Self::output_device_candidates(host)?;
        let mut matches: Vec<_> = candidates
            .into_iter()
            .filter(|candidate| Self::output_device_matches(&candidate.info, name_contains))
            .collect();

        if matches.is_empty() {
            let available_devices = Self::output_device_candidates(host)?
                .into_iter()
                .map(|candidate| candidate.info)
                .collect::<Vec<_>>();
            return Err(PetalSonicError::AudioDevice(format!(
                "No output device name or alias matches '{}'. Available output devices: {}",
                name_contains,
                Self::format_device_infos(&available_devices)
            )));
        }

        let matched_devices: Vec<_> = matches
            .iter()
            .map(|candidate| candidate.info.clone())
            .collect();
        if matched_devices.len() > 1 {
            log::warn!(
                "PetalSonic output device substring '{}' matched multiple devices: {}; using {}",
                name_contains,
                Self::format_device_infos(&matched_devices),
                Self::format_device_info(&matched_devices[0])
            );
        } else {
            log::info!(
                "PetalSonic output device substring '{}' matched {}",
                name_contains,
                Self::format_device_info(&matched_devices[0])
            );
        }

        Ok(matches.remove(0).device)
    }

    fn output_device_candidates(host: &cpal::Host) -> Result<Vec<AudioOutputDeviceCandidate>> {
        let default_name = host
            .default_output_device()
            .and_then(|device| device.name().ok());
        let alsa_card_aliases = Self::alsa_card_aliases();
        let output_devices = host.output_devices().map_err(|e| {
            PetalSonicError::AudioDevice(format!("Failed to enumerate output devices: {}", e))
        })?;

        Ok(output_devices
            .map(|device| {
                let name = device
                    .name()
                    .unwrap_or_else(|_| "Unknown output device".to_string());
                let aliases = Self::output_device_aliases(&name, &alsa_card_aliases);
                let is_default = default_name.as_deref() == Some(name.as_str());
                AudioOutputDeviceCandidate {
                    device,
                    info: AudioOutputDeviceInfo {
                        name,
                        is_default,
                        aliases,
                    },
                }
            })
            .collect())
    }

    fn output_device_matches(info: &AudioOutputDeviceInfo, name_contains: &str) -> bool {
        let requested = Self::normalize_device_name(name_contains);
        if requested.is_empty() {
            return false;
        }

        if Self::normalize_device_name(&info.name).contains(&requested) {
            return true;
        }

        info.aliases.iter().any(|alias| {
            let alias = Self::normalize_device_name(alias);
            !alias.is_empty()
                && (alias.contains(&requested) || alias.len() >= 3 && requested.contains(&alias))
        })
    }

    fn output_device_aliases(
        device_name: &str,
        alsa_card_aliases: &HashMap<String, Vec<String>>,
    ) -> Vec<String> {
        let mut aliases = Vec::new();
        for card_id in Self::alsa_card_ids_in_device_name(device_name) {
            if let Some(card_aliases) = alsa_card_aliases.get(&card_id) {
                aliases.extend(card_aliases.iter().cloned());
            }
        }
        Self::dedup_strings(&mut aliases);
        aliases
    }

    fn alsa_card_ids_in_device_name(device_name: &str) -> Vec<String> {
        let mut ids = Vec::new();
        let mut rest = device_name;
        while let Some(index) = rest.find("CARD=") {
            let after_card = &rest[index + "CARD=".len()..];
            let id = after_card
                .split(|ch: char| ch == ',' || ch == ':' || ch.is_whitespace())
                .next()
                .unwrap_or("")
                .trim();
            if !id.is_empty() {
                ids.push(id.to_string());
            }
            rest = &after_card[id.len()..];
        }
        Self::dedup_strings(&mut ids);
        ids
    }

    #[cfg(target_os = "linux")]
    fn alsa_card_aliases() -> HashMap<String, Vec<String>> {
        Self::parse_alsa_card_aliases(Path::new("/proc/asound/cards"))
    }

    #[cfg(not(target_os = "linux"))]
    fn alsa_card_aliases() -> HashMap<String, Vec<String>> {
        HashMap::new()
    }

    fn parse_alsa_card_aliases(path: &Path) -> HashMap<String, Vec<String>> {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return HashMap::new();
        };

        let mut aliases_by_id = HashMap::new();
        let mut lines = contents.lines();
        while let Some(line) = lines.next() {
            let Some(open) = line.find('[') else {
                continue;
            };
            let Some(close) = line[open + 1..].find(']').map(|offset| open + 1 + offset) else {
                continue;
            };
            let card_id = line[open + 1..close].trim();
            if card_id.is_empty() {
                continue;
            }

            let mut aliases = vec![card_id.to_string()];
            if let Some((_, display_name)) = line.split_once(" - ") {
                aliases.push(display_name.trim().to_string());
            }
            if let Some(detail_line) = lines.next() {
                let detail = detail_line
                    .trim()
                    .split_once(" at ")
                    .map(|(name, _)| name)
                    .unwrap_or_else(|| detail_line.trim());
                if !detail.is_empty() {
                    aliases.push(detail.to_string());
                }
            }

            Self::dedup_strings(&mut aliases);
            aliases_by_id.insert(card_id.to_string(), aliases);
        }

        aliases_by_id
    }

    fn dedup_strings(values: &mut Vec<String>) {
        let mut seen = Vec::new();
        values.retain(|value| {
            let normalized = Self::normalize_device_name(value);
            if normalized.is_empty() || seen.contains(&normalized) {
                false
            } else {
                seen.push(normalized);
                true
            }
        });
    }

    fn normalize_device_name(name: &str) -> String {
        name.trim().to_lowercase()
    }

    fn format_device_infos(devices: &[AudioOutputDeviceInfo]) -> String {
        if devices.is_empty() {
            return "<none>".to_string();
        }

        devices
            .iter()
            .map(Self::format_device_info)
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn format_device_info(device: &AudioOutputDeviceInfo) -> String {
        let default_marker = if device.is_default { " (default)" } else { "" };
        if device.aliases.is_empty() {
            return format!("'{}'{}", device.name, default_marker);
        }

        format!(
            "'{}'{} (aliases: {})",
            device.name,
            default_marker,
            device
                .aliases
                .iter()
                .map(|alias| format!("'{}'", alias))
                .collect::<Vec<_>>()
                .join(", ")
        )
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
            if let Ok(parsed) = value.parse::<u32>()
                && parsed > 0
            {
                return Some(parsed);
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
        // Keep Linux fallback at 256 frames. Smaller fixed periods (e.g. 128 at 44.1 kHz)
        // can push ALSA/bridge backends over their scheduling budget, which may trigger
        // backend XRUN recovery artifacts (echo/phasey repeats, crackles) even when our
        // own ring buffer does not report an underrun.
        Some(1024)
    }

    #[cfg(not(target_os = "linux"))]
    fn platform_default_buffer_size() -> Option<u32> {
        None
    }

    /// Build an audio stream. Starting it is deliberately separate so the render
    /// buffer can be prefilled before the first device callback.
    fn build_stream(
        &mut self,
        device: &cpal::Device,
        device_config: &cpal::SupportedStreamConfig,
        config: &cpal::StreamConfig,
        device_sample_rate: u32,
        command_receivers: EngineCommandReceivers,
    ) -> Result<(cpal::Stream, Arc<Mutex<PumpState>>)> {
        let is_running = self.is_running.clone();
        let frames_processed = self.frames_processed.clone();
        let world_sample_rate = self.desc.sample_rate;
        let channels = LOGICAL_CHANNELS;
        let active_playback = self.active_playback.clone();
        let event_sender = self.event_sender.clone();
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
                    command_receivers: command_receivers.clone(),
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
                    command_receivers: command_receivers.clone(),
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
                    command_receivers,
                    event_sender,
                    timing_sender,
                },
            )?,
            _ => {
                return Err(PetalSonicError::PermanentDeviceFailure(
                    "Unsupported sample format".into(),
                ));
            }
        };

        Ok(result)
    }

    /// Stop the audio engine
    pub(crate) fn stop(&mut self) -> Result<()> {
        self.is_running.store(false, Ordering::Release);

        if let Some(render_thread) = self.render_thread.take() {
            render_thread.thread().unpark();
            render_thread.join().map_err(|_| {
                PetalSonicError::Engine("Render thread panicked while shutting down".into())
            })?;
        }

        // Dropping the CPAL stream stops future callbacks. The producer is already
        // quiescent, so no callback can race runtime teardown after this point.
        drop(self.stream.take());

        self.pump_state = None;
        self.drain_retired_backend_resources();
        if let Ok(mut current) = self.current_device_name.lock() {
            *current = None;
        }

        Ok(())
    }

    pub(crate) fn drain_retired_backend_resources(&mut self) {
        while self.backend_retirement_receiver.try_recv().is_ok() {}
    }

    pub(crate) fn output_recovery_reason(&self) -> Option<OutputRecoveryReason> {
        if !self.is_running() || self.stream_error.load(Ordering::Acquire) {
            return Some(OutputRecoveryReason::StreamFailure);
        }
        if !matches!(
            self.desc.output_device,
            OutputDevicePolicy::FollowSystemDefault
        ) {
            return None;
        }

        let default_name = cpal::default_host()
            .default_output_device()
            .and_then(|device| device.name().ok());
        self.current_device_name
            .lock()
            .map(|current| {
                (default_name.as_deref() != current.as_deref())
                    .then_some(OutputRecoveryReason::SelectionChanged)
            })
            .unwrap_or(Some(OutputRecoveryReason::SelectionChanged))
    }

    /// Opens the newly selected device before the current output is released.
    ///
    /// The probe stream remains paused and silent. `start` consumes the exact device
    /// and negotiated format after the old stream has stopped, avoiding a second
    /// default-device lookup during the handoff.
    pub(crate) fn prepare_selected_output(&mut self) -> OutputPreparation {
        let requested_device = match &self.desc.output_device {
            OutputDevicePolicy::FollowSystemDefault => None,
            OutputDevicePolicy::PinnedNameContains(name) => Some(name.as_str()),
        };
        let Ok((device, config)) = Self::init_audio_device(requested_device) else {
            return OutputPreparation::Unavailable;
        };
        let stream_config = Self::create_stream_config(
            config.channels(),
            config.sample_rate().0,
            cpal::BufferSize::Default,
        );
        let probe = match config.sample_format() {
            cpal::SampleFormat::F32 => Self::build_silent_probe::<f32>(&device, &stream_config),
            cpal::SampleFormat::I16 => Self::build_silent_probe::<i16>(&device, &stream_config),
            cpal::SampleFormat::U16 => Self::build_silent_probe::<u16>(&device, &stream_config),
            _ => return OutputPreparation::Unavailable,
        };
        let Ok(probe_stream) = probe else {
            // Some platform backends cannot hold two output streams at once. The
            // supervisor will perform the documented stop-then-rebuild fallback.
            return OutputPreparation::RequiresStop;
        };
        self.prepared_output_device = Some(PreparedOutputDevice {
            device,
            config,
            _probe_stream: probe_stream,
        });
        OutputPreparation::Ready
    }

    fn build_silent_probe<T>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
    ) -> Result<cpal::Stream>
    where
        T: SizedSample + FromSample<f32>,
    {
        device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    for sample in data {
                        *sample = T::from_sample(0.0);
                    }
                },
                move |_error| {},
                None,
            )
            .map_err(|error| {
                PetalSonicError::AudioDevice(format!(
                    "Failed to open selected output device: {error}"
                ))
            })
    }

    pub(crate) fn emit_runtime_state(&self, state: RuntimeState) {
        Self::try_send_event(
            &self.event_sender,
            &self.counters,
            PetalSonicEvent::RuntimeStateChanged(state),
        );
    }

    fn try_send_event(
        sender: &Sender<PetalSonicEvent>,
        counters: &RuntimeCounters,
        event: PetalSonicEvent,
    ) {
        if sender.try_send(event).is_ok() {
            RuntimeCounters::observe_high_water(&counters.event_queue_high_water, sender.len());
        } else {
            counters.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn advance_without_output(
        &mut self,
        command_receivers: &EngineCommandReceivers,
        buses: &mut [BusParams],
        elapsed: Duration,
    ) {
        Self::process_playback_commands(
            command_receivers,
            &self.active_playback,
            &self.active_voice_count,
            &self.acoustic_voice_input,
            buses,
        );

        let frames = (elapsed.as_secs_f64() * self.desc.sample_rate as f64).floor() as usize;
        if frames == 0 {
            return;
        }
        let Ok(mut active) = self.active_playback.lock() else {
            return;
        };
        self.recovery_completed_playbacks.clear();
        for (voice_id, instance) in active.iter_mut() {
            if !matches!(instance.info.play_state, PlayState::Playing) {
                continue;
            }
            let bus = mixer::effective_bus_params(instance.bus_index, buses);
            if bus.paused {
                continue;
            }
            instance.set_mix_parameters(bus);
            instance.advance_silently(frames);
            let _ = instance.check_and_clear_end_flag();
            if instance.should_reclaim() {
                self.recovery_completed_playbacks
                    .push(mixer::CompletedPlayback {
                        voice_id: *voice_id,
                        emitter: instance.emitter,
                        completion_tag: instance.completion_tag,
                    });
            }
        }
        active.retain(|_, instance| !instance.should_reclaim());
        drop(active);

        if let Some(processor) = &self.spatial_processor
            && let Ok(mut processor) = processor.lock()
        {
            for completed in &self.recovery_completed_playbacks {
                let _ = processor.retire_source(completed.voice_id);
            }
        }
        for completed in &self.recovery_completed_playbacks {
            self.acoustic_voice_input.retire(completed.voice_id);
        }

        self.active_voice_count
            .fetch_sub(self.recovery_completed_playbacks.len(), Ordering::AcqRel);
        for completed in self.recovery_completed_playbacks.drain(..) {
            if let Some(tag) = completed.completion_tag {
                let _ = self.retirement_sender.try_send(completed.voice_id);
                Self::try_send_event(
                    &self.event_sender,
                    &self.counters,
                    PetalSonicEvent::PlaybackCompleted {
                        emitter: completed.emitter,
                        control: PlaybackControl {
                            world_id: completed.emitter.world_id,
                            voice_id: completed.voice_id,
                        },
                        tag,
                    },
                );
            }
        }
    }

    /// Create a typed audio stream
    fn create_stream<T>(
        &self,
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        params: StreamCreationParams,
    ) -> Result<(cpal::Stream, Arc<Mutex<PumpState>>)>
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

        let ring_buffer_size = block_size * self.schedule.ring_blocks;
        let ring_buffer = HeapRb::<StereoFrame>::new(ring_buffer_size);

        // Split ring buffer into producer (render thread) and consumer (device callback)
        // This is lock-free! Each thread gets exclusive ownership of its half.
        let (producer, consumer) = ring_buffer.split();

        let pump_state = Arc::new(Mutex::new(PumpState {
            active_playback: params.active_playback.clone(),
            active_voice_count: self.active_voice_count.clone(),
            retirement_sender: self.retirement_sender.clone(),
            latest_spatial_frame: self.latest_spatial_frame.clone(),
            current_spatial_frame: None,
            pending_spatial_retirement: None,
            spatial_retirement_sender: self.spatial_retirement_sender.clone(),
            latest_acoustic_response: self.latest_acoustic_response.clone(),
            pending_acoustic_response_retirement: None,
            acoustic_response_retirement_sender: self.acoustic_response_retirement_sender.clone(),
            acoustic_voice_input: self.acoustic_voice_input.clone(),
            resampler: resampler.clone(),
            ring_buffer_producer: producer,
            channels: config.channels,
            block_size,
            spatial_processor: self.spatial_processor.clone(),
            command_receivers: params.command_receivers,
            listener_pose: self.listener_pose.clone(),
            event_sender: params.event_sender,
            timing_sender: params.timing_sender,
            master_gain_linear: self.master_gain_linear,
            buses: self.starting_buses.clone(),
            schedule: self.schedule,
            mixer_scratch: mixer::MixerScratch::new(self.desc.max_voices),
            completed_playbacks: Vec::with_capacity(self.desc.max_voices),
            world_buffer: vec![0.0; block_size * params.channels as usize],
            resampled_buffer: vec![
                0.0;
                ((block_size as f64 * params.device_sample_rate as f64
                    / params.world_sample_rate as f64)
                    .ceil() as usize
                    + 10)
                    * params.channels as usize
            ],
            counters: self.counters.clone(),
            backend_retirement_sender: self.backend_retirement_sender.clone(),
            pending_backend_retirements: Vec::with_capacity(self.desc.max_voices),
            render_block_index: 0,
        }));

        // Create context for audio callback (simplified - just consumes from ring buffer)
        let mut context = AudioCallbackContext {
            is_running: params.is_running,
            frames_processed: params.frames_processed,
            underrun_count: self.underrun_count.clone(),
            ring_buffer_consumer: consumer,
            channels: params.channels,
            startup_underrun_callbacks_remaining: STARTUP_UNDERRUN_GRACE_CALLBACKS,
            fade_in_remaining_frames: (params.device_sample_rate as usize
                * OUTPUT_FADE_IN_MILLISECONDS
                / 1000)
                .max(1),
            fade_in_total_frames: (params.device_sample_rate as usize
                * OUTPUT_FADE_IN_MILLISECONDS
                / 1000)
                .max(1),
        };

        let stream_error = self.stream_error.clone();

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    Self::audio_callback(data, &mut context);
                },
                move |_err| {
                    // Error formatting and recovery happen on the runtime thread.
                    stream_error.store(true, Ordering::Release);
                },
                None,
            )
            .map_err(|e| PetalSonicError::AudioDevice(format!("Failed to build stream: {}", e)))?;

        Ok((stream, pump_state))
    }

    fn pump_render_state(ctx: &mut PumpState) {
        // Bounded refill policy:
        // - aim to keep roughly 3 blocks buffered
        // - do at most 1 block of work per normal pump
        // - allow up to 2 blocks only when buffer occupancy is critically low
        // This avoids the positive-feedback loop where a slow frame causes a larger refill,
        // which then makes the next frame even slower.
        let target_occupancy = ctx.block_size * ctx.schedule.high_water_blocks;
        let critical_occupancy = ctx.block_size * ctx.schedule.low_water_blocks;
        let normal_chunk = ctx.block_size * ctx.schedule.normal_chunk_blocks;
        let catch_up_chunk = ctx.block_size * ctx.schedule.catch_up_chunk_blocks;

        Self::flush_backend_retirements(ctx);
        Self::consume_latest_spatial_frame(ctx);
        Self::consume_latest_acoustic_response(ctx);

        // Update listener pose in spatial processor if available.
        if let Some(ref spatial_processor) = ctx.spatial_processor
            && let Ok(mut processor) = spatial_processor.try_lock()
            && let Ok(listener_pose) = ctx.listener_pose.try_lock()
        {
            let _ = processor.set_listener_pose(*listener_pose);
        }

        Self::process_playback_commands(
            &ctx.command_receivers,
            &ctx.active_playback,
            &ctx.active_voice_count,
            &ctx.acoustic_voice_input,
            &mut ctx.buses,
        );

        let occupied = ctx.ring_buffer_producer.occupied_len();
        if occupied >= target_occupancy {
            return;
        }

        let free_space = ctx.ring_buffer_producer.vacant_len();
        if free_space == 0 {
            return;
        }

        let deficit = target_occupancy.saturating_sub(occupied);
        let max_chunk = if occupied < critical_occupancy {
            catch_up_chunk
        } else {
            normal_chunk
        };
        let samples_to_generate = free_space.min(deficit).min(max_chunk);

        if samples_to_generate == 0 {
            return;
        }

        let spatial_revision = ctx
            .current_spatial_frame
            .as_ref()
            .map_or(0, |frame| frame.revision());
        let timing = Self::generate_samples(
            &mut ctx.ring_buffer_producer,
            samples_to_generate,
            ctx.channels as usize,
            ctx.channels,
            ctx.master_gain_linear,
            &ctx.resampler,
            &ctx.active_playback,
            ctx.spatial_processor.as_ref(),
            &ctx.buses,
            &mut ctx.mixer_scratch,
            &mut ctx.completed_playbacks,
            &mut ctx.world_buffer,
            &mut ctx.resampled_buffer,
            &mut ctx.render_block_index,
            spatial_revision,
            &ctx.event_sender,
            &ctx.counters,
        );

        ctx.counters.record_render_time(timing.total_time_us);
        if ctx.timing_sender.try_send(timing).is_ok() {
            RuntimeCounters::observe_high_water(
                &ctx.counters.timing_queue_high_water,
                ctx.timing_sender.len(),
            );
        } else {
            ctx.counters
                .dropped_timing_events
                .fetch_add(1, Ordering::Relaxed);
        }

        let deferred_retirements = Self::retire_completed_spatial_sources(ctx);
        for completed in &ctx.completed_playbacks {
            ctx.acoustic_voice_input.retire(completed.voice_id);
        }

        ctx.active_voice_count.fetch_sub(
            ctx.completed_playbacks.len() - deferred_retirements,
            Ordering::AcqRel,
        );
        for completed in ctx.completed_playbacks.drain(..) {
            if let Some(tag) = completed.completion_tag {
                let _ = ctx.retirement_sender.try_send(completed.voice_id);
                Self::try_send_event(
                    &ctx.event_sender,
                    &ctx.counters,
                    PetalSonicEvent::PlaybackCompleted {
                        emitter: completed.emitter,
                        control: PlaybackControl {
                            world_id: completed.emitter.world_id,
                            voice_id: completed.voice_id,
                        },
                        tag,
                    },
                );
            }
        }
    }

    fn flush_backend_retirements(ctx: &mut PumpState) {
        while let Some((voice_id, retired)) = ctx.pending_backend_retirements.pop() {
            if let Err(error) = ctx.backend_retirement_sender.try_send(retired) {
                ctx.pending_backend_retirements
                    .push((voice_id, error.into_inner()));
                break;
            }
            ctx.active_voice_count.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn retire_completed_spatial_sources(ctx: &mut PumpState) -> usize {
        let Some(processor) = &ctx.spatial_processor else {
            return 0;
        };
        let Ok(mut processor) = processor.try_lock() else {
            return 0;
        };
        let mut deferred = 0;
        for completed in &ctx.completed_playbacks {
            let Some(retired) = processor.retire_source(completed.voice_id) else {
                continue;
            };
            if let Err(error) = ctx.backend_retirement_sender.try_send(retired) {
                assert!(
                    ctx.pending_backend_retirements.len()
                        < ctx.pending_backend_retirements.capacity()
                );
                ctx.pending_backend_retirements
                    .push((completed.voice_id, error.into_inner()));
                deferred += 1;
            }
        }
        deferred
    }

    fn consume_latest_spatial_frame(ctx: &mut PumpState) {
        if let Some(pending) = ctx.pending_spatial_retirement.take() {
            match ctx.spatial_retirement_sender.try_send(pending) {
                Ok(()) => {}
                Err(TrySendError::Full(pending)) => {
                    ctx.pending_spatial_retirement = Some(pending);
                    return;
                }
                Err(TrySendError::Disconnected(pending)) => {
                    ctx.pending_spatial_retirement = Some(pending);
                    return;
                }
            }
        }

        let next = ctx
            .latest_spatial_frame
            .try_lock()
            .ok()
            .and_then(|mut latest| latest.take());
        let Some(next) = next else {
            return;
        };

        let Ok(mut active_playback) = ctx.active_playback.try_lock() else {
            if let Ok(mut latest) = ctx.latest_spatial_frame.try_lock()
                && latest.is_none()
            {
                *latest = Some(next);
            }
            return;
        };

        if let Ok(mut listener_pose) = ctx.listener_pose.try_lock() {
            *listener_pose = next.listener();
        }
        Self::apply_spatial_frame_to_voices(&next, &mut active_playback);

        if let Some(previous) = ctx.current_spatial_frame.replace(next)
            && let Err(error) = ctx.spatial_retirement_sender.try_send(previous)
        {
            ctx.pending_spatial_retirement = Some(error.into_inner());
        }
    }

    fn apply_spatial_frame_to_voices(
        frame: &SpatialFrame,
        active_playback: &mut HashMap<SourceId, PlaybackInstance>,
    ) {
        for instance in active_playback.values_mut() {
            if instance.detached {
                continue;
            }
            if let Some(spatial) = frame
                .emitters()
                .iter()
                .find(|spatial| spatial.emitter == instance.emitter)
            {
                instance.config.set_pose(spatial.pose);
            }
        }
    }

    fn consume_latest_acoustic_response(ctx: &mut PumpState) {
        if let Some(pending) = ctx.pending_acoustic_response_retirement.take() {
            match ctx.acoustic_response_retirement_sender.try_send(pending) {
                Ok(()) => {}
                Err(error) => {
                    ctx.pending_acoustic_response_retirement = Some(error.into_inner());
                    return;
                }
            }
        }

        let next = ctx
            .latest_acoustic_response
            .try_lock()
            .ok()
            .and_then(|mut latest| latest.take());
        let Some(next) = next else {
            return;
        };

        let Some(processor) = &ctx.spatial_processor else {
            if let Err(error) = ctx.acoustic_response_retirement_sender.try_send(next) {
                ctx.pending_acoustic_response_retirement = Some(error.into_inner());
            }
            return;
        };
        let Ok(mut processor) = processor.try_lock() else {
            let mut next = Some(next);
            if let Ok(mut latest) = ctx.latest_acoustic_response.try_lock()
                && latest.is_none()
            {
                *latest = next.take();
            }
            if let Some(next) = next
                && let Err(error) = ctx.acoustic_response_retirement_sender.try_send(next)
            {
                ctx.pending_acoustic_response_retirement = Some(error.into_inner());
            }
            return;
        };
        let replaced = processor.replace_acoustic_response(next);

        if let Some(previous) = replaced
            && let Err(error) = ctx.acoustic_response_retirement_sender.try_send(previous)
        {
            ctx.pending_acoustic_response_retirement = Some(error.into_inner());
        }
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
        let device_frames = data.len() / channels_usize;

        // If not running, fill silence
        if !ctx.is_running.load(Ordering::Relaxed) {
            Self::fill_silence(data);
            return;
        }

        // Consume samples from ring buffer to fill output (lock-free!)
        // This is the only audio generation that happens on the real-time thread
        let mut samples_consumed = 0;
        for i in 0..device_frames {
            if let Some(frame) = ctx.ring_buffer_consumer.try_pop() {
                let fade_gain = if ctx.fade_in_remaining_frames > 0 {
                    let completed = ctx
                        .fade_in_total_frames
                        .saturating_sub(ctx.fade_in_remaining_frames);
                    ctx.fade_in_remaining_frames -= 1;
                    completed as f32 / ctx.fade_in_total_frames.max(1) as f32
                } else {
                    1.0
                };
                let frame_start = i * channels_usize;
                if channels_usize == 1 {
                    data[frame_start] =
                        T::from_sample((frame.left + frame.right) * 0.5 * fade_gain);
                } else {
                    data[frame_start] = T::from_sample(frame.left * fade_gain);
                    data[frame_start + 1] = T::from_sample(frame.right * fade_gain);
                    for sample in &mut data[frame_start + 2..frame_start + channels_usize] {
                        *sample = T::from_sample(0.0);
                    }
                }
                samples_consumed += 1;
            } else {
                // Not enough samples in ring buffer, fill rest with silence
                // This indicates the render thread is falling behind
                if ctx.startup_underrun_callbacks_remaining > 0 {
                    ctx.startup_underrun_callbacks_remaining -= 1;
                } else {
                    ctx.underrun_count.fetch_add(1, Ordering::Relaxed);
                }
                data[i * channels_usize..].fill(T::from_sample(0.0f32));
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
        command_receivers: &EngineCommandReceivers,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
        active_voice_count: &Arc<AtomicUsize>,
        acoustic_voice_input: &AcousticVoiceInput,
        buses: &mut [BusParams],
    ) {
        // Important real-time rule:
        // - Never dequeue a command unless we *already* hold the active_playback lock.
        //   Otherwise, if locking fails after dequeue, the command would be lost.
        let Ok(mut active_playback) = active_playback.try_lock() else {
            // Can't safely mutate the playback map this quantum; leave commands queued.
            // They'll be processed by a later render quantum when the lock is available.
            return;
        };

        // Bound regular work per quantum, then always service the independent
        // lifecycle reserve. This prevents a producer from starving stop/destroy.
        for _ in 0..command_receivers.regular.capacity().unwrap_or(1) {
            let Ok(command) = command_receivers.regular.try_recv() else {
                break;
            };
            Self::apply_playback_command(
                command,
                &mut active_playback,
                active_voice_count,
                acoustic_voice_input,
                buses,
            );
        }
        while let Ok(command) = command_receivers.lifecycle.try_recv() {
            Self::apply_playback_command(
                command,
                &mut active_playback,
                active_voice_count,
                acoustic_voice_input,
                buses,
            );
        }
    }

    fn apply_playback_command(
        command: PlaybackCommand,
        active_playback: &mut HashMap<SourceId, PlaybackInstance>,
        active_voice_count: &AtomicUsize,
        acoustic_voice_input: &AcousticVoiceInput,
        buses: &mut [BusParams],
    ) {
        match command {
            PlaybackCommand::Play {
                voice_id,
                emitter,
                source,
                config,
                loop_mode,
                detached,
                completion_tag,
                bus_index,
                playback_rate,
                direct_path,
                environment_send,
                play_command_id,
                mono_scratch,
            } => {
                let acoustic_voice = match &config {
                    SourceConfig::Spatial { pose, .. } => Some(AcousticVoice {
                        voice_id,
                        emitter,
                        emitter_world_pose: *pose,
                        acoustic_priority: 1.0,
                        detached,
                        direct_path,
                        environment_send,
                    }),
                    SourceConfig::NonSpatial { .. } => None,
                };
                let mut instance = PlaybackInstance::from_source(VoiceStart {
                    emitter,
                    audio_data: source,
                    config,
                    loop_mode,
                    bus_index,
                    playback_rate,
                    direct_path,
                    environment_send,
                    play_command_id,
                    detached,
                    completion_tag,
                    mono_scratch,
                });
                instance.play_from_beginning();
                if active_playback.insert(voice_id, instance).is_some() {
                    active_voice_count.fetch_sub(1, Ordering::AcqRel);
                }
                if let Some(acoustic_voice) = acoustic_voice {
                    acoustic_voice_input.activate(acoustic_voice);
                }
            }
            PlaybackCommand::PauseVoice(voice_id) => {
                if let Some(instance) = active_playback.get_mut(&voice_id) {
                    instance.pause();
                }
            }
            PlaybackCommand::StopVoice(voice_id) => {
                if let Some(instance) = active_playback.get_mut(&voice_id) {
                    instance.begin_fade_out();
                }
            }
            PlaybackCommand::SeekVoice(voice_id, progress) => {
                if let Some(instance) = active_playback.get_mut(&voice_id) {
                    instance.seek(progress);
                }
            }
            PlaybackCommand::ResumeVoice(voice_id) => {
                if let Some(instance) = active_playback.get_mut(&voice_id) {
                    instance.resume();
                }
            }
            PlaybackCommand::SetVoiceRate(voice_id, rate) => {
                if let Some(instance) = active_playback.get_mut(&voice_id) {
                    instance.set_playback_rate(rate);
                }
            }
            PlaybackCommand::PauseEmitter(emitter) => {
                for instance in active_playback.values_mut() {
                    if instance.emitter == emitter {
                        instance.pause();
                    }
                }
            }
            PlaybackCommand::ResumeEmitter(emitter) => {
                for instance in active_playback.values_mut() {
                    if instance.emitter == emitter {
                        instance.resume();
                    }
                }
            }
            PlaybackCommand::StopEmitter(emitter) => {
                for instance in active_playback.values_mut() {
                    if instance.emitter == emitter {
                        instance.begin_fade_out();
                    }
                }
            }
            PlaybackCommand::SeekEmitter(emitter, progress) => {
                for instance in active_playback.values_mut() {
                    if instance.emitter == emitter {
                        instance.seek(progress);
                    }
                }
            }
            PlaybackCommand::DestroyEmitter(emitter) => {
                for instance in active_playback.values_mut() {
                    if instance.emitter == emitter && !instance.detached {
                        instance.begin_fade_out();
                    }
                }
            }
            PlaybackCommand::UpdateEmitter(emitter, config, bus_index) => {
                for instance in active_playback.values_mut() {
                    if instance.emitter == emitter && !instance.detached {
                        instance.config = config.clone();
                        instance.bus_index = bus_index;
                    }
                }
            }
            PlaybackCommand::UpdateBus(index, params) => {
                if let Some(bus) = buses.get_mut(index) {
                    *bus = params;
                }
            }
            PlaybackCommand::StopAll => {
                for instance in active_playback.values_mut() {
                    instance.begin_fade_out();
                }
            }
        }
    }

    /// Generate resampled samples and push to ring buffer
    /// Appends completed voices to reusable storage and returns timing data.
    #[allow(clippy::too_many_arguments)] // All parameters are necessary for this complex function
    fn generate_samples(
        producer: &mut impl Producer<Item = StereoFrame>,
        samples_needed: usize,
        channels_usize: usize,
        channels: u16,
        master_gain_linear: f32,
        resampler_arc: &Arc<Mutex<StreamingResampler>>,
        active_playback: &Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
        spatial_processor: Option<&Arc<Mutex<SpatialProcessor>>>,
        buses: &[BusParams],
        mixer_scratch: &mut mixer::MixerScratch,
        completed_playbacks: &mut Vec<mixer::CompletedPlayback>,
        world_buffer: &mut [f32],
        resampled_buffer: &mut [f32],
        render_block_index: &mut u64,
        spatial_revision: u64,
        event_sender: &Sender<PetalSonicEvent>,
        counters: &RuntimeCounters,
    ) -> RenderTimingEvent {
        let total_start = Instant::now();
        let mut total_mixing_time_us = 0u64;
        let mut total_spatial_time_us = 0u64;
        let mut total_direct_mixing_time_us = 0u64;
        let mut total_spatial_source_count = 0usize;
        let mut total_spatial_simulation_time_us = 0u64;
        let mut total_direct_processing_time_us = 0u64;
        let mut total_ambisonics_encoding_time_us = 0u64;
        let mut total_ambisonics_decoding_time_us = 0u64;
        let mut total_hrtf_rendering_time_us = 0u64;
        let mut total_late_reverb_time_us = 0u64;
        let mut total_early_reflection_time_us = 0u64;
        let mut total_native_hrtf_direction_lookup_time_us = 0u64;
        let mut total_native_hrtf_convolution_time_us = 0u64;
        let mut total_resampling_time_us = 0u64;
        completed_playbacks.clear();

        let Ok(mut resampler) = resampler_arc.try_lock() else {
            return RenderTimingEvent {
                mixing_time_us: 0,
                spatial_time_us: 0,
                direct_mixing_time_us: 0,
                spatial_source_count: 0,
                spatial_simulation_time_us: 0,
                direct_processing_time_us: 0,
                ambisonics_encoding_time_us: 0,
                ambisonics_decoding_time_us: 0,
                hrtf_rendering_time_us: 0,
                late_reverb_time_us: 0,
                early_reflection_time_us: 0,
                native_hrtf_direction_lookup_time_us: 0,
                native_hrtf_convolution_time_us: 0,
                resampling_time_us: 0,
                total_time_us: 0,
            };
        };

        // Generate samples in fixed world block_size chunks, output is variable
        let mut total_generated = 0;
        while total_generated < samples_needed {
            let generated_before = total_generated;
            // Both buffers were sized when this output session was opened. Stable
            // rendering only clears and reuses them.
            world_buffer.fill(0.0);
            let mixing_start = Instant::now();
            let mut spatial_processor_guard =
                spatial_processor.and_then(|processor| processor.try_lock().ok());
            let mix_profiling = mixer::mix_playback_instances_with_metrics(
                world_buffer,
                channels,
                active_playback,
                spatial_processor_guard.as_deref_mut(),
                buses,
                SpatialRenderContext {
                    render_block_index: *render_block_index,
                    spatial_revision,
                },
                mixer_scratch,
                completed_playbacks,
            );
            *render_block_index = render_block_index.wrapping_add(1);
            for event in mixer_scratch.drain_voice_events() {
                Self::try_send_event(event_sender, counters, event);
            }
            total_mixing_time_us += mixing_start.elapsed().as_micros() as u64;
            total_direct_mixing_time_us += mix_profiling.direct_mix_time_us;
            total_spatial_time_us += mix_profiling.spatial_mix_time_us;
            if let Some(spatial_metrics) = mix_profiling.spatial_metrics {
                total_spatial_source_count += spatial_metrics.spatial_source_count;
                total_spatial_simulation_time_us += spatial_metrics.physics_simulation_time_us;
                total_direct_processing_time_us += spatial_metrics.direct_processing_time_us;
                total_ambisonics_encoding_time_us += spatial_metrics.ambisonics_encoding_time_us;
                total_ambisonics_decoding_time_us += spatial_metrics.ambisonics_decoding_time_us;
                total_hrtf_rendering_time_us += spatial_metrics.hrtf_rendering_time_us;
                total_late_reverb_time_us += spatial_metrics.late_reverb_time_us;
                total_early_reflection_time_us += spatial_metrics.early_reflection_time_us;
                total_native_hrtf_direction_lookup_time_us +=
                    spatial_metrics.native_hrtf_direction_lookup_time_us;
                total_native_hrtf_convolution_time_us +=
                    spatial_metrics.native_hrtf_convolution_time_us;
            }

            let resampling_start = Instant::now();
            if let Ok((frames_out, _frames_in)) =
                resampler.process_interleaved(world_buffer, resampled_buffer)
            {
                total_resampling_time_us += resampling_start.elapsed().as_micros() as u64;
                apply_master_gain_and_limit(
                    resampled_buffer,
                    frames_out,
                    channels_usize,
                    master_gain_linear,
                );

                let mut pushed = 0;
                for frame_samples in resampled_buffer
                    .chunks_exact(channels_usize)
                    .take(frames_out)
                {
                    let frame = StereoFrame {
                        left: frame_samples[0],
                        right: frame_samples[1],
                    };
                    if producer.try_push(frame).is_ok() {
                        pushed += 1;
                    } else {
                        break;
                    }
                }
                total_generated += pushed;
            }

            if total_generated == generated_before {
                break;
            }

            // If we've generated enough or can't push more, stop
            if total_generated >= samples_needed {
                break;
            }
        }

        let total_elapsed = total_start.elapsed();

        RenderTimingEvent {
            mixing_time_us: total_mixing_time_us,
            spatial_time_us: total_spatial_time_us,
            direct_mixing_time_us: total_direct_mixing_time_us,
            spatial_source_count: total_spatial_source_count,
            spatial_simulation_time_us: total_spatial_simulation_time_us,
            direct_processing_time_us: total_direct_processing_time_us,
            ambisonics_encoding_time_us: total_ambisonics_encoding_time_us,
            ambisonics_decoding_time_us: total_ambisonics_decoding_time_us,
            hrtf_rendering_time_us: total_hrtf_rendering_time_us,
            late_reverb_time_us: total_late_reverb_time_us,
            early_reflection_time_us: total_early_reflection_time_us,
            native_hrtf_direction_lookup_time_us: total_native_hrtf_direction_lookup_time_us,
            native_hrtf_convolution_time_us: total_native_hrtf_convolution_time_us,
            resampling_time_us: total_resampling_time_us,
            total_time_us: total_elapsed.as_micros() as u64,
        }
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

    for sample in buffer.iter_mut().take(sample_count) {
        let scaled = *sample * master_gain_linear;
        if scaled > 1.0 {
            *sample = 1.0;
        } else if scaled < -1.0 {
            *sample = -1.0;
        } else {
            *sample = scaled;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_data::PetalSonicAudioData;
    use crate::config::SourceConfig;
    use crate::playback::LoopMode;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct CallbackProbeAllocator;

    thread_local! {
        static PROBE_ACTIVE: Cell<bool> = const { Cell::new(false) };
        static PROBE_ACTIVITY: Cell<usize> = const { Cell::new(0) };
    }

    #[global_allocator]
    static CALLBACK_PROBE_ALLOCATOR: CallbackProbeAllocator = CallbackProbeAllocator;

    unsafe impl GlobalAlloc for CallbackProbeAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            PROBE_ACTIVE.with(|active| {
                if active.get() {
                    PROBE_ACTIVITY.with(|count| count.set(count.get() + 1));
                }
            });
            // SAFETY: this allocator is only an observation wrapper around System.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            PROBE_ACTIVE.with(|active| {
                if active.get() {
                    PROBE_ACTIVITY.with(|count| count.set(count.get() + 1));
                }
            });
            // SAFETY: `ptr` and `layout` came from the delegated System allocator.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            PROBE_ACTIVE.with(|active| {
                if active.get() {
                    PROBE_ACTIVITY.with(|count| count.set(count.get() + 1));
                }
            });
            // SAFETY: arguments preserve GlobalAlloc's realloc contract.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    fn callback_memory_activity(operation: impl FnOnce()) -> usize {
        PROBE_ACTIVITY.with(|count| count.set(0));
        PROBE_ACTIVE.with(|active| active.set(true));
        operation();
        PROBE_ACTIVE.with(|active| active.set(false));
        PROBE_ACTIVITY.with(Cell::get)
    }

    fn balanced_release_baseline_value(key: &str) -> u64 {
        include_str!("../perf/balanced_near_capacity.baseline")
            .lines()
            .filter_map(|line| line.split_once('='))
            .find_map(|(candidate, value)| {
                (candidate.trim() == key).then(|| {
                    value
                        .trim()
                        .parse()
                        .unwrap_or_else(|_| panic!("invalid {key} in release performance baseline"))
                })
            })
            .unwrap_or_else(|| panic!("missing {key} in release performance baseline"))
    }

    #[test]
    fn device_callback_and_error_signal_do_not_allocate_or_free() {
        let ring_buffer = HeapRb::<StereoFrame>::new(8);
        let (mut producer, consumer) = ring_buffer.split();
        for _ in 0..4 {
            producer
                .try_push(StereoFrame {
                    left: 0.5,
                    right: -0.5,
                })
                .unwrap();
        }
        let mut context = AudioCallbackContext {
            is_running: Arc::new(AtomicBool::new(true)),
            frames_processed: Arc::new(AtomicUsize::new(0)),
            underrun_count: Arc::new(AtomicUsize::new(0)),
            ring_buffer_consumer: consumer,
            channels: 2,
            startup_underrun_callbacks_remaining: 0,
            fade_in_remaining_frames: 0,
            fade_in_total_frames: 1,
        };
        let mut output = [0.0f32; 8];
        let stream_error = AtomicBool::new(false);

        let activity = callback_memory_activity(|| {
            PetalSonicEngine::audio_callback(&mut output, &mut context);
            // This is exactly the CPAL stream-error callback operation.
            stream_error.store(true, Ordering::Release);
        });

        assert_eq!(activity, 0);
        assert_eq!(output, [0.5, -0.5, 0.5, -0.5, 0.5, -0.5, 0.5, -0.5]);
        assert!(stream_error.load(Ordering::Acquire));
    }

    #[test]
    fn device_callback_maps_logical_stereo_to_physical_layout() {
        let ring_buffer = HeapRb::<StereoFrame>::new(2);
        let (mut producer, consumer) = ring_buffer.split();
        producer
            .try_push(StereoFrame {
                left: 0.75,
                right: -0.25,
            })
            .unwrap();
        let mut context = AudioCallbackContext {
            is_running: Arc::new(AtomicBool::new(true)),
            frames_processed: Arc::new(AtomicUsize::new(0)),
            underrun_count: Arc::new(AtomicUsize::new(0)),
            ring_buffer_consumer: consumer,
            channels: 6,
            startup_underrun_callbacks_remaining: 0,
            fade_in_remaining_frames: 0,
            fade_in_total_frames: 1,
        };
        let mut output = [1.0f32; 6];

        PetalSonicEngine::audio_callback(&mut output, &mut context);

        assert_eq!(output, [0.75, -0.25, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn alsa_card_alias_parser_extracts_card_id_and_display_names() {
        let path = std::env::temp_dir().join(format!(
            "petalsonic-asound-cards-test-{}.txt",
            std::process::id()
        ));
        std::fs::write(
            &path,
            " 4 [KA3            ]: USB-Audio - FiiO KA3\n                      FiiO FiiO KA3 at usb-0000:00:14.0-11, high speed\n",
        )
        .unwrap();

        let aliases = PetalSonicEngine::parse_alsa_card_aliases(&path);
        let ka3_aliases = aliases.get("KA3").unwrap();
        assert_eq!(
            ka3_aliases,
            &vec![
                "KA3".to_string(),
                "FiiO KA3".to_string(),
                "FiiO FiiO KA3".to_string(),
            ]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn output_device_matching_accepts_linux_alias_and_pipewire_style_name() {
        let device = AudioOutputDeviceInfo {
            name: "sysdefault:CARD=KA3".to_string(),
            is_default: false,
            aliases: vec!["KA3".to_string(), "FiiO KA3".to_string()],
        };

        assert!(PetalSonicEngine::output_device_matches(&device, "ka3"));
        assert!(PetalSonicEngine::output_device_matches(
            &device,
            "FiiO KA3 Analog Stereo"
        ));
        assert!(!PetalSonicEngine::output_device_matches(&device, "EX2050S"));
    }

    #[test]
    fn public_quality_profiles_resolve_to_fixed_internal_plans() {
        let mut desc = PetalSonicWorldDesc {
            spatial_quality: SpatialQuality::LowLatency,
            ..PetalSonicWorldDesc::default()
        };
        let low_latency = PetalSonicEngine::resolve_spatial_backend_plan(&desc);
        assert!(!low_latency.use_ambisonics);

        desc.spatial_quality = SpatialQuality::Balanced;
        let balanced = PetalSonicEngine::resolve_spatial_backend_plan(&desc);
        assert!(balanced.use_ambisonics);

        desc.spatial_quality = SpatialQuality::HighQuality;
        let high_quality = PetalSonicEngine::resolve_spatial_backend_plan(&desc);
        assert!(high_quality.use_ambisonics);
    }

    #[test]
    fn latency_profiles_only_select_bounded_render_schedules() {
        let responsive = RenderSchedule::for_profile(LatencyProfile::Responsive);
        let balanced = RenderSchedule::for_profile(LatencyProfile::Balanced);
        let robust = RenderSchedule::for_profile(LatencyProfile::Robust);

        assert!(responsive.ring_blocks < balanced.ring_blocks);
        assert!(balanced.ring_blocks < robust.ring_blocks);
        for schedule in [responsive, balanced, robust] {
            assert!(schedule.low_water_blocks < schedule.high_water_blocks);
            assert!(schedule.high_water_blocks <= schedule.ring_blocks);
            assert!(schedule.catch_up_chunk_blocks <= schedule.high_water_blocks);
        }
    }

    #[test]
    fn render_thread_produces_audio_without_a_caller_pump() {
        let block_size = 256;
        let sample_rate = 48_000;
        let source_id = SourceId::from(1);
        let clip = Arc::new(PetalSonicAudioData::new(
            vec![0.25; block_size * 8],
            sample_rate,
            1,
            Duration::from_secs_f64((block_size * 8) as f64 / sample_rate as f64),
        ));

        let (command_sender, command_receiver) = crossbeam_channel::bounded(8);
        let emitter = crate::domain::Emitter {
            world_id: 1,
            index: 0,
            generation: 1,
        };
        command_sender
            .try_send(PlaybackCommand::Play {
                voice_id: source_id,
                emitter,
                source: clip,
                config: SourceConfig::non_spatial(),
                loop_mode: LoopMode::Infinite,
                detached: false,
                completion_tag: None,
                bus_index: 0,
                playback_rate: 1.0,
                direct_path: crate::domain::DirectPath::default(),
                environment_send: crate::domain::EnvironmentSend::default(),
                play_command_id: None,
                mono_scratch: vec![0.0; block_size],
            })
            .unwrap();
        let (_lifecycle_sender, lifecycle_receiver) = crossbeam_channel::bounded(8);

        let ring_buffer = HeapRb::<StereoFrame>::new(block_size * 8);
        let (producer, mut consumer) = ring_buffer.split();
        let (event_sender, _event_receiver) = crossbeam_channel::bounded(8);
        let (timing_sender, _timing_receiver) = crossbeam_channel::bounded(8);
        let (backend_retirement_sender, _backend_retirement_receiver) =
            crossbeam_channel::bounded(8);
        let pump_state = Arc::new(Mutex::new(PumpState {
            active_playback: Arc::new(Mutex::new(HashMap::new())),
            active_voice_count: Arc::new(AtomicUsize::new(1)),
            retirement_sender: crossbeam_channel::bounded(8).0,
            latest_spatial_frame: Arc::new(Mutex::new(None)),
            current_spatial_frame: None,
            pending_spatial_retirement: None,
            spatial_retirement_sender: crossbeam_channel::bounded(1).0,
            latest_acoustic_response: Arc::new(Mutex::new(None)),
            pending_acoustic_response_retirement: None,
            acoustic_response_retirement_sender: crossbeam_channel::bounded(2).0,
            acoustic_voice_input: AcousticVoiceInput::isolated(8),
            // Exercise the non-bypass path used when the physical device rate differs
            // from the world's logical 48 kHz rate.
            resampler: PetalSonicEngine::create_resampler(sample_rate, 44_100, 2, block_size)
                .unwrap(),
            ring_buffer_producer: producer,
            channels: 2,
            block_size,
            spatial_processor: None,
            command_receivers: EngineCommandReceivers::new(command_receiver, lifecycle_receiver),
            listener_pose: Arc::new(Mutex::new(Pose::default())),
            event_sender,
            timing_sender,
            master_gain_linear: 1.0,
            buses: vec![BusParams::default()],
            schedule: RenderSchedule::for_profile(LatencyProfile::Balanced),
            mixer_scratch: mixer::MixerScratch::new(8),
            completed_playbacks: Vec::with_capacity(8),
            world_buffer: vec![0.0; block_size * 2],
            resampled_buffer: vec![0.0; (block_size + 10) * 2],
            counters: Arc::new(RuntimeCounters::default()),
            backend_retirement_sender,
            pending_backend_retirements: Vec::with_capacity(8),
            render_block_index: 0,
        }));
        let is_running = Arc::new(AtomicBool::new(true));
        let render_thread = PetalSonicEngine::spawn_render_thread(
            pump_state,
            is_running.clone(),
            block_size,
            sample_rate,
            4,
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while consumer.occupied_len() == 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }

        is_running.store(false, Ordering::Release);
        render_thread.thread().unpark();
        render_thread.join().unwrap();

        let rendered = consumer
            .try_pop()
            .expect("render thread produced no frames");
        assert!(rendered.left > 0.0);
        assert!(rendered.right > 0.0);
    }

    #[test]
    fn warmed_near_capacity_balanced_render_stays_bounded_and_meets_budget() {
        const VOICES: usize = 32;
        const TIMING_SAMPLES: usize = 1_024;
        let block_size = 64;
        let sample_rate = 48_000;
        let device_sample_rate = 44_100;
        let (command_sender, command_receiver) = crossbeam_channel::bounded(VOICES);
        let (_lifecycle_sender, lifecycle_receiver) = crossbeam_channel::bounded(VOICES);
        let source = Arc::new(PetalSonicAudioData::new(
            vec![0.25 / VOICES as f32; block_size * 16],
            sample_rate,
            1,
            Duration::from_secs_f64((block_size * 16) as f64 / sample_rate as f64),
        ));
        for voice in 0..VOICES {
            command_sender
                .try_send(PlaybackCommand::Play {
                    voice_id: SourceId::from(voice as u64 + 1),
                    emitter: crate::domain::Emitter {
                        world_id: 1,
                        index: voice as u32,
                        generation: 1,
                    },
                    source: source.clone(),
                    config: SourceConfig::non_spatial(),
                    loop_mode: LoopMode::Infinite,
                    detached: false,
                    completion_tag: None,
                    bus_index: 0,
                    playback_rate: 1.0,
                    direct_path: crate::domain::DirectPath::default(),
                    environment_send: crate::domain::EnvironmentSend::default(),
                    play_command_id: None,
                    mono_scratch: vec![0.0; block_size],
                })
                .unwrap();
        }
        let ring_buffer = HeapRb::<StereoFrame>::new(block_size * 8);
        let (producer, mut consumer) = ring_buffer.split();
        let (event_sender, _event_receiver) = crossbeam_channel::bounded(VOICES);
        let (timing_sender, timing_receiver) = crossbeam_channel::bounded(VOICES);
        let (backend_retirement_sender, _backend_retirement_receiver) =
            crossbeam_channel::bounded(VOICES);
        let mut pump = PumpState {
            active_playback: Arc::new(Mutex::new(HashMap::with_capacity(VOICES))),
            active_voice_count: Arc::new(AtomicUsize::new(VOICES)),
            retirement_sender: crossbeam_channel::bounded(VOICES).0,
            latest_spatial_frame: Arc::new(Mutex::new(None)),
            current_spatial_frame: None,
            pending_spatial_retirement: None,
            spatial_retirement_sender: crossbeam_channel::bounded(1).0,
            latest_acoustic_response: Arc::new(Mutex::new(None)),
            pending_acoustic_response_retirement: None,
            acoustic_response_retirement_sender: crossbeam_channel::bounded(2).0,
            acoustic_voice_input: AcousticVoiceInput::isolated(VOICES),
            resampler: PetalSonicEngine::create_resampler(
                sample_rate,
                device_sample_rate,
                2,
                block_size,
            )
            .unwrap(),
            ring_buffer_producer: producer,
            channels: 2,
            block_size,
            spatial_processor: None,
            command_receivers: EngineCommandReceivers::new(command_receiver, lifecycle_receiver),
            listener_pose: Arc::new(Mutex::new(Pose::default())),
            event_sender,
            timing_sender,
            master_gain_linear: 1.0,
            buses: vec![BusParams::default()],
            schedule: RenderSchedule::for_profile(LatencyProfile::Balanced),
            mixer_scratch: mixer::MixerScratch::new(VOICES),
            completed_playbacks: Vec::with_capacity(VOICES),
            world_buffer: vec![0.0; block_size * 2],
            resampled_buffer: vec![0.0; (block_size + 10) * 2],
            counters: Arc::new(RuntimeCounters::default()),
            backend_retirement_sender,
            pending_backend_retirements: Vec::with_capacity(VOICES),
            render_block_index: 0,
        };

        PetalSonicEngine::pump_render_state(&mut pump);
        while consumer.try_pop().is_some() {}
        while timing_receiver.try_recv().is_ok() {}

        let activity = callback_memory_activity(|| {
            PetalSonicEngine::pump_render_state(&mut pump);
        });

        assert_eq!(activity, 0, "steady render quantum allocated or freed");
        assert!(consumer.try_pop().is_some());

        let sustained_activity = callback_memory_activity(|| {
            for _ in 0..4_096 {
                while consumer.try_pop().is_some() {}
                while timing_receiver.try_recv().is_ok() {}
                PetalSonicEngine::pump_render_state(&mut pump);
            }
        });
        assert_eq!(
            sustained_activity, 0,
            "sustained near-capacity rendering allocated or freed"
        );

        let mut elapsed_us = [0u64; TIMING_SAMPLES];
        for elapsed in &mut elapsed_us {
            while consumer.try_pop().is_some() {}
            while timing_receiver.try_recv().is_ok() {}
            let start = Instant::now();
            PetalSonicEngine::pump_render_state(&mut pump);
            *elapsed = start.elapsed().as_micros() as u64;
        }
        elapsed_us.sort_unstable();
        let p99 = elapsed_us[elapsed_us.len() * 99 / 100];
        let device_period_us = block_size as u64 * 1_000_000 / sample_rate as u64;
        if !cfg!(debug_assertions) {
            assert_eq!(
                balanced_release_baseline_value("voices"),
                VOICES as u64,
                "release performance workload drifted without a new baseline"
            );
            assert_eq!(
                balanced_release_baseline_value("world_sample_rate"),
                sample_rate as u64,
                "release performance workload drifted without a new baseline"
            );
            assert_eq!(
                balanced_release_baseline_value("device_sample_rate"),
                device_sample_rate as u64,
                "release performance workload drifted without a new baseline"
            );
            assert_eq!(
                balanced_release_baseline_value("block_size"),
                block_size as u64,
                "release performance workload drifted without a new baseline"
            );
            assert!(
                p99 * 100 < device_period_us * 80,
                "Balanced full-quantum p99 {p99}us lacks 20% margin under {device_period_us}us period"
            );
            let baseline_p99 = balanced_release_baseline_value("p99_us");
            let max_regression_percent =
                balanced_release_baseline_value("max_p99_regression_percent");
            let regression_limit = baseline_p99
                .saturating_mul(100 + max_regression_percent)
                .div_ceil(100);
            assert!(
                p99 <= regression_limit,
                "Balanced full-quantum p99 regressed: current={p99}us baseline={baseline_p99}us limit={regression_limit}us ({max_regression_percent}% allowed)"
            );
        }
        eprintln!(
            "balanced near-capacity baseline ({VOICES} voices): p50={}us p95={}us p99={}us max={}us period={}us",
            elapsed_us[elapsed_us.len() / 2],
            elapsed_us[elapsed_us.len() * 95 / 100],
            p99,
            elapsed_us[elapsed_us.len() - 1],
            device_period_us,
        );
    }

    #[test]
    fn emitter_supports_overlapping_voices_and_detached_destroy_semantics() {
        let emitter = crate::domain::Emitter {
            world_id: 1,
            index: 4,
            generation: 2,
        };
        let clip = Arc::new(PetalSonicAudioData::new(
            vec![0.25; 32],
            48_000,
            1,
            Duration::from_secs_f64(32.0 / 48_000.0),
        ));
        let (sender, receiver) = crossbeam_channel::bounded(8);
        let (lifecycle_sender, lifecycle_receiver) = crossbeam_channel::bounded(8);
        let receivers = EngineCommandReceivers::new(receiver, lifecycle_receiver);
        for (voice, detached) in [(SourceId::from(10), false), (SourceId::from(11), true)] {
            sender
                .try_send(PlaybackCommand::Play {
                    voice_id: voice,
                    emitter,
                    source: clip.clone(),
                    config: SourceConfig::non_spatial(),
                    loop_mode: LoopMode::Infinite,
                    detached,
                    completion_tag: None,
                    bus_index: 0,
                    playback_rate: 1.0,
                    direct_path: crate::domain::DirectPath::default(),
                    environment_send: crate::domain::EnvironmentSend::default(),
                    play_command_id: None,
                    mono_scratch: vec![0.0; 32],
                })
                .unwrap();
        }

        let active = Arc::new(Mutex::new(HashMap::new()));
        let active_count = Arc::new(AtomicUsize::new(2));
        let acoustic_voice_input = AcousticVoiceInput::isolated(8);
        let mut buses = [BusParams::default()];
        PetalSonicEngine::process_playback_commands(
            &receivers,
            &active,
            &active_count,
            &acoustic_voice_input,
            &mut buses,
        );
        assert_eq!(active.lock().unwrap().len(), 2);

        lifecycle_sender
            .try_send(PlaybackCommand::DestroyEmitter(emitter))
            .unwrap();
        PetalSonicEngine::process_playback_commands(
            &receivers,
            &active,
            &active_count,
            &acoustic_voice_input,
            &mut buses,
        );

        let mut active = active.lock().unwrap();
        assert_eq!(active.len(), 2, "attached voice fades before retirement");
        assert!(active.values().any(|voice| voice.detached));
        let attached = active.values_mut().find(|voice| !voice.detached).unwrap();
        attached.advance_silently(240);
        assert!(attached.should_reclaim());
    }

    #[test]
    fn spatial_frame_updates_attached_voices_as_one_generation() {
        let emitter = crate::domain::Emitter {
            world_id: 1,
            index: 2,
            generation: 3,
        };
        let clip = Arc::new(PetalSonicAudioData::new(
            vec![0.5; 32],
            48_000,
            1,
            Duration::from_secs_f64(32.0 / 48_000.0),
        ));
        let old_pose = Pose::from_position(crate::math::Vec3::ZERO);
        let new_pose = Pose::from_position(crate::math::Vec3::new(4.0, 0.0, -2.0));
        let mut voices = HashMap::new();
        for (voice_id, detached) in [(SourceId::from(20), false), (SourceId::from(21), true)] {
            voices.insert(
                voice_id,
                PlaybackInstance::from_source(VoiceStart {
                    emitter,
                    audio_data: clip.clone(),
                    config: SourceConfig::spatial(old_pose),
                    loop_mode: LoopMode::Infinite,
                    bus_index: 0,
                    playback_rate: 1.0,
                    detached,
                    completion_tag: None,
                    direct_path: crate::domain::DirectPath::default(),
                    environment_send: crate::domain::EnvironmentSend::default(),
                    play_command_id: None,
                    mono_scratch: vec![0.0; 32],
                }),
            );
        }
        let frame = SpatialFrame::new(
            1,
            0.0,
            Pose::default(),
            vec![crate::domain::EmitterSpatialState::new(emitter, new_pose)],
        );

        PetalSonicEngine::apply_spatial_frame_to_voices(&frame, &mut voices);

        assert_eq!(voices[&SourceId::from(20)].config.pose(), Some(new_pose));
        assert_eq!(voices[&SourceId::from(21)].config.pose(), Some(old_pose));
    }
}
