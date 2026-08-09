use crate::audio_data::{ResamplerType, StreamingResampler};
use crate::config::{
    AmbisonicsBackend, DirectPathBackend, HrtfBackend, LatencyProfile, OutputDevicePolicy,
    PetalSonicWorldDesc, SpatialQuality,
};
use crate::domain::{PlaybackControl, SpatialFrame};
use crate::error::PetalSonicError;
use crate::error::Result;
use crate::events::{PetalSonicEvent, RenderTimingEvent};
use crate::math::Pose;
use crate::mixer;
use crate::playback::{PlaybackCommand, PlaybackInstance};
use crate::spatial::SpatialProcessor;
use crate::world::SourceId;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use std::cell::RefCell;
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

// Thread-local buffers to avoid allocations in audio callback
thread_local! {
    static WORLD_BUFFER: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
    static RESAMPLED_BUFFER: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}

const MASTER_HEADROOM_DB: f32 = -6.0;
const STARTUP_UNDERRUN_GRACE_CALLBACKS: usize = 8;
const LOGICAL_CHANNELS: u16 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpatialBackendPlan {
    hrtf: HrtfBackend,
    direct_path: DirectPathBackend,
    use_ambisonics: bool,
    ambisonics: AmbisonicsBackend,
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
}

struct PumpState {
    active_playback: Arc<Mutex<HashMap<SourceId, PlaybackInstance>>>,
    active_voice_count: Arc<AtomicUsize>,
    retirement_sender: Sender<SourceId>,
    latest_spatial_frame: Arc<Mutex<Option<Arc<SpatialFrame>>>>,
    current_spatial_frame: Option<Arc<SpatialFrame>>,
    pending_spatial_retirement: Option<Arc<SpatialFrame>>,
    spatial_retirement_sender: Sender<Arc<SpatialFrame>>,
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
    schedule: RenderSchedule,
}

/// Parameters for stream creation - groups related parameters to reduce argument count
struct StreamCreationParams {
    is_running: Arc<AtomicBool>,
    frames_processed: Arc<AtomicUsize>,
    world_sample_rate: u32,
    device_sample_rate: u32,
    channels: u16,
    active_playback: Arc<Mutex<HashMap<SourceId, PlaybackInstance>>>,
    command_receiver: Receiver<PlaybackCommand>,
    event_sender: Sender<PetalSonicEvent>,
    timing_sender: Sender<RenderTimingEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioOutputDeviceInfo {
    pub name: String,
    pub is_default: bool,
    pub aliases: Vec<String>,
}

struct AudioOutputDeviceCandidate {
    device: cpal::Device,
    info: AudioOutputDeviceInfo,
}

/// Audio engine that manages real-time audio processing and output
pub(crate) struct PetalSonicEngine {
    desc: PetalSonicWorldDesc,
    stream: Option<cpal::Stream>,
    is_running: Arc<AtomicBool>,
    frames_processed: Arc<AtomicUsize>,
    underrun_count: Arc<AtomicUsize>,
    stream_error: Arc<AtomicBool>,
    active_playback: Arc<std::sync::Mutex<HashMap<SourceId, PlaybackInstance>>>,
    active_voice_count: Arc<AtomicUsize>,
    retirement_sender: Sender<SourceId>,
    latest_spatial_frame: Arc<Mutex<Option<Arc<SpatialFrame>>>>,
    spatial_retirement_sender: Sender<Arc<SpatialFrame>>,
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
    event_receiver: Receiver<PetalSonicEvent>,
    /// Timing channel for performance profiling
    /// The sender is cloned to render thread, receiver stays here for polling
    timing_sender: Sender<RenderTimingEvent>,
    timing_receiver: Receiver<RenderTimingEvent>,
    master_headroom_db: f32,
    master_gain_linear: f32,
    schedule: RenderSchedule,
}

impl PetalSonicEngine {
    /// Create the internal engine owned by a [`PetalSonicWorld`](crate::PetalSonicWorld).
    pub(crate) fn new(
        desc: PetalSonicWorldDesc,
        listener_pose: Arc<Mutex<Pose>>,
        active_voice_count: Arc<AtomicUsize>,
        retirement_sender: Sender<SourceId>,
        latest_spatial_frame: Arc<Mutex<Option<Arc<SpatialFrame>>>>,
        spatial_retirement_sender: Sender<Arc<SpatialFrame>>,
    ) -> Result<Self> {
        let backend_plan = Self::resolve_spatial_backend_plan(&desc);
        let schedule = RenderSchedule::for_profile(desc.latency_profile);
        // Initialize spatial processor
        // Use distance_scaler from world configuration (converts world units to meters)
        let spatial_processor = match SpatialProcessor::new(
            desc.sample_rate,
            desc.block_size,
            desc.distance_scaler,
            desc.steam_hrtf_path.as_deref(),
            desc.native_hrtf_path.as_deref(),
            desc.hrtf_gain,
            backend_plan.hrtf,
            backend_plan.direct_path,
            backend_plan.use_ambisonics,
            backend_plan.ambisonics,
            desc.batched_any_hit_ray_tracer.clone(),
            desc.batched_closest_hit_ray_tracer.clone(),
        ) {
            Ok(processor) => Some(Arc::new(Mutex::new(processor))),
            Err(e) => {
                log::warn!("Failed to initialize spatial audio processor: {}", e);
                log::warn!("Spatial audio will be disabled");
                None
            }
        };

        // Event and timing delivery must remain bounded. Saturation is observable via
        // diagnostics in the public world rather than paid for with unbounded memory.
        let (event_sender, event_receiver) = crossbeam_channel::bounded(desc.event_queue_capacity);

        let (timing_sender, timing_receiver) =
            crossbeam_channel::bounded(desc.timing_queue_capacity);

        let master_headroom_db = MASTER_HEADROOM_DB;
        let master_gain_linear = crate::gain::db_to_linear(master_headroom_db);

        Ok(Self {
            device_sample_rate: desc.sample_rate, // Will be updated when stream starts
            desc,
            stream: None,
            is_running: Arc::new(AtomicBool::new(false)),
            frames_processed: Arc::new(AtomicUsize::new(0)),
            underrun_count: Arc::new(AtomicUsize::new(0)),
            stream_error: Arc::new(AtomicBool::new(false)),
            active_playback: Arc::new(std::sync::Mutex::new(HashMap::new())),
            active_voice_count,
            retirement_sender,
            latest_spatial_frame,
            spatial_retirement_sender,
            pump_state: None,
            render_thread: None,
            spatial_processor,
            listener_pose,
            event_sender,
            event_receiver,
            timing_sender,
            timing_receiver,
            master_headroom_db,
            master_gain_linear,
            schedule,
        })
    }

