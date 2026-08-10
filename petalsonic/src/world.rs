use crate::acoustics::{AcousticSceneSlot, AcousticSceneSnapshot};
use crate::domain::{
    Bus, BusParams, Emitter, EmitterDesc, PlayOptions, PlaybackControl, PlaybackTag, ResidentClip,
    SpatialFrame,
};
use crate::engine::{
    EngineCommandReceivers, EngineObservability, EngineStartup, OutputRecoveryReason,
    PetalSonicEngine,
};
use crate::error::{PetalSonicError, Result};
use crate::events::{
    PetalSonicEvent, RenderTimingEvent, RuntimeCounters, RuntimeDiagnostics, RuntimeState,
    RuntimeStatus,
};
use crate::math::Pose;
use crate::playback::PlaybackCommand;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MIN_PLAYBACK_RATE: f32 = 0.01;
const MAX_PLAYBACK_RATE: f32 = 4.0;
const OUTPUT_RETRY_INTERVAL: Duration = Duration::from_millis(500);
static NEXT_WORLD_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputPreparation {
    Ready,
    Unavailable,
    RequiresStop,
}

trait OutputRuntimeDriver {
    fn drain_retired_resources(&mut self);
    fn output_recovery_reason(&self) -> Option<OutputRecoveryReason>;
    fn prepare_selected_output(&mut self) -> OutputPreparation;
    fn stop_output(&mut self) -> Result<()>;
    fn advance_without_output(
        &mut self,
        commands: &EngineCommandReceivers,
        buses: &mut [BusParams],
        elapsed: Duration,
    );
    fn start_output(
        &mut self,
        commands: EngineCommandReceivers,
        buses: Vec<BusParams>,
    ) -> Result<()>;
    fn emit_runtime_state(&self, state: RuntimeState);
}

impl OutputRuntimeDriver for PetalSonicEngine {
    fn drain_retired_resources(&mut self) {
        PetalSonicEngine::drain_retired_backend_resources(self);
    }

    fn output_recovery_reason(&self) -> Option<OutputRecoveryReason> {
        PetalSonicEngine::output_recovery_reason(self)
    }

    fn prepare_selected_output(&mut self) -> OutputPreparation {
        PetalSonicEngine::prepare_selected_output(self)
    }

    fn stop_output(&mut self) -> Result<()> {
        PetalSonicEngine::stop(self)
    }

    fn advance_without_output(
        &mut self,
        commands: &EngineCommandReceivers,
        buses: &mut [BusParams],
        elapsed: Duration,
    ) {
        PetalSonicEngine::advance_without_output(self, commands, buses, elapsed);
    }

    fn start_output(
        &mut self,
        commands: EngineCommandReceivers,
        buses: Vec<BusParams>,
    ) -> Result<()> {
        PetalSonicEngine::start(self, commands, buses)
    }

    fn emit_runtime_state(&self, state: RuntimeState) {
        PetalSonicEngine::emit_runtime_state(self, state);
    }
}

struct SupervisorSchedule {
    next_retry: Instant,
    next_health_probe: Instant,
    last_advance: Instant,
}

impl SupervisorSchedule {
    fn new(now: Instant) -> Self {
        Self {
            next_retry: now,
            next_health_probe: now,
            last_advance: now,
        }
    }
}

/// Internal identity for one playback voice.
///
/// IDs are never reused within a world, so a stale control cannot alias a later
/// voice even after the earlier slot has been reclaimed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(u64);

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VoiceId({})", self.0)
    }
}

impl From<u64> for SourceId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone)]
struct EmitterState {
    clip: ResidentClip,
    desc: EmitterDesc,
}

#[derive(Clone, Copy)]
struct ControlledVoiceState {
    emitter: Emitter,
    detached: bool,
}

struct EmitterSlot {
    generation: u32,
    state: Option<EmitterState>,
}

struct EmitterRegistry {
    world_id: u64,
    slots: Vec<EmitterSlot>,
    free: Vec<u32>,
    len: usize,
    limit: usize,
}

impl EmitterRegistry {
    fn new(limit: usize, world_id: u64) -> Self {
        Self {
            world_id,
            slots: Vec::with_capacity(limit),
            free: Vec::with_capacity(limit),
            len: 0,
            limit,
        }
    }

    fn insert(&mut self, state: EmitterState) -> Result<Emitter> {
        if self.len >= self.limit {
            return Err(PetalSonicError::CapacityExceeded {
                resource: "emitter",
                limit: self.limit,
            });
        }

        self.len += 1;
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.state = Some(state);
            return Ok(Emitter {
                world_id: self.world_id,
                index,
                generation: slot.generation,
            });
        }

        let index = self.slots.len() as u32;
        self.slots.push(EmitterSlot {
            generation: 1,
            state: Some(state),
        });
        Ok(Emitter {
            world_id: self.world_id,
            index,
            generation: 1,
        })
    }

    fn get(&self, emitter: Emitter) -> Result<&EmitterState> {
        if emitter.world_id != self.world_id {
            return Err(PetalSonicError::StaleEmitter);
        }
        let slot = self
            .slots
            .get(emitter.index as usize)
            .ok_or(PetalSonicError::StaleEmitter)?;
        if slot.generation != emitter.generation {
            return Err(PetalSonicError::StaleEmitter);
        }
        slot.state.as_ref().ok_or(PetalSonicError::StaleEmitter)
    }

    fn get_mut(&mut self, emitter: Emitter) -> Result<&mut EmitterState> {
        if emitter.world_id != self.world_id {
            return Err(PetalSonicError::StaleEmitter);
        }
        let slot = self
            .slots
            .get_mut(emitter.index as usize)
            .ok_or(PetalSonicError::StaleEmitter)?;
        if slot.generation != emitter.generation {
            return Err(PetalSonicError::StaleEmitter);
        }
        slot.state.as_mut().ok_or(PetalSonicError::StaleEmitter)
    }

    fn remove(&mut self, emitter: Emitter) -> Result<EmitterState> {
        if emitter.world_id != self.world_id {
            return Err(PetalSonicError::StaleEmitter);
        }
        let slot = self
            .slots
            .get_mut(emitter.index as usize)
            .ok_or(PetalSonicError::StaleEmitter)?;
        if slot.generation != emitter.generation {
            return Err(PetalSonicError::StaleEmitter);
        }
        let state = slot.state.take().ok_or(PetalSonicError::StaleEmitter)?;
        slot.generation = slot.generation.wrapping_add(1).max(1);
        self.free.push(emitter.index);
        self.len -= 1;
        Ok(state)
    }

    fn apply_spatial_frame(&mut self, frame: &SpatialFrame) -> Result<()> {
        let expected = self
            .slots
            .iter()
            .filter_map(|slot| slot.state.as_ref())
            .filter(|state| state.desc.is_spatial())
            .count();
        if frame.emitters().len() != expected {
            return Err(PetalSonicError::InvalidConfiguration {
                field: "spatial_frame",
                reason: format!(
                    "expected {expected} spatial emitters, received {}",
                    frame.emitters().len()
                ),
            });
        }

        let mut seen = HashSet::with_capacity(frame.emitters().len());
        for spatial in frame.emitters() {
            if !seen.insert(spatial.emitter) {
                return Err(PetalSonicError::InvalidConfiguration {
                    field: "spatial_frame",
                    reason: format!("contains duplicate {}", spatial.emitter),
                });
            }
            let state = self.get(spatial.emitter)?;
            if !state.desc.is_spatial() {
                return Err(PetalSonicError::InvalidConfiguration {
                    field: "spatial_frame",
                    reason: format!("{} is not spatial", spatial.emitter),
                });
            }
        }

        for spatial in frame.emitters() {
            self.get_mut(spatial.emitter)?.desc.set_pose(spatial.pose);
        }
        Ok(())
    }
}

