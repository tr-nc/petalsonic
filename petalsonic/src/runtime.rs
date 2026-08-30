use crate::acoustic_propagation::{AcousticPropagation, AcousticResponse};
use crate::acoustics::AcousticSceneSnapshot;
use crate::audio_data::PetalSonicAudioData;
use crate::config::{PetalSonicWorldDesc, SourceConfig};
use crate::domain::{
    BusParams, DirectPath, Emitter, EnvironmentSend, OcclusionProfile, PlayCommandId, PlaybackTag,
    SourceExtent, SpatialFrame, VoiceId,
};
use crate::engine::{EngineCommandReceivers, EngineObservability, EngineStartup, PetalSonicEngine};
use crate::error::{PetalSonicError, Result};
use crate::events::{
    AcousticTelemetryDiagnostics, AcousticTelemetryEvent, PetalSonicEvent, RenderTimingEvent,
    RuntimeCounters, RuntimeDiagnostics, RuntimeState, RuntimeStatus, VoiceTelemetryDiagnostics,
    VoiceTelemetryEvent,
};
use crate::platform::output::{
    CpalOutputPlatform, OutputPlatform, OutputRecoveryRequest, OutputRecoveryResult,
};
use crate::playback::{LoopMode, PlaybackCommand};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(crate) const OUTPUT_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// A domain-validated discrete intent accepted by one World's runtime.
///
/// Queue selection and render-path preparation deliberately remain behind the
/// runtime seam; callers cannot acquire or route through the underlying endpoints.
pub(crate) enum RuntimeIntent {
    Play {
        voice_id: VoiceId,
        emitter: Emitter,
        source: Arc<PetalSonicAudioData>,
        config: SourceConfig,
        loop_mode: LoopMode,
        detached: bool,
        completion_tag: Option<PlaybackTag>,
        bus_index: usize,
        playback_rate: f32,
        direct_path: DirectPath,
        environment_send: EnvironmentSend,
        play_command_id: Option<PlayCommandId>,
        source_extent: SourceExtent,
        occlusion_profile: OcclusionProfile,
    },
    UpdateEmitter(Emitter, SourceConfig, usize),
    DestroyEmitter(Emitter),
    PauseEmitter(Emitter),
    ResumeEmitter(Emitter),
    StopEmitter(Emitter),
    SeekEmitter(Emitter, f32),
    PauseVoice(VoiceId),
    ResumeVoice(VoiceId),
    SetVoiceRate(VoiceId, f32),
    StopVoice(VoiceId),
    SeekVoice(VoiceId, f32),
    StopAll,
    UpdateBus(usize, BusParams),
}

impl RuntimeIntent {
    fn into_command(self, block_size: usize) -> PlaybackCommand {
        match self {
            Self::Play {
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
                source_extent,
                occlusion_profile,
            } => PlaybackCommand::Play {
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
                source_extent,
                occlusion_profile,
                mono_scratch: vec![0.0; block_size],
            },
            Self::UpdateEmitter(emitter, config, bus) => {
                PlaybackCommand::UpdateEmitter(emitter, config, bus)
            }
            Self::DestroyEmitter(emitter) => PlaybackCommand::DestroyEmitter(emitter),
            Self::PauseEmitter(emitter) => PlaybackCommand::PauseEmitter(emitter),
            Self::ResumeEmitter(emitter) => PlaybackCommand::ResumeEmitter(emitter),
            Self::StopEmitter(emitter) => PlaybackCommand::StopEmitter(emitter),
            Self::SeekEmitter(emitter, progress) => PlaybackCommand::SeekEmitter(emitter, progress),
            Self::PauseVoice(voice) => PlaybackCommand::PauseVoice(voice),
            Self::ResumeVoice(voice) => PlaybackCommand::ResumeVoice(voice),
            Self::SetVoiceRate(voice, rate) => PlaybackCommand::SetVoiceRate(voice, rate),
            Self::StopVoice(voice) => PlaybackCommand::StopVoice(voice),
            Self::SeekVoice(voice, progress) => PlaybackCommand::SeekVoice(voice, progress),
            Self::StopAll => PlaybackCommand::StopAll,
            Self::UpdateBus(index, params) => PlaybackCommand::UpdateBus(index, params),
        }
    }
}