    fn resolve_spatial_backend_plan(desc: &PetalSonicWorldDesc) -> SpatialBackendPlan {
        let native_hrtf_available = desc.native_hrtf_path.is_some();
        match desc.spatial_quality {
            SpatialQuality::LowLatency => SpatialBackendPlan {
                hrtf: if native_hrtf_available {
                    HrtfBackend::Native
                } else {
                    HrtfBackend::SteamAudio
                },
                direct_path: DirectPathBackend::Native,
                use_ambisonics: false,
                ambisonics: AmbisonicsBackend::Native,
            },
            SpatialQuality::Balanced => SpatialBackendPlan {
                hrtf: if native_hrtf_available {
                    HrtfBackend::Native
                } else {
                    HrtfBackend::SteamAudio
                },
                direct_path: DirectPathBackend::Native,
                use_ambisonics: true,
                ambisonics: AmbisonicsBackend::Native,
            },
            SpatialQuality::HighQuality => SpatialBackendPlan {
                hrtf: HrtfBackend::SteamAudio,
                direct_path: DirectPathBackend::SteamAudio,
                use_ambisonics: true,
                ambisonics: AmbisonicsBackend::SteamAudio,
            },
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Relaxed)
    }

    /// Start the audio engine with automatic playback management
    pub(crate) fn start(&mut self, command_receiver: Receiver<PlaybackCommand>) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }

        let requested_device = match &self.desc.output_device {
            OutputDevicePolicy::FollowSystemDefault => None,
            OutputDevicePolicy::PinnedNameContains(name) => Some(name.as_str()),
        };
        let (device, device_config) = Self::init_audio_device(requested_device)?;
        let device_sample_rate = device_config.sample_rate().0;

        self.device_sample_rate = device_sample_rate;

        log::info!(
            "PetalSonic master headroom: {} dB (linear gain {:.3})",
            self.master_headroom_db,
            self.master_gain_linear
        );

        let buffer_size = Self::select_buffer_size(&device_config);
        let config = Self::create_stream_config(LOGICAL_CHANNELS, device_sample_rate, buffer_size);

        self.is_running.store(true, Ordering::Release);
        self.stream_error.store(false, Ordering::Release);