/// Main facade for audio resources, emitters, playback, events, and runtime state.
///
/// Creating a world starts its private render runtime. Callers submit bounded,
/// non-blocking intent and never drive audio progress themselves.
pub struct PetalSonicWorld {
    world_id: u64,
    desc: crate::config::PetalSonicWorldDesc,
    bus_params: Arc<Mutex<Vec<BusParams>>>,
    emitters: Mutex<EmitterRegistry>,
    next_voice_id: AtomicU64,
    active_voice_count: Arc<AtomicUsize>,
    controlled_voices: Mutex<HashMap<SourceId, ControlledVoiceState>>,
    retirement_receiver: Receiver<SourceId>,
    latest_spatial_frame: Arc<Mutex<Option<Arc<SpatialFrame>>>>,
    spatial_retirement_receiver: Receiver<Arc<SpatialFrame>>,
    latest_acoustic_scene: Arc<Mutex<Option<Arc<AcousticSceneSnapshot>>>>,
    acoustic_retirement_receiver: Receiver<Arc<AcousticSceneSnapshot>>,
    acoustic_scene_version: AtomicU64,
    command_sender: Sender<PlaybackCommand>,
    lifecycle_sender: Sender<PlaybackCommand>,
    counters: Arc<RuntimeCounters>,
    frames_processed: Arc<AtomicUsize>,
    underrun_count: Arc<AtomicUsize>,
    active_output_device: Arc<Mutex<Option<String>>>,
    event_receiver: Receiver<PetalSonicEvent>,
    timing_receiver: Receiver<RenderTimingEvent>,
    runtime_state: Arc<AtomicU8>,
    recovery_attempts: Arc<AtomicU64>,
    supervisor_stop: Arc<AtomicBool>,
    close_lock: Mutex<()>,
    supervisor_thread: Mutex<Option<JoinHandle<()>>>,
}