pub(crate) trait OutputRuntimeDriver {
    fn drain_retired_resources(&mut self);
    fn reconcile_output(&mut self, request: OutputRecoveryRequest) -> OutputRecoveryResult;
    fn emit_runtime_state(&self, state: RuntimeState);
}

impl OutputRuntimeDriver for PetalSonicEngine {
    fn drain_retired_resources(&mut self) {
        PetalSonicEngine::drain_retired_backend_resources(self);
    }

    fn reconcile_output(&mut self, request: OutputRecoveryRequest) -> OutputRecoveryResult {
        PetalSonicEngine::reconcile_output(self, request)
    }

    fn emit_runtime_state(&self, state: RuntimeState) {
        PetalSonicEngine::emit_runtime_state(self, state);
    }
}

pub(crate) struct SupervisorSchedule {
    next_retry: Instant,
    next_health_probe: Instant,
    last_advance: Instant,
}

impl SupervisorSchedule {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            next_retry: now,
            next_health_probe: now,
            last_advance: now,
        }
    }
}

/// The active runtime owned by exactly one [`PetalSonicWorld`](crate::PetalSonicWorld).
///
/// This module owns all thread, endpoint, propagation, output-recovery, and shutdown
/// coordination. The World facade only validates domain intent and maintains caller-facing
/// handle registries.
pub(crate) struct AudioRuntime {
    block_size: usize,
    max_voices: usize,
    bus_params: Arc<Mutex<Vec<BusParams>>>,
    active_voice_count: Arc<AtomicUsize>,
    retirement_receiver: Receiver<VoiceId>,
    latest_spatial_frame: Arc<Mutex<Option<Arc<SpatialFrame>>>>,
    spatial_retirement_receiver: Receiver<Arc<SpatialFrame>>,
    spatial_frame_revision: AtomicU64,
    spatial_sim_time_bits: AtomicU64,
    acoustic_propagation: AcousticPropagation,
    acoustic_response_retirement_receiver: Receiver<Arc<AcousticResponse>>,
    acoustic_scene_version: Arc<AtomicU64>,
    environmental_acoustics_enabled: Arc<AtomicBool>,
    command_sender: Sender<PlaybackCommand>,
    lifecycle_sender: Sender<PlaybackCommand>,
    counters: Arc<RuntimeCounters>,
    frames_processed: Arc<AtomicUsize>,
    underrun_count: Arc<AtomicUsize>,
    active_output_device: Arc<Mutex<Option<String>>>,
    event_receiver: Receiver<PetalSonicEvent>,
    voice_telemetry_receiver: Receiver<VoiceTelemetryEvent>,
    acoustic_telemetry_receiver: Receiver<AcousticTelemetryEvent>,
    timing_receiver: Receiver<RenderTimingEvent>,
    runtime_state: Arc<AtomicU8>,
    recovery_attempts: Arc<AtomicU64>,
    supervisor_stop: Arc<AtomicBool>,
    close_lock: Mutex<()>,
    supervisor_thread: Mutex<Option<JoinHandle<()>>>,
}

impl AudioRuntime {
    pub(crate) fn start(config: &PetalSonicWorldDesc) -> Result<Self> {
        Self::start_with_output_factory(
            config,
            Box::new(|| Ok(Box::new(CpalOutputPlatform::new()?) as Box<dyn OutputPlatform>)),
        )
    }

    #[cfg(test)]
    pub(crate) fn start_with_output(
        config: &PetalSonicWorldDesc,
        output: impl FnOnce() -> Result<Box<dyn OutputPlatform>> + Send + 'static,
    ) -> Result<Self> {
        Self::start_with_output_factory(config, Box::new(output))
    }