        let stream_result = self.build_stream(
            &device,
            &device_config,
            &config,
            device_sample_rate,
            command_receiver.clone(),
        );
        let (stream, pump_state) = match stream_result {
            Ok(result) => result,
            Err(err) if !matches!(config.buffer_size, cpal::BufferSize::Default) => {
                log::warn!(
                    "PetalSonic failed to start stream with requested output buffer size ({}); retrying with the device default buffer size",
                    err
                );
                let default_config = Self::create_stream_config(
                    LOGICAL_CHANNELS,
                    device_sample_rate,
                    cpal::BufferSize::Default,
                );
                match self.build_stream(
                    &device,
                    &device_config,
                    &default_config,
                    device_sample_rate,
                    command_receiver,
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

    pub(crate) fn available_output_devices() -> Result<Vec<AudioOutputDeviceInfo>> {
        let host = cpal::default_host();
        Ok(Self::output_device_candidates(&host)?
            .into_iter()
            .map(|candidate| candidate.info)
            .collect())
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
        command_receiver: Receiver<PlaybackCommand>,
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
                    command_receiver: command_receiver.clone(),
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
                    command_receiver: command_receiver.clone(),
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
                    command_receiver,
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

        Ok(())
    }

    pub(crate) fn frames_processed(&self) -> usize {
        self.frames_processed.load(Ordering::Relaxed)
    }

    pub(crate) fn underrun_count(&self) -> usize {
        self.underrun_count.load(Ordering::Relaxed)
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
    pub(crate) fn poll_events(&self) -> Vec<PetalSonicEvent> {
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
    pub(crate) fn poll_timing_events(&self) -> Vec<RenderTimingEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.timing_receiver.try_recv() {
            events.push(event);
        }
        events
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
            resampler: resampler.clone(),
            ring_buffer_producer: producer,
            channels: params.channels,
            block_size,
            spatial_processor: self.spatial_processor.clone(),
            command_receiver: params.command_receiver,
            listener_pose: self.listener_pose.clone(),
            event_sender: params.event_sender,
            timing_sender: params.timing_sender,
            master_gain_linear: self.master_gain_linear,
            schedule: self.schedule,
        }));

        // Create context for audio callback (simplified - just consumes from ring buffer)
        let mut context = AudioCallbackContext {
            is_running: params.is_running,
            frames_processed: params.frames_processed,
            underrun_count: self.underrun_count.clone(),
            ring_buffer_consumer: consumer,
            channels: params.channels,
            startup_underrun_callbacks_remaining: STARTUP_UNDERRUN_GRACE_CALLBACKS,
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

        Self::consume_latest_spatial_frame(ctx);

        // Update listener pose in spatial processor if available.
        if let Some(ref spatial_processor) = ctx.spatial_processor
            && let Ok(mut processor) = spatial_processor.try_lock()
            && let Ok(listener_pose) = ctx.listener_pose.try_lock()
            && let Err(e) = processor.set_listener_pose(*listener_pose)
        {
            log::error!("Failed to update listener pose: {}", e);
        }

        Self::process_playback_commands(
            &ctx.command_receiver,
            &ctx.active_playback,
            &ctx.active_voice_count,
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

        let (completed_playbacks, _looped_sources, timing) = Self::generate_samples(
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

        let _ = ctx.timing_sender.try_send(timing);

        ctx.active_voice_count
            .fetch_sub(completed_playbacks.len(), Ordering::AcqRel);
        for completed in completed_playbacks {
            if let Some(tag) = completed.completion_tag {
                let _ = ctx.retirement_sender.try_send(completed.voice_id);
                let _ = ctx
                    .event_sender
                    .try_send(PetalSonicEvent::PlaybackCompleted {
                        emitter: completed.emitter,
                        control: PlaybackControl {
                            voice_id: completed.voice_id,
                        },
                        tag,
                    });
            }
        }
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

        if let Some(previous) = ctx.current_spatial_frame.replace(next) {
            if let Err(error) = ctx.spatial_retirement_sender.try_send(previous) {
                ctx.pending_spatial_retirement = Some(error.into_inner());
            }
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
                if ctx.startup_underrun_callbacks_remaining > 0 {
                    ctx.startup_underrun_callbacks_remaining -= 1;
                } else {
                    ctx.underrun_count.fetch_add(1, Ordering::Relaxed);
                }
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
        active_voice_count: &Arc<AtomicUsize>,
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
                PlaybackCommand::Play {
                    voice_id,
                    emitter,
                    source,
                    config,
                    loop_mode,
                    detached,
                    completion_tag,
                } => {
                    let mut instance = PlaybackInstance::from_source(
                        voice_id,
                        emitter,
                        source,
                        config,
                        loop_mode,
                        detached,
                        completion_tag,
                    );
                    instance.play_from_beginning();
                    if active_playback.insert(voice_id, instance).is_some() {
                        active_voice_count.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                PlaybackCommand::PauseVoice(voice_id) => {
                    if let Some(instance) = active_playback.get_mut(&voice_id) {
                        instance.pause();
                    }
                }
                PlaybackCommand::StopVoice(voice_id) => {
                    if active_playback.remove(&voice_id).is_some() {
                        active_voice_count.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                PlaybackCommand::SeekVoice(voice_id, progress) => {
                    if let Some(instance) = active_playback.get_mut(&voice_id) {
                        instance.seek(progress);
                    }
                }
                PlaybackCommand::PauseEmitter(emitter) => {
                    for instance in active_playback.values_mut() {
                        if instance.emitter == emitter {
                            instance.pause();
                        }
                    }
                }
                PlaybackCommand::StopEmitter(emitter) => {
                    let before = active_playback.len();
                    active_playback.retain(|_, instance| instance.emitter != emitter);
                    active_voice_count.fetch_sub(before - active_playback.len(), Ordering::AcqRel);
                }
                PlaybackCommand::SeekEmitter(emitter, progress) => {
                    for instance in active_playback.values_mut() {
                        if instance.emitter == emitter {
                            instance.seek(progress);
                        }
                    }
                }
                PlaybackCommand::DestroyEmitter(emitter) => {
                    let before = active_playback.len();
                    active_playback
                        .retain(|_, instance| instance.emitter != emitter || instance.detached);
                    active_voice_count.fetch_sub(before - active_playback.len(), Ordering::AcqRel);
                }
                PlaybackCommand::UpdateEmitter(emitter, config) => {
                    for instance in active_playback.values_mut() {
                        if instance.emitter == emitter && !instance.detached {
                            instance.config = config.clone();
                        }
                    }
                }
                PlaybackCommand::UpdateDirectPathOverride(emitter, direct_path_override) => {
                    for instance in active_playback.values_mut() {
                        if instance.emitter == emitter && !instance.detached {
                            instance.direct_path_override = direct_path_override;
                        }
                    }
                }
                PlaybackCommand::StopAll => {
                    active_voice_count.fetch_sub(active_playback.len(), Ordering::AcqRel);
                    active_playback.clear();
                }
            }
        }
    }

    /// Generate resampled samples and push to ring buffer
    /// Returns completed voices, loop notifications, and timing data.
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
    ) -> (
        Vec<mixer::CompletedPlayback>,
        Vec<SourceId>,
        RenderTimingEvent,
    ) {
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
        let mut total_native_hrtf_direction_lookup_time_us = 0u64;
        let mut total_native_hrtf_convolution_time_us = 0u64;
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
                    spatial_source_count: 0,
                    spatial_simulation_time_us: 0,
                    direct_processing_time_us: 0,
                    ambisonics_encoding_time_us: 0,
                    ambisonics_decoding_time_us: 0,
                    hrtf_rendering_time_us: 0,
                    native_hrtf_direction_lookup_time_us: 0,
                    native_hrtf_convolution_time_us: 0,
                    resampling_time_us: 0,
                    total_time_us: 0,
                },
            );
        };

        // Track all completed and looped sources across all mixing iterations
        let mut all_completed_playbacks = Vec::new();
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
                all_completed_playbacks.extend(mix_result.completed_playbacks);
                all_looped_sources.extend(mix_result.looped_sources);

                // Capture coarse mixing time plus the detailed stage breakdown reported by the mixer
                total_mixing_time_us += mixing_elapsed.as_micros() as u64;
                total_direct_mixing_time_us += mix_profiling.direct_mix_time_us;
                total_spatial_time_us += mix_profiling.spatial_mix_time_us;
                if let Some(spatial_metrics) = mix_profiling.spatial_metrics {
                    total_spatial_source_count += spatial_metrics.spatial_source_count;
                    total_spatial_simulation_time_us += spatial_metrics.physics_simulation_time_us;
                    total_direct_processing_time_us += spatial_metrics.direct_processing_time_us;
                    total_ambisonics_encoding_time_us +=
                        spatial_metrics.ambisonics_encoding_time_us;
                    total_ambisonics_decoding_time_us +=
                        spatial_metrics.ambisonics_decoding_time_us;
                    total_hrtf_rendering_time_us += spatial_metrics.hrtf_rendering_time_us;
                    total_native_hrtf_direction_lookup_time_us +=
                        spatial_metrics.native_hrtf_direction_lookup_time_us;
                    total_native_hrtf_convolution_time_us +=
                        spatial_metrics.native_hrtf_convolution_time_us;
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
            all_completed_playbacks,
            all_looped_sources,
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
                native_hrtf_direction_lookup_time_us: total_native_hrtf_direction_lookup_time_us,
                native_hrtf_convolution_time_us: total_native_hrtf_convolution_time_us,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_data::PetalSonicAudioData;
    use crate::config::SourceConfig;
    use crate::playback::LoopMode;

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
        let mut desc = PetalSonicWorldDesc::default();

        desc.spatial_quality = SpatialQuality::LowLatency;
        let low_latency = PetalSonicEngine::resolve_spatial_backend_plan(&desc);
        assert_eq!(low_latency.hrtf, HrtfBackend::SteamAudio);
        assert_eq!(low_latency.direct_path, DirectPathBackend::Native);
        assert!(!low_latency.use_ambisonics);

        desc.native_hrtf_path = Some("headset.petalhrtf".into());
        desc.spatial_quality = SpatialQuality::Balanced;
        let balanced = PetalSonicEngine::resolve_spatial_backend_plan(&desc);
        assert_eq!(balanced.hrtf, HrtfBackend::Native);
        assert_eq!(balanced.direct_path, DirectPathBackend::Native);
        assert!(balanced.use_ambisonics);

        desc.spatial_quality = SpatialQuality::HighQuality;
        let high_quality = PetalSonicEngine::resolve_spatial_backend_plan(&desc);
        assert_eq!(high_quality.hrtf, HrtfBackend::SteamAudio);
        assert_eq!(high_quality.direct_path, DirectPathBackend::SteamAudio);
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
            })
            .unwrap();

        let ring_buffer = HeapRb::<StereoFrame>::new(block_size * 8);
        let (producer, mut consumer) = ring_buffer.split();
        let (event_sender, _event_receiver) = crossbeam_channel::bounded(8);
        let (timing_sender, _timing_receiver) = crossbeam_channel::bounded(8);
        let pump_state = Arc::new(Mutex::new(PumpState {
            active_playback: Arc::new(Mutex::new(HashMap::new())),
            active_voice_count: Arc::new(AtomicUsize::new(1)),
            retirement_sender: crossbeam_channel::bounded(8).0,
            latest_spatial_frame: Arc::new(Mutex::new(None)),
            current_spatial_frame: None,
            pending_spatial_retirement: None,
            spatial_retirement_sender: crossbeam_channel::bounded(1).0,
            resampler: PetalSonicEngine::create_resampler(sample_rate, sample_rate, 2, block_size)
                .unwrap(),
            ring_buffer_producer: producer,
            channels: 2,
            block_size,
            spatial_processor: None,
            command_receiver,
            listener_pose: Arc::new(Mutex::new(Pose::default())),
            event_sender,
            timing_sender,
            master_gain_linear: 1.0,
            schedule: RenderSchedule::for_profile(LatencyProfile::Balanced),
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
    fn emitter_supports_overlapping_voices_and_detached_destroy_semantics() {
        let emitter = crate::domain::Emitter {
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
                })
                .unwrap();
        }

        let active = Arc::new(Mutex::new(HashMap::new()));
        let active_count = Arc::new(AtomicUsize::new(2));
        PetalSonicEngine::process_playback_commands(&receiver, &active, &active_count);
        assert_eq!(active.lock().unwrap().len(), 2);

        sender
            .try_send(PlaybackCommand::DestroyEmitter(emitter))
            .unwrap();
        PetalSonicEngine::process_playback_commands(&receiver, &active, &active_count);

        let active = active.lock().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active.values().next().unwrap().detached);
        assert_eq!(active_count.load(Ordering::Acquire), 1);
    }

    #[test]
    fn spatial_frame_updates_attached_voices_as_one_generation() {
        let emitter = crate::domain::Emitter {
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
                PlaybackInstance::from_source(
                    voice_id,
                    emitter,
                    clip.clone(),
                    SourceConfig::spatial(old_pose),
                    LoopMode::Infinite,
                    detached,
                    None,
                ),
            );
        }
        let frame = SpatialFrame::new(
            Pose::default(),
            vec![crate::domain::EmitterSpatialState::new(emitter, new_pose)],
        );

        PetalSonicEngine::apply_spatial_frame_to_voices(&frame, &mut voices);

        assert_eq!(voices[&SourceId::from(20)].config.pose(), Some(new_pose));
        assert_eq!(voices[&SourceId::from(21)].config.pose(), Some(old_pose));
    }
}