impl PetalSonicWorld {
    pub fn new(config: crate::config::PetalSonicWorldDesc) -> Result<Self> {
        Self::validate_config(&config)?;
        let world_id = NEXT_WORLD_ID.fetch_add(1, Ordering::Relaxed);

        let (command_sender, command_receiver) =
            crossbeam_channel::bounded(config.control_queue_capacity);
        let (lifecycle_sender, lifecycle_receiver) =
            crossbeam_channel::bounded(config.lifecycle_queue_capacity);
        let listener_pose = Arc::new(Mutex::new(Pose::default()));
        let active_voice_count = Arc::new(AtomicUsize::new(0));
        let (retirement_sender, retirement_receiver) =
            crossbeam_channel::bounded(config.max_voices);
        let latest_spatial_frame = Arc::new(Mutex::new(None));
        let (spatial_retirement_sender, spatial_retirement_receiver) =
            crossbeam_channel::bounded(1);
        let initial_acoustic_scene = config.acoustic_scene.clone().map(Arc::new);
        let acoustic_scene_version = initial_acoustic_scene
            .as_ref()
            .map(|scene| scene.version())
            .unwrap_or(0);
        let acoustic_scene_slot = Arc::new(AcousticSceneSlot::new(initial_acoustic_scene));
        let latest_acoustic_scene = Arc::new(Mutex::new(None));
        let (acoustic_retirement_sender, acoustic_retirement_receiver) =
            crossbeam_channel::bounded(2);
        let bus_params = Arc::new(Mutex::new(
            std::iter::once(BusParams::default())
                .chain(config.buses.iter().map(|bus| bus.params()))
                .collect::<Vec<_>>(),
        ));
        let (ports, observability) = PetalSonicEngine::create_runtime_ports(&config);
        let startup = EngineStartup {
            desc: config.clone(),
            listener_pose,
            active_voice_count: active_voice_count.clone(),
            retirement_sender,
            latest_spatial_frame: latest_spatial_frame.clone(),
            spatial_retirement_sender,
            latest_acoustic_scene: latest_acoustic_scene.clone(),
            acoustic_scene_slot,
            acoustic_retirement_sender,
            ports,
        };
        let EngineObservability {
            frames_processed,
            underrun_count,
            active_device_name,
            event_receiver,
            timing_receiver,
            counters,
        } = observability;
        let runtime_state = Arc::new(AtomicU8::new(RuntimeState::Recovering as u8));
        let recovery_attempts = Arc::new(AtomicU64::new(0));
        let supervisor_stop = Arc::new(AtomicBool::new(false));
        let supervisor_thread = Self::spawn_output_supervisor(
            startup,
            EngineCommandReceivers::new(command_receiver, lifecycle_receiver),
            bus_params.clone(),
            runtime_state.clone(),
            recovery_attempts.clone(),
            supervisor_stop.clone(),
        )?;

        Ok(Self {
            world_id,
            emitters: Mutex::new(EmitterRegistry::new(config.max_emitters, world_id)),
            bus_params,
            controlled_voices: Mutex::new(HashMap::with_capacity(config.max_voices)),
            desc: config,
            next_voice_id: AtomicU64::new(0),
            active_voice_count,
            retirement_receiver,
            latest_spatial_frame,
            spatial_retirement_receiver,
            latest_acoustic_scene,
            acoustic_retirement_receiver,
            acoustic_scene_version: AtomicU64::new(acoustic_scene_version),
            command_sender,
            lifecycle_sender,
            counters,
            frames_processed,
            underrun_count,
            active_output_device: active_device_name,
            event_receiver,
            timing_receiver,
            runtime_state,
            recovery_attempts,
            supervisor_stop,
            close_lock: Mutex::new(()),
            supervisor_thread: Mutex::new(Some(supervisor_thread)),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn supervisor_tick<D: OutputRuntimeDriver>(
        driver: &mut D,
        commands: &EngineCommandReceivers,
        bus_params: &Mutex<Vec<BusParams>>,
        runtime_state: &AtomicU8,
        recovery_attempts: &AtomicU64,
        recovery_buses: &mut Vec<BusParams>,
        schedule: &mut SupervisorSchedule,
        now: Instant,
    ) {
        driver.drain_retired_resources();
        let state = Self::load_runtime_state(runtime_state);
        let recovery_reason = (state == RuntimeState::Running && now >= schedule.next_health_probe)
            .then(|| driver.output_recovery_reason())
            .flatten();
        let selection_preparation = matches!(
            recovery_reason,
            Some(OutputRecoveryReason::SelectionChanged)
        )
        .then(|| driver.prepare_selected_output());
        let should_recover = state == RuntimeState::Recovering
            || matches!(recovery_reason, Some(OutputRecoveryReason::StreamFailure))
            || matches!(
                selection_preparation,
                Some(OutputPreparation::Ready | OutputPreparation::RequiresStop)
            );

        if state == RuntimeState::Running && now >= schedule.next_health_probe {
            schedule.next_health_probe = now + OUTPUT_RETRY_INTERVAL;
        }

        if !should_recover {
            return;
        }

        if Self::load_runtime_state(runtime_state) == RuntimeState::Running {
            let _ = driver.stop_output();
            driver.emit_runtime_state(RuntimeState::Recovering);
            runtime_state.store(RuntimeState::Recovering as u8, Ordering::Release);
            *recovery_buses = bus_params
                .lock()
                .map(|buses| buses.clone())
                .unwrap_or_else(|_| vec![BusParams::default()]);
            schedule.last_advance = now;
            schedule.next_retry = now;
        }

        if Self::load_runtime_state(runtime_state) != RuntimeState::Recovering {
            return;
        }

        let elapsed = now.saturating_duration_since(schedule.last_advance);
        schedule.last_advance = now;
        driver.advance_without_output(commands, recovery_buses, elapsed);

        if now < schedule.next_retry {
            return;
        }

        recovery_attempts.fetch_add(1, Ordering::Relaxed);
        let next_buses = bus_params
            .lock()
            .map(|buses| buses.clone())
            .unwrap_or_else(|_| recovery_buses.clone());
        match driver.start_output(commands.clone(), next_buses) {
            Ok(()) => {
                runtime_state.store(RuntimeState::Running as u8, Ordering::Release);
                schedule.next_health_probe = now + OUTPUT_RETRY_INTERVAL;
                driver.emit_runtime_state(RuntimeState::Running);
            }
            Err(
                PetalSonicError::AudioFormat(_)
                | PetalSonicError::PermanentDeviceFailure(_)
                | PetalSonicError::BackendUnavailable { .. },
            ) => {
                runtime_state.store(RuntimeState::Failed as u8, Ordering::Release);
                driver.emit_runtime_state(RuntimeState::Failed);
            }
            Err(_) => {
                schedule.next_retry = now + OUTPUT_RETRY_INTERVAL;
            }
        }
    }

    fn spawn_output_supervisor(
        startup: EngineStartup,
        command_receivers: EngineCommandReceivers,
        bus_params: Arc<Mutex<Vec<BusParams>>>,
        runtime_state: Arc<AtomicU8>,
        recovery_attempts: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
    ) -> Result<JoinHandle<()>> {
        let (startup_sender, startup_receiver) = crossbeam_channel::bounded(1);
        let handle = std::thread::Builder::new()
            .name("petalsonic-output".into())
            .spawn(move || {
                let mut engine = match PetalSonicEngine::new(startup) {
                    Ok(engine) => engine,
                    Err(error) => {
                        runtime_state.store(RuntimeState::Failed as u8, Ordering::Release);
                        let _ = startup_sender.send(Err(error));
                        return;
                    }
                };
                if startup_sender.send(Ok(())).is_err() {
                    return;
                }
                let poll_interval = Duration::from_millis(20);
                let mut schedule = SupervisorSchedule::new(Instant::now());
                let mut recovery_buses = bus_params
                    .lock()
                    .map(|buses| buses.clone())
                    .unwrap_or_else(|_| vec![BusParams::default()]);
                engine.emit_runtime_state(RuntimeState::Recovering);

                while !stop.load(Ordering::Acquire) {
                    Self::supervisor_tick(
                        &mut engine,
                        &command_receivers,
                        &bus_params,
                        &runtime_state,
                        &recovery_attempts,
                        &mut recovery_buses,
                        &mut schedule,
                        Instant::now(),
                    );

                    std::thread::park_timeout(poll_interval);
                }
                let _ = engine.stop_output();
            })
            .map_err(|error| {
                PetalSonicError::Engine(format!("Failed to start output supervisor: {error}"))
            })?;

        match startup_receiver.recv() {
            Ok(Ok(())) => Ok(handle),
            Ok(Err(error)) => {
                let _ = handle.join();
                Err(error)
            }
            Err(_) => {
                let _ = handle.join();
                Err(PetalSonicError::Engine(
                    "Output supervisor exited during initialization".into(),
                ))
            }
        }
    }

    fn load_runtime_state(state: &AtomicU8) -> RuntimeState {
        match state.load(Ordering::Acquire) {
            value if value == RuntimeState::Running as u8 => RuntimeState::Running,
            value if value == RuntimeState::Recovering as u8 => RuntimeState::Recovering,
            value if value == RuntimeState::Failed as u8 => RuntimeState::Failed,
            value if value == RuntimeState::Closing as u8 => RuntimeState::Closing,
            _ => RuntimeState::Closed,
        }
    }

    fn validate_config(config: &crate::config::PetalSonicWorldDesc) -> Result<()> {
        for (field, value) in [
            ("sample_rate", config.sample_rate as usize),
            ("block_size", config.block_size),
            ("max_emitters", config.max_emitters),
            ("max_voices", config.max_voices),
            ("control_queue_capacity", config.control_queue_capacity),
            ("lifecycle_queue_capacity", config.lifecycle_queue_capacity),
            ("event_queue_capacity", config.event_queue_capacity),
            ("timing_queue_capacity", config.timing_queue_capacity),
        ] {
            if value == 0 {
                return Err(PetalSonicError::InvalidConfiguration {
                    field,
                    reason: "must be greater than zero".into(),
                });
            }
        }
        if !config.hrtf_gain.is_finite() {
            return Err(PetalSonicError::InvalidConfiguration {
                field: "hrtf_gain",
                reason: "must be finite".into(),
            });
        }
        if !config.distance_scaler.is_finite() || config.distance_scaler <= 0.0 {
            return Err(PetalSonicError::InvalidConfiguration {
                field: "distance_scaler",
                reason: "must be finite and greater than zero".into(),
            });
        }
        if config.buses.len() > config.max_buses {
            return Err(PetalSonicError::CapacityExceeded {
                resource: "bus",
                limit: config.max_buses,
            });
        }
        let mut names = HashSet::with_capacity(config.buses.len());
        for bus in &config.buses {
            let name = bus.name().trim();
            if name.is_empty() {
                return Err(PetalSonicError::InvalidConfiguration {
                    field: "buses",
                    reason: "bus names must not be empty".into(),
                });
            }
            if name.eq_ignore_ascii_case("Master") {
                return Err(PetalSonicError::InvalidConfiguration {
                    field: "buses",
                    reason: "Master is implicit and must not be declared".into(),
                });
            }
            if !names.insert(name.to_ascii_lowercase()) {
                return Err(PetalSonicError::InvalidConfiguration {
                    field: "buses",
                    reason: format!("duplicate bus name {name:?}"),
                });
            }
            Self::validate_bus_params(bus.params())?;
        }
        Ok(())
    }

    pub fn config(&self) -> &crate::config::PetalSonicWorldDesc {
        &self.desc
    }

    pub fn create_emitter(&self, clip: ResidentClip, desc: EmitterDesc) -> Result<Emitter> {
        self.ensure_open()?;
        self.validate_optional_bus(desc.bus())?;
        let clip = self.prepare_clip(clip)?;
        self.emitters
            .lock()
            .map_err(|_| PetalSonicError::Engine("Emitter registry is poisoned".into()))?
            .insert(EmitterState { clip, desc })
    }

    fn prepare_clip(&self, clip: ResidentClip) -> Result<ResidentClip> {
        if clip.sample_rate() == self.desc.sample_rate {
            return Ok(clip);
        }
        Ok(ResidentClip::from_audio_data(Arc::new(
            clip.data.resample(self.desc.sample_rate)?,
        )))
    }

    pub fn update_emitter(&self, emitter: Emitter, desc: EmitterDesc) -> Result<()> {
        self.validate_optional_bus(desc.bus())?;
        let bus_index = self.resolve_bus(desc.bus())?;
        let mut emitters = self
            .emitters
            .lock()
            .map_err(|_| PetalSonicError::Engine("Emitter registry is poisoned".into()))?;
        let state = emitters.get_mut(emitter)?;
        if state.desc.is_spatial() != desc.is_spatial() {
            return Err(PetalSonicError::InvalidConfiguration {
                field: "emitter_desc",
                reason: "spatial placement cannot change after emitter creation".into(),
            });
        }
        self.try_send(PlaybackCommand::UpdateEmitter(
            emitter,
            desc.source_config(0.0),
            bus_index,
        ))?;
        state.desc = desc;
        Ok(())
    }

    /// Publishes the latest complete listener + spatial-emitter transform set.
    ///
    /// An unconsumed older frame is replaced on the caller thread. The render thread
    /// observes only complete frame generations and never accumulates stale movement.
    pub fn publish_spatial_frame(&self, frame: SpatialFrame) -> Result<()> {
        self.drain_retired_spatial_frames();
        let mut latest = self
            .latest_spatial_frame
            .try_lock()
            .map_err(|_| PetalSonicError::QueuePressure)?;
        let mut emitters = self
            .emitters
            .try_lock()
            .map_err(|_| PetalSonicError::QueuePressure)?;
        emitters.apply_spatial_frame(&frame)?;
        *latest = Some(Arc::new(frame));
        Ok(())
    }

    /// Publishes a newer immutable acoustic-scene version by swapping a shared handle.
    /// Geometry and unchanged BVH chunks remain owned and shared by the snapshot backend.
    pub fn publish_acoustic_scene(&self, snapshot: AcousticSceneSnapshot) -> Result<()> {
        self.ensure_open()?;
        self.drain_retired_acoustic_scenes();
        let mut latest = self
            .latest_acoustic_scene
            .try_lock()
            .map_err(|_| PetalSonicError::QueuePressure)?;
        let current = self.acoustic_scene_version.load(Ordering::Acquire);
        if snapshot.version() <= current {
            return Err(PetalSonicError::InvalidConfiguration {
                field: "acoustic_scene.version",
                reason: format!("must increase monotonically beyond the current version {current}"),
            });
        }
        self.acoustic_scene_version
            .store(snapshot.version(), Ordering::Release);
        *latest = Some(Arc::new(snapshot));
        Ok(())
    }

    pub fn destroy_emitter(&self, emitter: Emitter) -> Result<()> {
        let mut emitters = self
            .emitters
            .lock()
            .map_err(|_| PetalSonicError::Engine("Emitter registry is poisoned".into()))?;
        emitters.get(emitter)?;
        self.try_send(PlaybackCommand::DestroyEmitter(emitter))?;
        emitters.remove(emitter)?;
        if let Ok(mut controlled) = self.controlled_voices.lock() {
            controlled.retain(|_, voice| voice.emitter != emitter || voice.detached);
        }
        Ok(())
    }

    pub fn play(&self, emitter: Emitter, options: PlayOptions) -> Result<()> {
        self.submit_play(emitter, options, None).map(|_| ())
    }

    pub fn play_controlled(
        &self,
        emitter: Emitter,
        options: PlayOptions,
        tag: PlaybackTag,
    ) -> Result<PlaybackControl> {
        let voice_id = self.submit_play(emitter, options, Some(tag))?;
        Ok(PlaybackControl {
            world_id: self.world_id,
            voice_id,
        })
    }

    fn submit_play(
        &self,
        emitter: Emitter,
        options: PlayOptions,
        completion_tag: Option<PlaybackTag>,
    ) -> Result<SourceId> {
        self.drain_retired_controls();
        self.ensure_open()?;
        let state = self.emitter_state(emitter)?;
        Self::validate_playback_rate(options.playback_rate())?;
        let bus_index = self.resolve_bus(options.bus().or(state.desc.bus()))?;
        self.reserve_voice()?;
        let voice_id = SourceId(self.next_voice_id.fetch_add(1, Ordering::Relaxed));
        if completion_tag.is_some() {
            let mut controlled = match self.controlled_voices.lock() {
                Ok(controlled) => controlled,
                Err(_) => {
                    self.active_voice_count.fetch_sub(1, Ordering::AcqRel);
                    return Err(PetalSonicError::Engine(
                        "Playback registry is poisoned".into(),
                    ));
                }
            };
            controlled.insert(
                voice_id,
                ControlledVoiceState {
                    emitter,
                    detached: options.detached,
                },
            );
        }
        let command = PlaybackCommand::Play {
            voice_id,
            emitter,
            source: state.clip.data.clone(),
            config: state.desc.source_config(options.gain_db),
            loop_mode: options.loop_mode,
            detached: options.detached,
            completion_tag,
            bus_index,
            playback_rate: options.playback_rate(),
            mono_scratch: vec![0.0; self.desc.block_size],
        };
        if let Err(error) = self.try_send(command) {
            self.active_voice_count.fetch_sub(1, Ordering::AcqRel);
            if completion_tag.is_some()
                && let Ok(mut controlled) = self.controlled_voices.lock()
            {
                controlled.remove(&voice_id);
            }
            return Err(error);
        }
        Ok(voice_id)
    }

    fn reserve_voice(&self) -> Result<()> {
        self.active_voice_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.desc.max_voices).then_some(active + 1)
            })
            .map(|_| ())
            .map_err(|_| PetalSonicError::CapacityExceeded {
                resource: "voice",
                limit: self.desc.max_voices,
            })
    }