    fn start_with_output_factory(
        config: &PetalSonicWorldDesc,
        output: Box<dyn FnOnce() -> Result<Box<dyn OutputPlatform>> + Send>,
    ) -> Result<Self> {
        let (command_sender, command_receiver) =
            crossbeam_channel::bounded(config.control_queue_capacity);
        let (lifecycle_sender, lifecycle_receiver) =
            crossbeam_channel::bounded(config.lifecycle_queue_capacity);
        let active_voice_count = Arc::new(AtomicUsize::new(0));
        let (retirement_sender, retirement_receiver) =
            crossbeam_channel::bounded(config.max_voices);
        let latest_spatial_frame = Arc::new(Mutex::new(None));
        let (spatial_retirement_sender, spatial_retirement_receiver) =
            crossbeam_channel::bounded(1);
        let initial_acoustic_scene = config.acoustic_scene.clone().map(Arc::new);
        let acoustic_scene_version = Arc::new(AtomicU64::new(
            initial_acoustic_scene
                .as_ref()
                .map(|scene| scene.version())
                .unwrap_or(0),
        ));
        let environmental_acoustics_enabled =
            Arc::new(AtomicBool::new(config.environmental_acoustics_enabled));
        let acoustic_propagation = AcousticPropagation::new(
            config.distance_scaler,
            environmental_acoustics_enabled.clone(),
            config.environmental_acoustics_quality,
            config.environmental_acoustics_budget,
            config.max_voices,
            config.event_queue_capacity,
        )
        .map_err(|error| {
            PetalSonicError::Engine(format!(
                "Failed to start acoustic propagation worker: {error}"
            ))
        })?;
        let acoustic_telemetry_receiver = acoustic_propagation.telemetry_receiver();
        if let Some(scene) = initial_acoustic_scene {
            acoustic_propagation.publish_scene(scene).map_err(|_| {
                PetalSonicError::Engine("Failed to publish initial acoustic scene".into())
            })?;
        }
        let (acoustic_response_retirement_sender, acoustic_response_retirement_receiver) =
            crossbeam_channel::bounded(2);
        let bus_params = Arc::new(Mutex::new(
            std::iter::once(BusParams::default())
                .chain(config.buses.iter().map(|bus| bus.params()))
                .collect::<Vec<_>>(),
        ));
        let (ports, observability) = PetalSonicEngine::create_runtime_ports(config);
        let startup = EngineStartup {
            desc: config.clone(),
            active_voice_count: active_voice_count.clone(),
            retirement_sender,
            latest_spatial_frame: latest_spatial_frame.clone(),
            spatial_retirement_sender,
            latest_acoustic_response: acoustic_propagation.latest_response_slot(),
            acoustic_response_retirement_sender,
            acoustic_voice_input: acoustic_propagation.voice_input(),
            acoustic_scene_version: acoustic_scene_version.clone(),
            environmental_acoustics_enabled: environmental_acoustics_enabled.clone(),
            ports,
        };
        let EngineObservability {
            frames_processed,
            underrun_count,
            active_device_name,
            event_receiver,
            voice_telemetry_receiver,
            timing_receiver,
            counters,
        } = observability;
        let runtime_state = Arc::new(AtomicU8::new(RuntimeState::Recovering as u8));
        let recovery_attempts = Arc::new(AtomicU64::new(0));
        let supervisor_stop = Arc::new(AtomicBool::new(false));
        let supervisor_thread = Self::spawn_output_supervisor(
            startup,
            output,
            EngineCommandReceivers::new(command_receiver, lifecycle_receiver),
            bus_params.clone(),
            runtime_state.clone(),
            recovery_attempts.clone(),
            supervisor_stop.clone(),
        )?;

        Ok(Self {
            block_size: config.block_size,
            max_voices: config.max_voices,
            bus_params,
            active_voice_count,
            retirement_receiver,
            latest_spatial_frame,
            spatial_retirement_receiver,
            spatial_frame_revision: AtomicU64::new(0),
            spatial_sim_time_bits: AtomicU64::new(0.0f64.to_bits()),
            acoustic_propagation,
            acoustic_response_retirement_receiver,
            acoustic_scene_version,
            environmental_acoustics_enabled,
            command_sender,
            lifecycle_sender,
            counters,
            frames_processed,
            underrun_count,
            active_output_device: active_device_name,
            event_receiver,
            voice_telemetry_receiver,
            acoustic_telemetry_receiver,
            timing_receiver,
            runtime_state,
            recovery_attempts,
            supervisor_stop,
            close_lock: Mutex::new(()),
            supervisor_thread: Mutex::new(Some(supervisor_thread)),
        })
    }