    pub fn pause_emitter(&self, emitter: Emitter) -> Result<()> {
        self.emitter_state(emitter)?;
        self.try_send(PlaybackCommand::PauseEmitter(emitter))
    }

    pub fn resume_emitter(&self, emitter: Emitter) -> Result<()> {
        self.emitter_state(emitter)?;
        self.try_send(PlaybackCommand::ResumeEmitter(emitter))
    }

    pub fn stop_emitter(&self, emitter: Emitter) -> Result<()> {
        self.emitter_state(emitter)?;
        self.try_send(PlaybackCommand::StopEmitter(emitter))?;
        if let Ok(mut controlled) = self.controlled_voices.lock() {
            controlled.retain(|_, voice| voice.emitter != emitter);
        }
        Ok(())
    }

    pub fn seek_emitter(&self, emitter: Emitter, progress: f32) -> Result<()> {
        self.emitter_state(emitter)?;
        self.try_send(PlaybackCommand::SeekEmitter(emitter, progress))
    }

    pub fn pause_playback(&self, control: PlaybackControl) -> Result<()> {
        self.ensure_controlled(control)?;
        self.try_send(PlaybackCommand::PauseVoice(control.voice_id))
    }

    pub fn resume_playback(&self, control: PlaybackControl) -> Result<()> {
        self.ensure_controlled(control)?;
        self.try_send(PlaybackCommand::ResumeVoice(control.voice_id))
    }

    pub fn set_playback_rate(&self, control: PlaybackControl, playback_rate: f32) -> Result<()> {
        self.ensure_controlled(control)?;
        Self::validate_playback_rate(playback_rate)?;
        self.try_send(PlaybackCommand::SetVoiceRate(
            control.voice_id,
            playback_rate,
        ))
    }

    pub fn stop_playback(&self, control: PlaybackControl) -> Result<()> {
        self.ensure_controlled(control)?;
        self.try_send(PlaybackCommand::StopVoice(control.voice_id))?;
        self.controlled_voices
            .lock()
            .map_err(|_| PetalSonicError::Engine("Playback registry is poisoned".into()))?
            .remove(&control.voice_id);
        Ok(())
    }

    pub fn seek_playback(&self, control: PlaybackControl, progress: f32) -> Result<()> {
        self.ensure_controlled(control)?;
        self.try_send(PlaybackCommand::SeekVoice(control.voice_id, progress))
    }

    pub fn stop_all(&self) -> Result<()> {
        self.try_send(PlaybackCommand::StopAll)?;
        self.controlled_voices
            .lock()
            .map_err(|_| PetalSonicError::Engine("Playback registry is poisoned".into()))?
            .clear();
        Ok(())
    }

    /// Returns the implicit Master bus.
    pub fn master_bus(&self) -> Bus {
        Bus {
            world_id: self.world_id,
            index: 0,
        }
    }

    /// Resolves a declared bus by name. Matching is case-insensitive.
    pub fn bus(&self, name: &str) -> Option<Bus> {
        if name.eq_ignore_ascii_case("Master") {
            return Some(self.master_bus());
        }
        self.desc
            .buses
            .iter()
            .position(|bus| bus.name().eq_ignore_ascii_case(name))
            .and_then(|index| u16::try_from(index + 1).ok())
            .map(|index| Bus {
                world_id: self.world_id,
                index,
            })
    }

    pub fn set_bus_params(&self, bus: Bus, params: BusParams) -> Result<()> {
        let index = self.resolve_bus(Some(bus))?;
        Self::validate_bus_params(params)?;
        let mut current = self
            .bus_params
            .lock()
            .map_err(|_| PetalSonicError::Engine("Bus state is poisoned".into()))?;
        self.try_send(PlaybackCommand::UpdateBus(index, params))?;
        current[index] = params;
        Ok(())
    }

    pub fn bus_params(&self, bus: Bus) -> Result<BusParams> {
        let index = self.resolve_bus(Some(bus))?;
        self.bus_params
            .lock()
            .map_err(|_| PetalSonicError::Engine("Bus state is poisoned".into()))?
            .get(index)
            .copied()
            .ok_or(PetalSonicError::StaleBus)
    }

    fn validate_optional_bus(&self, bus: Option<Bus>) -> Result<()> {
        self.resolve_bus(bus).map(|_| ())
    }

    fn resolve_bus(&self, bus: Option<Bus>) -> Result<usize> {
        let Some(bus) = bus else {
            return Ok(0);
        };
        let index = bus.index as usize;
        if bus.world_id != self.world_id || index > self.desc.buses.len() {
            return Err(PetalSonicError::StaleBus);
        }
        Ok(index)
    }

    fn validate_bus_params(params: BusParams) -> Result<()> {
        if !params.gain_db.is_finite() {
            return Err(PetalSonicError::InvalidConfiguration {
                field: "bus.gain_db",
                reason: "must be finite".into(),
            });
        }
        Self::validate_playback_rate(params.playback_rate)
    }

    fn validate_playback_rate(playback_rate: f32) -> Result<()> {
        if playback_rate.is_finite()
            && (MIN_PLAYBACK_RATE..=MAX_PLAYBACK_RATE).contains(&playback_rate)
        {
            Ok(())
        } else {
            Err(PetalSonicError::InvalidConfiguration {
                field: "playback_rate",
                reason: format!("must be between {MIN_PLAYBACK_RATE} and {MAX_PLAYBACK_RATE}"),
            })
        }
    }

    fn emitter_state(&self, emitter: Emitter) -> Result<EmitterState> {
        self.emitters
            .lock()
            .map_err(|_| PetalSonicError::Engine("Emitter registry is poisoned".into()))?
            .get(emitter)
            .cloned()
    }

    fn ensure_controlled(&self, control: PlaybackControl) -> Result<()> {
        self.drain_retired_controls();
        if control.world_id == self.world_id
            && self
                .controlled_voices
                .lock()
                .map_err(|_| PetalSonicError::Engine("Playback registry is poisoned".into()))?
                .contains_key(&control.voice_id)
        {
            Ok(())
        } else {
            Err(PetalSonicError::StalePlayback)
        }
    }

    fn try_send(&self, command: PlaybackCommand) -> Result<()> {
        self.ensure_open()?;
        let lifecycle = matches!(
            &command,
            PlaybackCommand::StopVoice(_)
                | PlaybackCommand::StopEmitter(_)
                | PlaybackCommand::DestroyEmitter(_)
                | PlaybackCommand::StopAll
        );
        let sender = if lifecycle {
            &self.lifecycle_sender
        } else {
            &self.command_sender
        };
        match sender.try_send(command) {
            Ok(()) => {
                let high_water = if lifecycle {
                    &self.counters.lifecycle_queue_high_water
                } else {
                    &self.counters.control_queue_high_water
                };
                RuntimeCounters::observe_high_water(high_water, sender.len());
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.counters
                    .rejected_commands
                    .fetch_add(1, Ordering::Relaxed);
                Err(PetalSonicError::QueuePressure)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.counters
                    .rejected_commands
                    .fetch_add(1, Ordering::Relaxed);
                Err(PetalSonicError::RuntimeClosed)
            }
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.desc.sample_rate
    }

    pub fn is_running(&self) -> bool {
        Self::load_runtime_state(&self.runtime_state) == RuntimeState::Running
    }

    pub fn runtime_status(&self) -> RuntimeStatus {
        let active_output_device = self
            .active_output_device
            .lock()
            .map(|device| device.clone())
            .unwrap_or_default();
        RuntimeStatus {
            state: Self::load_runtime_state(&self.runtime_state),
            recovery_attempts: self.recovery_attempts.load(Ordering::Relaxed),
            active_output_device,
        }
    }

    pub fn diagnostics(&self) -> RuntimeDiagnostics {
        let (
            render_iterations,
            render_time_p50_us,
            render_time_p95_us,
            render_time_p99_us,
            render_time_max_us,
        ) = self.counters.render_summary();
        RuntimeDiagnostics {
            frames_processed: self.frames_processed.load(Ordering::Relaxed),
            underrun_count: self.underrun_count.load(Ordering::Relaxed),
            active_emitters: self
                .emitters
                .lock()
                .map(|emitters| emitters.len)
                .unwrap_or_default(),
            active_voices: self.active_voice_count.load(Ordering::Acquire),
            control_queue_depth: self.command_sender.len(),
            control_queue_high_water: self
                .counters
                .control_queue_high_water
                .load(Ordering::Relaxed),
            lifecycle_queue_depth: self.lifecycle_sender.len(),
            lifecycle_queue_high_water: self
                .counters
                .lifecycle_queue_high_water
                .load(Ordering::Relaxed),
            event_queue_depth: self.event_receiver.len(),
            event_queue_high_water: self.counters.event_queue_high_water.load(Ordering::Relaxed),
            timing_queue_depth: self.timing_receiver.len(),
            timing_queue_high_water: self
                .counters
                .timing_queue_high_water
                .load(Ordering::Relaxed),
            rejected_commands: self.counters.rejected_commands.load(Ordering::Relaxed),
            dropped_events: self.counters.dropped_events.load(Ordering::Relaxed),
            dropped_timing_events: self.counters.dropped_timing_events.load(Ordering::Relaxed),
            render_iterations,
            render_time_p50_us,
            render_time_p95_us,
            render_time_p99_us,
            render_time_max_us,
            device_generation: self.counters.device_generation.load(Ordering::Relaxed),
            recovery_attempts: self.recovery_attempts.load(Ordering::Relaxed),
            output_sample_rate: self.counters.output_sample_rate.load(Ordering::Relaxed) as u32,
            output_channels: self.counters.output_channels.load(Ordering::Relaxed) as u16,
            spatial_quality: self.desc.spatial_quality,
            latency_profile: self.desc.latency_profile,
        }
    }

    pub fn active_voice_count(&self) -> usize {
        self.active_voice_count.load(Ordering::Acquire)
    }

    pub fn frames_processed(&self) -> usize {
        self.frames_processed.load(Ordering::Relaxed)
    }

    pub fn underrun_count(&self) -> usize {
        self.underrun_count.load(Ordering::Relaxed)
    }

    pub fn drain_events(&self) -> Vec<PetalSonicEvent> {
        let events = self.event_receiver.try_iter().collect::<Vec<_>>();
        self.drain_retired_controls();
        if let Ok(mut controlled) = self.controlled_voices.lock() {
            for event in &events {
                if let PetalSonicEvent::PlaybackCompleted { control, .. } = event {
                    controlled.remove(&control.voice_id);
                }
            }
        }
        events
    }

    fn drain_retired_controls(&self) {
        if let Ok(mut controlled) = self.controlled_voices.lock() {
            while let Ok(voice_id) = self.retirement_receiver.try_recv() {
                controlled.remove(&voice_id);
            }
        }
    }

    fn drain_retired_spatial_frames(&self) {
        while self.spatial_retirement_receiver.try_recv().is_ok() {}
    }

    fn drain_retired_acoustic_scenes(&self) {
        while self.acoustic_retirement_receiver.try_recv().is_ok() {}
    }

    pub fn drain_timing_events(&self) -> Vec<RenderTimingEvent> {
        self.timing_receiver.try_iter().collect()
    }

    pub fn close(&self) -> Result<()> {
        let _close_guard = self
            .close_lock
            .lock()
            .map_err(|_| PetalSonicError::Engine("World close lock is poisoned".into()))?;
        if Self::load_runtime_state(&self.runtime_state) == RuntimeState::Closed {
            return Ok(());
        }
        self.runtime_state
            .store(RuntimeState::Closing as u8, Ordering::Release);
        self.supervisor_stop.store(true, Ordering::Release);
        if let Some(supervisor) = self
            .supervisor_thread
            .lock()
            .map_err(|_| PetalSonicError::Engine("Output supervisor lock is poisoned".into()))?
            .take()
        {
            supervisor.thread().unpark();
            supervisor.join().map_err(|_| {
                PetalSonicError::Engine("Output supervisor panicked while shutting down".into())
            })?;
        }
        self.active_voice_count.store(0, Ordering::Release);
        self.runtime_state
            .store(RuntimeState::Closed as u8, Ordering::Release);
        Ok(())
    }

    fn ensure_open(&self) -> Result<()> {
        match Self::load_runtime_state(&self.runtime_state) {
            RuntimeState::Failed => Err(PetalSonicError::RuntimeFailed),
            RuntimeState::Closing | RuntimeState::Closed => Err(PetalSonicError::RuntimeClosed),
            _ => Ok(()),
        }
    }
}

impl Drop for PetalSonicWorld {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_data::PetalSonicAudioData;
    use std::cell::Cell;
    use std::time::Duration;

    fn clip() -> ResidentClip {
        ResidentClip::from_audio_data(Arc::new(PetalSonicAudioData::new(
            vec![0.0; 16],
            48_000,
            1,
            Duration::from_secs_f64(16.0 / 48_000.0),
        )))
    }