    pub(crate) fn ensure_open(&self) -> Result<()> {
        match Self::load_runtime_state(&self.runtime_state) {
            RuntimeState::Failed => Err(PetalSonicError::RuntimeFailed),
            RuntimeState::Closing | RuntimeState::Closed => Err(PetalSonicError::RuntimeClosed),
            _ => Ok(()),
        }
    }

    pub(crate) fn reserve_voice(&self) -> Result<()> {
        self.active_voice_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.max_voices).then_some(active + 1)
            })
            .map(|_| ())
            .map_err(|_| PetalSonicError::CapacityExceeded {
                resource: "voice",
                limit: self.max_voices,
            })
    }

    pub(crate) fn release_reserved_voice(&self) {
        self.active_voice_count.fetch_sub(1, Ordering::AcqRel);
    }

    pub(crate) fn try_submit(&self, intent: RuntimeIntent) -> Result<()> {
        self.ensure_open()?;
        let command = intent.into_command(self.block_size);
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

    pub(crate) fn spatial_cursor(&self) -> (u64, f64) {
        (
            self.spatial_frame_revision.load(Ordering::Acquire),
            f64::from_bits(self.spatial_sim_time_bits.load(Ordering::Acquire)),
        )
    }

    pub(crate) fn publish_spatial_frame(&self, frame: Arc<SpatialFrame>) -> Result<()> {
        self.ensure_open()?;
        self.drain_retired_spatial_frames();
        self.drain_retired_acoustic_responses();
        let mut latest = self
            .latest_spatial_frame
            .try_lock()
            .map_err(|_| PetalSonicError::QueuePressure)?;
        self.acoustic_propagation
            .publish_spatial_frame(frame.clone())
            .map_err(|_| PetalSonicError::QueuePressure)?;
        self.spatial_frame_revision
            .store(frame.revision(), Ordering::Release);
        self.spatial_sim_time_bits
            .store(frame.sim_time_seconds().to_bits(), Ordering::Release);
        *latest = Some(frame);
        Ok(())
    }

    pub(crate) fn acoustic_scene_version(&self) -> u64 {
        self.acoustic_scene_version.load(Ordering::Acquire)
    }

    pub(crate) fn publish_acoustic_scene(
        &self,
        snapshot: Arc<AcousticSceneSnapshot>,
    ) -> Result<()> {
        self.ensure_open()?;
        self.drain_retired_acoustic_responses();
        self.acoustic_propagation
            .publish_scene(snapshot.clone())
            .map_err(|_| PetalSonicError::QueuePressure)?;
        self.acoustic_scene_version
            .store(snapshot.version(), Ordering::Release);
        Ok(())
    }

    pub(crate) fn set_environmental_acoustics_enabled(&self, enabled: bool) -> Result<()> {
        self.ensure_open()?;
        self.acoustic_propagation.set_enabled(enabled);
        Ok(())
    }

    pub(crate) fn environmental_acoustics_enabled(&self) -> bool {
        self.environmental_acoustics_enabled.load(Ordering::Acquire)
    }

    pub(crate) fn set_environmental_acoustics_quality(&self, quality: f32) -> Result<()> {
        self.ensure_open()?;
        self.acoustic_propagation.set_quality(quality);
        Ok(())
    }

    pub(crate) fn environmental_acoustics_quality(&self) -> f32 {
        self.acoustic_propagation.quality()
    }

    #[cfg(test)]
    pub(crate) fn fail_acoustic_worker_for_test(&self) {
        self.acoustic_propagation.fail_worker_for_test();
    }

    pub(crate) fn set_bus_params(&self, index: usize, params: BusParams) -> Result<()> {
        let mut current = self
            .bus_params
            .lock()
            .map_err(|_| PetalSonicError::Engine("Bus state is poisoned".into()))?;
        self.try_submit(RuntimeIntent::UpdateBus(index, params))?;
        current[index] = params;
        Ok(())
    }

    pub(crate) fn bus_params(&self, index: usize) -> Result<BusParams> {
        self.bus_params
            .lock()
            .map_err(|_| PetalSonicError::Engine("Bus state is poisoned".into()))?
            .get(index)
            .copied()
            .ok_or(PetalSonicError::StaleBus)
    }

    pub(crate) fn runtime_status(&self) -> RuntimeStatus {
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

    pub(crate) fn diagnostics(
        &self,
        active_emitters: usize,
        desc: &PetalSonicWorldDesc,
    ) -> RuntimeDiagnostics {
        let (
            render_iterations,
            render_time_p50_us,
            render_time_p95_us,
            render_time_p99_us,
            render_time_max_us,
        ) = self.counters.render_summary();
        let acoustics = self.acoustic_propagation.diagnostics();
        RuntimeDiagnostics {
            frames_processed: self.frames_processed.load(Ordering::Relaxed),
            underrun_count: self.underrun_count.load(Ordering::Relaxed),
            active_emitters,
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
            acoustic_solve_count: acoustics.solve_count,
            acoustic_superseded_solve_count: acoustics.superseded_solve_count,
            acoustic_published_response_count: acoustics.published_response_count,
            acoustic_response_spatial_revision: acoustics.latest_spatial_revision,
            acoustic_response_geometry_version: acoustics.latest_geometry_version,
            acoustic_last_solve_time_us: acoustics.last_solve_time_us,
            acoustic_solve_time_p50_us: acoustics.solve_time_p50_us,
            acoustic_solve_time_p95_us: acoustics.solve_time_p95_us,
            acoustic_solve_time_p99_us: acoustics.solve_time_p99_us,
            acoustic_solve_time_max_us: acoustics.solve_time_max_us,
            acoustic_response_age_ms: acoustics.response_age_ms,
            acoustic_direct_ray_count: acoustics.direct_ray_count,
            acoustic_sample_cache_hit_count: acoustics.cache_hit_count,
            acoustic_processed_extent_count: acoustics.processed_extent_count,
            acoustic_lobe_count: acoustics.lobe_count,
            acoustic_retained_response_count: acoustics.retained_response_count,
            acoustic_deferred_response_count: acoustics.deferred_response_count,
            acoustic_render_rejected_response_count: self
                .counters
                .acoustic_render_rejected_responses
                .load(Ordering::Relaxed),
            device_generation: self.counters.device_generation.load(Ordering::Relaxed),
            recovery_attempts: self.recovery_attempts.load(Ordering::Relaxed),
            output_sample_rate: self.counters.output_sample_rate.load(Ordering::Relaxed) as u32,
            output_channels: self.counters.output_channels.load(Ordering::Relaxed) as u16,
            spatial_quality: desc.spatial_quality,
            latency_profile: desc.latency_profile,
        }
    }

    pub(crate) fn active_voice_count(&self) -> usize {
        self.active_voice_count.load(Ordering::Acquire)
    }

    pub(crate) fn frames_processed(&self) -> usize {
        self.frames_processed.load(Ordering::Relaxed)
    }

    pub(crate) fn underrun_count(&self) -> usize {
        self.underrun_count.load(Ordering::Relaxed)
    }

    pub(crate) fn drain_events(&self) -> Vec<PetalSonicEvent> {
        self.event_receiver.try_iter().collect()
    }

    pub(crate) fn drain_voice_telemetry(&self) -> Vec<VoiceTelemetryEvent> {
        self.voice_telemetry_receiver.try_iter().collect()
    }

    pub(crate) fn voice_telemetry_diagnostics(&self) -> VoiceTelemetryDiagnostics {
        VoiceTelemetryDiagnostics {
            queue_depth: self.voice_telemetry_receiver.len(),
            queue_high_water: self
                .counters
                .voice_telemetry_queue_high_water
                .load(Ordering::Relaxed),
            dropped_events: self
                .counters
                .dropped_voice_telemetry
                .load(Ordering::Relaxed),
        }
    }

    pub(crate) fn drain_acoustic_telemetry(&self) -> Vec<AcousticTelemetryEvent> {
        self.acoustic_telemetry_receiver.try_iter().collect()
    }

    pub(crate) fn acoustic_telemetry_diagnostics(&self) -> AcousticTelemetryDiagnostics {
        let (queue_high_water, dropped_events) = self.acoustic_propagation.telemetry_pressure();
        AcousticTelemetryDiagnostics {
            queue_depth: self.acoustic_telemetry_receiver.len(),
            queue_high_water,
            dropped_events,
        }
    }

    pub(crate) fn drain_timing_events(&self) -> Vec<RenderTimingEvent> {
        self.timing_receiver.try_iter().collect()
    }

    pub(crate) fn drain_retired_voice_ids(&self) -> Vec<VoiceId> {
        self.retirement_receiver.try_iter().collect()
    }

    fn drain_retired_spatial_frames(&self) {
        while self.spatial_retirement_receiver.try_recv().is_ok() {}
    }

    fn drain_retired_acoustic_responses(&self) {
        while self
            .acoustic_response_retirement_receiver
            .try_recv()
            .is_ok()
        {}
    }

    pub(crate) fn close(&self) -> Result<()> {
        let _close_guard = self
            .close_lock
            .lock()
            .map_err(|_| PetalSonicError::Engine("Runtime close lock is poisoned".into()))?;
        if Self::load_runtime_state(&self.runtime_state) == RuntimeState::Closed {
            return Ok(());
        }
        self.runtime_state
            .store(RuntimeState::Closing as u8, Ordering::Release);
        self.supervisor_stop.store(true, Ordering::Release);
        let shutdown_error = match self.supervisor_thread.lock() {
            Ok(mut owner) => owner.take().and_then(|supervisor| {
                supervisor.thread().unpark();
                supervisor.join().err().map(|_| {
                    PetalSonicError::Engine("Output supervisor panicked while shutting down".into())
                })
            }),
            Err(_) => Some(PetalSonicError::Engine(
                "Output supervisor lock is poisoned".into(),
            )),
        };
        self.acoustic_propagation.close();
        self.drain_retired_spatial_frames();
        self.drain_retired_acoustic_responses();
        self.active_voice_count.store(0, Ordering::Release);
        self.runtime_state
            .store(RuntimeState::Closed as u8, Ordering::Release);
        shutdown_error.map_or(Ok(()), Err)
    }

    pub(crate) fn supervisor_tick<D: OutputRuntimeDriver>(
        driver: &mut D,
        runtime_state: &AtomicU8,
        recovery_attempts: &AtomicU64,
        schedule: &mut SupervisorSchedule,
        now: Instant,
    ) {
        driver.drain_retired_resources();
        let state = Self::load_runtime_state(runtime_state);
        let probe = state == RuntimeState::Running && now >= schedule.next_health_probe;
        if state == RuntimeState::Running && !probe {
            return;
        }
        if !matches!(state, RuntimeState::Running | RuntimeState::Recovering) {
            return;
        }

        if state == RuntimeState::Running {
            schedule.next_health_probe = now + OUTPUT_RETRY_INTERVAL;
        }
        let elapsed = if state == RuntimeState::Recovering {
            now.saturating_duration_since(schedule.last_advance)
        } else {
            Duration::ZERO
        };
        schedule.last_advance = now;
        let retry_now = state == RuntimeState::Running || now >= schedule.next_retry;
        let result = driver.reconcile_output(OutputRecoveryRequest {
            probe,
            retry_now,
            elapsed_without_output: elapsed,
        });

        if retry_now && !matches!(result, OutputRecoveryResult::Stable) {
            recovery_attempts.fetch_add(1, Ordering::Relaxed);
        }
        match result {
            OutputRecoveryResult::Stable => {}
            OutputRecoveryResult::Running(_device) => {
                if state == RuntimeState::Running {
                    driver.emit_runtime_state(RuntimeState::Recovering);
                }
                runtime_state.store(RuntimeState::Running as u8, Ordering::Release);
                schedule.next_health_probe = now + OUTPUT_RETRY_INTERVAL;
                driver.emit_runtime_state(RuntimeState::Running);
            }
            OutputRecoveryResult::Recovering(_cause) => {
                if state == RuntimeState::Running {
                    runtime_state.store(RuntimeState::Recovering as u8, Ordering::Release);
                    driver.emit_runtime_state(RuntimeState::Recovering);
                }
                if retry_now {
                    schedule.next_retry = now + OUTPUT_RETRY_INTERVAL;
                }
            }
            OutputRecoveryResult::Failed(_failure) => {
                if state == RuntimeState::Running {
                    driver.emit_runtime_state(RuntimeState::Recovering);
                }
                runtime_state.store(RuntimeState::Failed as u8, Ordering::Release);
                driver.emit_runtime_state(RuntimeState::Failed);
            }
        }
    }

    fn spawn_output_supervisor(
        startup: EngineStartup,
        output: Box<dyn FnOnce() -> Result<Box<dyn OutputPlatform>> + Send>,
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
                let output = match output() {
                    Ok(output) => output,
                    Err(error) => {
                        runtime_state.store(RuntimeState::Failed as u8, Ordering::Release);
                        let _ = startup_sender.send(Err(error));
                        return;
                    }
                };
                let initial_buses = bus_params
                    .lock()
                    .map(|buses| buses.clone())
                    .unwrap_or_else(|_| vec![BusParams::default()]);
                let mut engine = match PetalSonicEngine::new_with_output(
                    startup,
                    output,
                    command_receivers.clone(),
                    initial_buses,
                ) {
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
                engine.emit_runtime_state(RuntimeState::Recovering);

                while !stop.load(Ordering::Acquire) {
                    Self::supervisor_tick(
                        &mut engine,
                        &runtime_state,
                        &recovery_attempts,
                        &mut schedule,
                        Instant::now(),
                    );
                    std::thread::park_timeout(poll_interval);
                }
                let _ = engine.stop();
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

    pub(crate) fn load_runtime_state(state: &AtomicU8) -> RuntimeState {
        match state.load(Ordering::Acquire) {
            value if value == RuntimeState::Running as u8 => RuntimeState::Running,
            value if value == RuntimeState::Recovering as u8 => RuntimeState::Recovering,
            value if value == RuntimeState::Failed as u8 => RuntimeState::Failed,
            value if value == RuntimeState::Closing as u8 => RuntimeState::Closing,
            _ => RuntimeState::Closed,
        }
    }
}

impl Drop for AudioRuntime {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::output::{
        OutputDeviceState, OutputRecoveryCause, OutputRecoveryRequest, OutputRecoveryResult,
    };
    use std::sync::atomic::{AtomicU8, AtomicU64};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct FakeDevice {
        name: &'static str,
        sample_rate: u32,
        channels: u16,
    }

    struct SupervisorFakeDriver {
        active: Option<FakeDevice>,
        selected: Option<FakeDevice>,
        stream_failed: bool,
        prepared: Option<FakeDevice>,
        advanced: Duration,
        loop_cursor_frames: usize,
        loop_length_frames: usize,
        one_shot_remaining_frames: usize,
        one_shot_completed: bool,
    }

    impl SupervisorFakeDriver {
        fn with_active(active: FakeDevice) -> Self {
            Self {
                active: Some(active),
                selected: Some(active),
                stream_failed: false,
                prepared: None,
                advanced: Duration::ZERO,
                loop_cursor_frames: 0,
                loop_length_frames: 12_000,
                one_shot_remaining_frames: 4_800,
                one_shot_completed: false,
            }
        }
    }

    impl OutputRuntimeDriver for SupervisorFakeDriver {
        fn drain_retired_resources(&mut self) {}

        fn reconcile_output(&mut self, request: OutputRecoveryRequest) -> OutputRecoveryResult {
            if self.active.is_some() && request.probe {
                if !self.stream_failed && self.active == self.selected {
                    return OutputRecoveryResult::Stable;
                }
                if !self.stream_failed {
                    let Some(selected) = self.selected else {
                        return OutputRecoveryResult::Stable;
                    };
                    self.prepared = Some(selected);
                }
                self.active = None;
            }
            self.advanced += request.elapsed_without_output;
            let frames = (request.elapsed_without_output.as_secs_f64() * 48_000.0).floor() as usize;
            self.loop_cursor_frames = (self.loop_cursor_frames + frames) % self.loop_length_frames;
            if frames >= self.one_shot_remaining_frames {
                self.one_shot_remaining_frames = 0;
                self.one_shot_completed = true;
            } else {
                self.one_shot_remaining_frames -= frames;
            }
            if !request.retry_now {
                return OutputRecoveryResult::Recovering(OutputRecoveryCause::DeviceUnavailable);
            }
            let Some(selected) = self.prepared.take().or(self.selected) else {
                return OutputRecoveryResult::Recovering(OutputRecoveryCause::DeviceUnavailable);
            };
            self.active = Some(selected);
            self.stream_failed = false;
            OutputRecoveryResult::Running(OutputDeviceState {
                diagnostic_name: selected.name.to_string(),
                sample_rate: selected.sample_rate,
                physical_channels: selected.channels,
            })
        }

        fn emit_runtime_state(&self, _state: RuntimeState) {}
    }

    #[test]
    fn supervisor_publishes_running_after_default_device_recovery() {
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
        let mut driver = SupervisorFakeDriver::with_active(a);
        driver.selected = Some(b);
        let runtime_state = AtomicU8::new(RuntimeState::Running as u8);
        let recovery_attempts = AtomicU64::new(0);
        let now = Instant::now();
        let mut schedule = SupervisorSchedule::new(now);

        AudioRuntime::supervisor_tick(
            &mut driver,
            &runtime_state,
            &recovery_attempts,
            &mut schedule,
            now,
        );

        assert_eq!(driver.active, Some(b));
        assert_eq!(
            AudioRuntime::load_runtime_state(&runtime_state),
            RuntimeState::Running
        );
        assert_eq!(recovery_attempts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn recovery_accumulates_elapsed_time_and_advances_voice_timelines() {
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
        let mut driver = SupervisorFakeDriver::with_active(a);
        driver.stream_failed = true;
        driver.selected = None;
        let runtime_state = AtomicU8::new(RuntimeState::Running as u8);
        let recovery_attempts = AtomicU64::new(0);
        let now = Instant::now();
        let mut schedule = SupervisorSchedule::new(now);

        AudioRuntime::supervisor_tick(
            &mut driver,
            &runtime_state,
            &recovery_attempts,
            &mut schedule,
            now,
        );
        assert_eq!(
            AudioRuntime::load_runtime_state(&runtime_state),
            RuntimeState::Recovering
        );
        assert_eq!(recovery_attempts.load(Ordering::Relaxed), 1);

        driver.selected = Some(b);
        AudioRuntime::supervisor_tick(
            &mut driver,
            &runtime_state,
            &recovery_attempts,
            &mut schedule,
            now + OUTPUT_RETRY_INTERVAL,
        );

        assert_eq!(driver.active, Some(b));
        assert_eq!(driver.advanced, OUTPUT_RETRY_INTERVAL);
        assert_eq!(driver.loop_cursor_frames, 0);
        assert!(driver.one_shot_completed);
        assert_eq!(
            AudioRuntime::load_runtime_state(&runtime_state),
            RuntimeState::Running
        );
        assert_eq!(recovery_attempts.load(Ordering::Relaxed), 2);
    }
}