    fn debug_windows_boundary(message: &str) {
        #[cfg(target_os = "windows")]
        eprintln!("[DEBUG-win-context] {message}");
        #[cfg(not(target_os = "windows"))]
        let _ = message;
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct FakeDevice {
        name: &'static str,
        sample_rate: u32,
        channels: u16,
    }

    struct FakeOutputDriver {
        active: Option<FakeDevice>,
        selected: Option<FakeDevice>,
        stream_failed: bool,
        permanent_format_failure: bool,
        prepared: Option<FakeDevice>,
        requires_stop_to_prepare: bool,
        actions: Vec<&'static str>,
        checked_while_active: Cell<bool>,
        advanced: Duration,
        loop_cursor_frames: usize,
        loop_length_frames: usize,
        one_shot_remaining_frames: usize,
        one_shot_completed: bool,
    }

    impl FakeOutputDriver {
        fn with_active(active: FakeDevice) -> Self {
            Self {
                active: Some(active),
                selected: Some(active),
                stream_failed: false,
                permanent_format_failure: false,
                prepared: None,
                requires_stop_to_prepare: false,
                actions: Vec::new(),
                checked_while_active: Cell::new(false),
                advanced: Duration::ZERO,
                loop_cursor_frames: 0,
                loop_length_frames: 12_000,
                one_shot_remaining_frames: 4_800,
                one_shot_completed: false,
            }
        }
    }

    impl OutputRuntimeDriver for FakeOutputDriver {
        fn drain_retired_resources(&mut self) {}

        fn output_recovery_reason(&self) -> Option<OutputRecoveryReason> {
            if self.stream_failed {
                Some(OutputRecoveryReason::StreamFailure)
            } else if self.active != self.selected {
                Some(OutputRecoveryReason::SelectionChanged)
            } else {
                None
            }
        }

        fn prepare_selected_output(&mut self) -> OutputPreparation {
            self.checked_while_active.set(self.active.is_some());
            self.actions.push("prepare");
            if self.selected.is_some() && self.requires_stop_to_prepare {
                return OutputPreparation::RequiresStop;
            }
            self.prepared = self.selected;
            if self.prepared.is_some() {
                OutputPreparation::Ready
            } else {
                OutputPreparation::Unavailable
            }
        }

        fn stop_output(&mut self) -> Result<()> {
            self.actions.push("stop");
            self.active = None;
            Ok(())
        }

        fn advance_without_output(
            &mut self,
            _commands: &EngineCommandReceivers,
            _buses: &mut [BusParams],
            elapsed: Duration,
        ) {
            self.actions.push("advance");
            self.advanced += elapsed;
            let frames = (elapsed.as_secs_f64() * 48_000.0).floor() as usize;
            self.loop_cursor_frames = (self.loop_cursor_frames + frames) % self.loop_length_frames;
            if frames >= self.one_shot_remaining_frames {
                self.one_shot_remaining_frames = 0;
                self.one_shot_completed = true;
            } else {
                self.one_shot_remaining_frames -= frames;
            }
        }

        fn start_output(
            &mut self,
            _commands: EngineCommandReceivers,
            _buses: Vec<BusParams>,
        ) -> Result<()> {
            self.actions.push("start");
            if self.permanent_format_failure {
                return Err(PetalSonicError::PermanentDeviceFailure(
                    "unsupported fake format".into(),
                ));
            }
            let Some(selected) = self.prepared.take().or(self.selected) else {
                return Err(PetalSonicError::AudioDevice("no fake device".into()));
            };
            self.active = Some(selected);
            self.stream_failed = false;
            Ok(())
        }

        fn emit_runtime_state(&self, _state: RuntimeState) {}
    }

    fn fake_command_receivers() -> EngineCommandReceivers {
        let (_, regular) = crossbeam_channel::bounded(4);
        let (_, lifecycle) = crossbeam_channel::bounded(4);
        EngineCommandReceivers::new(regular, lifecycle)
    }

    #[test]
    fn bus_declarations_are_bounded_unique_and_exclude_master() {
        let mut desc = crate::config::PetalSonicWorldDesc {
            max_buses: 1,
            buses: vec![crate::domain::BusDesc::new("Gameplay")],
            ..Default::default()
        };
        assert!(PetalSonicWorld::validate_config(&desc).is_ok());

        desc.buses.push(crate::domain::BusDesc::new("Music"));
        assert!(matches!(
            PetalSonicWorld::validate_config(&desc),
            Err(PetalSonicError::CapacityExceeded {
                resource: "bus",
                limit: 1
            })
        ));

        desc.max_buses = 2;
        desc.buses = vec![
            crate::domain::BusDesc::new("Gameplay"),
            crate::domain::BusDesc::new("gameplay"),
        ];
        assert!(PetalSonicWorld::validate_config(&desc).is_err());

        desc.buses = vec![crate::domain::BusDesc::new("Master")];
        assert!(PetalSonicWorld::validate_config(&desc).is_err());
    }

    #[test]
    fn static_configuration_and_spatial_backend_fail_before_world_is_returned() {
        let desc = crate::config::PetalSonicWorldDesc {
            distance_scaler: 0.0,
            ..Default::default()
        };
        assert!(matches!(
            PetalSonicWorld::new(desc),
            Err(PetalSonicError::InvalidConfiguration {
                field: "distance_scaler",
                ..
            })
        ));

        let desc = crate::config::PetalSonicWorldDesc {
            spatial_quality: crate::config::SpatialQuality::LowLatency,
            native_hrtf_path: Some("/petalsonic/definitely-missing.petalhrtf".into()),
            output_device: crate::config::OutputDevicePolicy::PinnedNameContains(
                "petalsonic-test-device-that-does-not-exist".into(),
            ),
            ..Default::default()
        };
        assert!(matches!(
            PetalSonicWorld::new(desc),
            Err(PetalSonicError::BackendUnavailable {
                backend: "spatial renderer",
                ..
            })
        ));
    }

    #[test]
    fn fake_default_device_switch_probes_b_before_releasing_a() {
        let a = FakeDevice {
            name: "A",
            sample_rate: 48_000,
            channels: 2,
        };
        let b = FakeDevice {
            name: "B",
            sample_rate: 44_100,
            channels: 6,
        };
        let mut driver = FakeOutputDriver::with_active(a);
        driver.selected = Some(b);
        let commands = fake_command_receivers();
        let buses = Mutex::new(vec![BusParams::default()]);
        let runtime_state = AtomicU8::new(RuntimeState::Running as u8);
        let recovery_attempts = AtomicU64::new(0);
        let mut recovery_buses = vec![BusParams::default()];
        let now = Instant::now();
        let mut schedule = SupervisorSchedule::new(now);

        PetalSonicWorld::supervisor_tick(
            &mut driver,
            &commands,
            &buses,
            &runtime_state,
            &recovery_attempts,
            &mut recovery_buses,
            &mut schedule,
            now,
        );

        assert!(driver.checked_while_active.get());
        assert_eq!(driver.actions, ["prepare", "stop", "advance", "start"]);
        assert_eq!(driver.active, Some(b));
        assert_eq!(
            PetalSonicWorld::load_runtime_state(&runtime_state),
            RuntimeState::Running
        );
        assert_eq!(recovery_attempts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn fake_recovery_keeps_timeline_and_retries_with_virtual_time() {
        let a = FakeDevice {
            name: "A",
            sample_rate: 48_000,
            channels: 2,
        };
        let b = FakeDevice {
            name: "B",
            sample_rate: 96_000,
            channels: 2,
        };
        let mut driver = FakeOutputDriver::with_active(a);
        driver.stream_failed = true;
        driver.selected = None;
        let commands = fake_command_receivers();
        let buses = Mutex::new(vec![BusParams::default()]);
        let runtime_state = AtomicU8::new(RuntimeState::Running as u8);
        let recovery_attempts = AtomicU64::new(0);
        let mut recovery_buses = vec![BusParams::default()];
        let now = Instant::now();
        let mut schedule = SupervisorSchedule::new(now);

        PetalSonicWorld::supervisor_tick(
            &mut driver,
            &commands,
            &buses,
            &runtime_state,
            &recovery_attempts,
            &mut recovery_buses,
            &mut schedule,
            now,
        );
        assert_eq!(
            PetalSonicWorld::load_runtime_state(&runtime_state),
            RuntimeState::Recovering
        );
        assert_eq!(recovery_attempts.load(Ordering::Relaxed), 1);

        driver.selected = Some(b);
        PetalSonicWorld::supervisor_tick(
            &mut driver,
            &commands,
            &buses,
            &runtime_state,
            &recovery_attempts,
            &mut recovery_buses,
            &mut schedule,
            now + OUTPUT_RETRY_INTERVAL,
        );

        assert_eq!(driver.active, Some(b));
        assert_eq!(driver.advanced, OUTPUT_RETRY_INTERVAL);
        assert_eq!(driver.loop_cursor_frames, 0);
        assert!(driver.one_shot_completed);
        assert_eq!(
            PetalSonicWorld::load_runtime_state(&runtime_state),
            RuntimeState::Running
        );
        assert_eq!(recovery_attempts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn fake_healthy_a_is_kept_when_new_default_is_not_openable() {
        let a = FakeDevice {
            name: "A",
            sample_rate: 48_000,
            channels: 2,
        };
        let mut driver = FakeOutputDriver::with_active(a);
        driver.selected = None;
        let commands = fake_command_receivers();
        let buses = Mutex::new(vec![BusParams::default()]);
        let runtime_state = AtomicU8::new(RuntimeState::Running as u8);
        let recovery_attempts = AtomicU64::new(0);
        let mut recovery_buses = vec![BusParams::default()];
        let now = Instant::now();
        let mut schedule = SupervisorSchedule::new(now);

        PetalSonicWorld::supervisor_tick(
            &mut driver,
            &commands,
            &buses,
            &runtime_state,
            &recovery_attempts,
            &mut recovery_buses,
            &mut schedule,
            now,
        );

        assert_eq!(driver.active, Some(a));
        assert_eq!(driver.actions, ["prepare"]);
        assert_eq!(
            PetalSonicWorld::load_runtime_state(&runtime_state),
            RuntimeState::Running
        );
    }

    #[test]
    fn fake_exclusive_backend_falls_back_to_stop_then_rebuild() {
        let a = FakeDevice {
            name: "A",
            sample_rate: 48_000,
            channels: 2,
        };
        let b = FakeDevice {
            name: "B",
            sample_rate: 44_100,
            channels: 2,
        };
        let mut driver = FakeOutputDriver::with_active(a);
        driver.selected = Some(b);
        driver.requires_stop_to_prepare = true;
        let commands = fake_command_receivers();
        let buses = Mutex::new(vec![BusParams::default()]);
        let runtime_state = AtomicU8::new(RuntimeState::Running as u8);
        let recovery_attempts = AtomicU64::new(0);
        let mut recovery_buses = vec![BusParams::default()];
        let now = Instant::now();
        let mut schedule = SupervisorSchedule::new(now);

        PetalSonicWorld::supervisor_tick(
            &mut driver,
            &commands,
            &buses,
            &runtime_state,
            &recovery_attempts,
            &mut recovery_buses,
            &mut schedule,
            now,
        );

        assert_eq!(driver.actions, ["prepare", "stop", "advance", "start"]);
        assert_eq!(driver.active, Some(b));
        assert_eq!(
            PetalSonicWorld::load_runtime_state(&runtime_state),
            RuntimeState::Running
        );
    }

    #[test]
    fn fake_permanent_format_failure_is_not_retried_as_missing_device() {
        let a = FakeDevice {
            name: "A",
            sample_rate: 48_000,
            channels: 2,
        };
        let mut driver = FakeOutputDriver::with_active(a);
        driver.active = None;
        driver.permanent_format_failure = true;
        let commands = fake_command_receivers();
        let buses = Mutex::new(vec![BusParams::default()]);
        let runtime_state = AtomicU8::new(RuntimeState::Recovering as u8);
        let recovery_attempts = AtomicU64::new(0);
        let mut recovery_buses = vec![BusParams::default()];
        let now = Instant::now();
        let mut schedule = SupervisorSchedule::new(now);

        PetalSonicWorld::supervisor_tick(
            &mut driver,
            &commands,
            &buses,
            &runtime_state,
            &recovery_attempts,
            &mut recovery_buses,
            &mut schedule,
            now,
        );

        assert_eq!(
            PetalSonicWorld::load_runtime_state(&runtime_state),
            RuntimeState::Failed
        );
        assert_eq!(recovery_attempts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn lifecycle_commands_have_reserved_capacity_under_control_pressure() {
        let desc = crate::config::PetalSonicWorldDesc {
            control_queue_capacity: 1,
            lifecycle_queue_capacity: 1,
            output_device: crate::config::OutputDevicePolicy::PinnedNameContains(
                "petalsonic-test-device-that-does-not-exist".into(),
            ),
            ..Default::default()
        };
        let world = PetalSonicWorld::new(desc).unwrap();
        let emitter = world
            .create_emitter(clip(), EmitterDesc::non_spatial())
            .unwrap();

        let mut rejected = false;
        for _ in 0..10_000 {
            if matches!(
                world.pause_emitter(emitter),
                Err(PetalSonicError::QueuePressure)
            ) {
                rejected = true;
                break;
            }
        }
        assert!(rejected, "regular queue should report bounded pressure");
        world
            .stop_emitter(emitter)
            .expect("lifecycle reserve must remain independently available");

        let diagnostics = world.diagnostics();
        assert_eq!(diagnostics.control_queue_high_water, 1);
        assert_eq!(diagnostics.lifecycle_queue_high_water, 1);
        assert!(diagnostics.rejected_commands >= 1);
        world.close().unwrap();
    }

    #[test]
    fn near_capacity_runtime_remains_bounded_under_snapshot_and_bus_pressure() {
        const EMITTERS: usize = 24;
        const VOICES: usize = 48;
        let desc = crate::config::PetalSonicWorldDesc {
            max_emitters: EMITTERS,
            max_voices: VOICES,
            max_buses: 1,
            buses: vec![crate::domain::BusDesc::new("Gameplay")],
            control_queue_capacity: 128,
            lifecycle_queue_capacity: 64,
            output_device: crate::config::OutputDevicePolicy::PinnedNameContains(
                "petalsonic-test-device-that-does-not-exist".into(),
            ),
            ..Default::default()
        };
        let world = PetalSonicWorld::new(desc).unwrap();
        let mut emitters = Vec::with_capacity(EMITTERS);
        for index in 0..EMITTERS {
            let pose = Pose::from_position(crate::math::Vec3::new(index as f32, 0.0, 0.0));
            emitters.push(
                world
                    .create_emitter(clip(), EmitterDesc::spatial(pose))
                    .unwrap(),
            );
        }
        assert!(matches!(
            world.create_emitter(clip(), EmitterDesc::non_spatial()),
            Err(PetalSonicError::CapacityExceeded {
                resource: "emitter",
                limit: EMITTERS
            })
        ));

        for emitter in &emitters {
            world.play(*emitter, PlayOptions::looping()).unwrap();
            world.play(*emitter, PlayOptions::looping()).unwrap();
        }
        assert_eq!(world.active_voice_count(), VOICES);
        assert!(matches!(
            world.play(emitters[0], PlayOptions::looping()),
            Err(PetalSonicError::CapacityExceeded {
                resource: "voice",
                limit: VOICES
            })
        ));

        let gameplay = world.bus("Gameplay").unwrap();
        let mut queue_pressure_observed = false;
        for generation in 0usize..1_024 {
            let states = emitters
                .iter()
                .enumerate()
                .map(|(index, emitter)| {
                    crate::domain::EmitterSpatialState::new(
                        *emitter,
                        Pose::from_position(crate::math::Vec3::new(
                            index as f32,
                            generation as f32 * 0.001,
                            0.0,
                        )),
                    )
                })
                .collect();
            world
                .publish_spatial_frame(SpatialFrame::new(Pose::default(), states))
                .unwrap();

            let params = BusParams {
                gain_db: if generation.is_multiple_of(2) {
                    -3.0
                } else {
                    0.0
                },
                ..Default::default()
            };
            match world.set_bus_params(gameplay, params) {
                Ok(()) => {}
                Err(PetalSonicError::QueuePressure) => queue_pressure_observed = true,
                Err(error) => panic!("unexpected bus update failure: {error}"),
            }
        }

        let diagnostics = world.diagnostics();
        assert_eq!(diagnostics.active_emitters, EMITTERS);
        assert_eq!(diagnostics.active_voices, VOICES);
        assert!(diagnostics.control_queue_depth <= 128);
        assert!(diagnostics.control_queue_high_water <= 128);
        assert_eq!(
            world
                .latest_spatial_frame
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .emitters()
                .len(),
            EMITTERS
        );
        assert_eq!(queue_pressure_observed, diagnostics.rejected_commands > 0);

        world.stop_all().unwrap();
        world.close().unwrap();
        assert_eq!(world.runtime_status().state, RuntimeState::Closed);
    }

    #[test]
    fn bounded_event_pressure_is_observable() {
        let desc = crate::config::PetalSonicWorldDesc {
            max_emitters: 1,
            max_voices: 16,
            control_queue_capacity: 32,
            event_queue_capacity: 1,
            output_device: crate::config::OutputDevicePolicy::PinnedNameContains(
                "petalsonic-test-device-that-does-not-exist".into(),
            ),
            ..Default::default()
        };
        let world = PetalSonicWorld::new(desc).unwrap();
        let emitter = world
            .create_emitter(clip(), EmitterDesc::non_spatial())
            .unwrap();
        for tag in 0..16 {
            world
                .play_controlled(emitter, PlayOptions::once(), PlaybackTag(tag))
                .unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && world.diagnostics().dropped_events == 0 {
            std::thread::yield_now();
        }

        let diagnostics = world.diagnostics();
        assert_eq!(diagnostics.event_queue_high_water, 1);
        assert!(diagnostics.event_queue_depth <= 1);
        assert!(diagnostics.dropped_events > 0);
        world.close().unwrap();
    }

    #[test]
    fn spatial_publication_overwrites_unconsumed_frames_atomically() {
        let desc = crate::config::PetalSonicWorldDesc {
            output_device: crate::config::OutputDevicePolicy::PinnedNameContains(
                "petalsonic-test-device-that-does-not-exist".into(),
            ),
            ..Default::default()
        };
        let world = PetalSonicWorld::new(desc).unwrap();
        let emitter = world
            .create_emitter(clip(), EmitterDesc::spatial(Pose::default()))
            .unwrap();
        let first_listener = Pose::from_position(crate::math::Vec3::new(1.0, 0.0, 0.0));
        let second_listener = Pose::from_position(crate::math::Vec3::new(2.0, 0.0, 0.0));
        let first_emitter = Pose::from_position(crate::math::Vec3::new(10.0, 0.0, 0.0));
        let second_emitter = Pose::from_position(crate::math::Vec3::new(20.0, 0.0, 0.0));

        world
            .publish_spatial_frame(SpatialFrame::new(
                first_listener,
                vec![crate::domain::EmitterSpatialState::new(
                    emitter,
                    first_emitter,
                )],
            ))
            .unwrap();
        world
            .publish_spatial_frame(SpatialFrame::new(
                second_listener,
                vec![crate::domain::EmitterSpatialState::new(
                    emitter,
                    second_emitter,
                )],
            ))
            .unwrap();

        let latest = world
            .latest_spatial_frame
            .lock()
            .unwrap()
            .clone()
            .expect("latest frame should remain available while recovering");
        assert_eq!(latest.listener(), second_listener);
        assert_eq!(latest.emitters()[0].pose, second_emitter);
        assert_eq!(world.diagnostics().control_queue_depth, 0);
        world.close().unwrap();
    }

    #[test]
    fn worlds_close_idempotently_and_remain_isolated() {
        let desc = crate::config::PetalSonicWorldDesc {
            output_device: crate::config::OutputDevicePolicy::PinnedNameContains(
                "petalsonic-test-device-that-does-not-exist".into(),
            ),
            ..Default::default()
        };
        let first = PetalSonicWorld::new(desc.clone()).unwrap();
        let second = PetalSonicWorld::new(desc).unwrap();
        let first_emitter = first
            .create_emitter(clip(), EmitterDesc::non_spatial())
            .unwrap();
        let second_emitter = second
            .create_emitter(clip(), EmitterDesc::non_spatial())
            .unwrap();
        let first_control = first
            .play_controlled(first_emitter, PlayOptions::looping(), PlaybackTag(1))
            .unwrap();
        let second_control = second
            .play_controlled(second_emitter, PlayOptions::looping(), PlaybackTag(2))
            .unwrap();

        assert!(matches!(
            first.pause_emitter(second_emitter),
            Err(PetalSonicError::StaleEmitter)
        ));
        assert!(matches!(
            first.pause_playback(second_control),
            Err(PetalSonicError::StalePlayback)
        ));
        first.pause_playback(first_control).unwrap();

        first.close().unwrap();
        first.close().unwrap();
        assert_eq!(first.runtime_status().state, RuntimeState::Closed);
        assert!(matches!(
            first.create_emitter(clip(), EmitterDesc::non_spatial()),
            Err(PetalSonicError::RuntimeClosed)
        ));

        second
            .play(second_emitter, PlayOptions::looping())
            .expect("closing another world must not affect this runtime");
        assert_ne!(second.runtime_status().state, RuntimeState::Closed);
        second.close().unwrap();
    }

    #[test]
    fn world_can_be_recreated_after_close() {
        let desc = crate::config::PetalSonicWorldDesc {
            output_device: crate::config::OutputDevicePolicy::PinnedNameContains(
                "petalsonic-test-device-that-does-not-exist".into(),
            ),
            ..Default::default()
        };

        debug_windows_boundary("recreate: creating first world");
        let first = PetalSonicWorld::new(desc.clone()).unwrap();
        debug_windows_boundary("recreate: closing first world");
        first.close().unwrap();
        drop(first);
        debug_windows_boundary("recreate: dropped first world; creating second world");

        let second = PetalSonicWorld::new(desc).unwrap();
        debug_windows_boundary("recreate: closing second world");
        second.close().unwrap();
        debug_windows_boundary("recreate: closed second world");
    }

    #[test]
    fn detached_control_survives_emitter_destruction() {
        let desc = crate::config::PetalSonicWorldDesc {
            output_device: crate::config::OutputDevicePolicy::PinnedNameContains(
                "petalsonic-test-device-that-does-not-exist".into(),
            ),
            ..Default::default()
        };
        debug_windows_boundary("detached: creating world");
        let world = PetalSonicWorld::new(desc).unwrap();
        debug_windows_boundary("detached: creating emitter");
        let emitter = world
            .create_emitter(clip(), EmitterDesc::non_spatial())
            .unwrap();
        let control = world
            .play_controlled(emitter, PlayOptions::looping().detached(), PlaybackTag(7))
            .unwrap();

        debug_windows_boundary("detached: destroying emitter");
        world.destroy_emitter(emitter).unwrap();
        assert!(matches!(
            world.play(emitter, PlayOptions::once()),
            Err(PetalSonicError::StaleEmitter)
        ));
        world.pause_playback(control).unwrap();
        world.stop_playback(control).unwrap();
        debug_windows_boundary("detached: closing world");
        world.close().unwrap();
        debug_windows_boundary("detached: closed world");
    }

    #[test]
    fn unavailable_device_keeps_world_alive_and_expires_one_shots() {
        let desc = crate::config::PetalSonicWorldDesc {
            output_device: crate::config::OutputDevicePolicy::PinnedNameContains(
                "petalsonic-test-device-that-does-not-exist".into(),
            ),
            block_size: 64,
            ..Default::default()
        };
        let world = PetalSonicWorld::new(desc).unwrap();
        let emitter = world
            .create_emitter(clip(), EmitterDesc::non_spatial())
            .unwrap();
        let tag = PlaybackTag(42);
        world
            .play_controlled(emitter, PlayOptions::once(), tag)
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut completed = false;
        while Instant::now() < deadline {
            completed |= world.drain_events().into_iter().any(|event| {
                matches!(
                    event,
                    PetalSonicEvent::PlaybackCompleted {
                        tag: PlaybackTag(42),
                        ..
                    }
                )
            });
            if completed && world.active_voice_count() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(
            completed,
            "one-shot should expire while output is recovering"
        );
        assert_eq!(world.active_voice_count(), 0);
        assert_eq!(world.runtime_status().state, RuntimeState::Recovering);
        assert!(world.runtime_status().recovery_attempts > 0);
        world.close().unwrap();
        assert_eq!(world.runtime_status().state, RuntimeState::Closed);
    }

    #[test]
    fn emitter_registry_rejects_stale_generation_after_slot_reuse() {
        let mut registry = EmitterRegistry::new(1, 1);
        let first = registry
            .insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::default(),
            })
            .unwrap();
        registry.remove(first).unwrap();
        let second = registry
            .insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::default(),
            })
            .unwrap();

        assert_eq!(first.index, second.index);
        assert_ne!(first.generation, second.generation);
        assert!(matches!(
            registry.get(first),
            Err(PetalSonicError::StaleEmitter)
        ));
    }

    #[test]
    fn emitter_registry_enforces_capacity_and_recovers_after_remove() {
        let mut registry = EmitterRegistry::new(1, 1);
        let emitter = registry
            .insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::default(),
            })
            .unwrap();
        assert!(matches!(
            registry.insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::default(),
            }),
            Err(PetalSonicError::CapacityExceeded {
                resource: "emitter",
                limit: 1
            })
        ));
        registry.remove(emitter).unwrap();
        registry
            .insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::default(),
            })
            .unwrap();
    }

    #[test]
    fn spatial_frame_must_be_complete_before_any_pose_is_updated() {
        let mut registry = EmitterRegistry::new(2, 1);
        let first = registry
            .insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::spatial(Pose::default()),
            })
            .unwrap();
        let second = registry
            .insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::spatial(Pose::default()),
            })
            .unwrap();
        let moved = Pose::from_position(crate::math::Vec3::new(1.0, 2.0, 3.0));

        let incomplete = SpatialFrame::new(
            Pose::default(),
            vec![crate::domain::EmitterSpatialState::new(first, moved)],
        );
        assert!(matches!(
            registry.apply_spatial_frame(&incomplete),
            Err(PetalSonicError::InvalidConfiguration {
                field: "spatial_frame",
                ..
            })
        ));
        assert_eq!(
            registry.get(first).unwrap().desc.pose(),
            Some(Pose::default())
        );

        let complete = SpatialFrame::new(
            Pose::default(),
            vec![
                crate::domain::EmitterSpatialState::new(first, moved),
                crate::domain::EmitterSpatialState::new(second, Pose::default()),
            ],
        );
        registry.apply_spatial_frame(&complete).unwrap();
        assert_eq!(registry.get(first).unwrap().desc.pose(), Some(moved));
    }
}
