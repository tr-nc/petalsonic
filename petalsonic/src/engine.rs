use crate::acoustic_propagation::{AcousticResponse, AcousticVoiceInput};
use crate::config::PetalSonicWorldDesc;
use crate::domain::{BusParams, SpatialFrame, VoiceId};
use crate::error::{PetalSonicError, Result};
use crate::events::{
    PetalSonicEvent, RenderTimingEvent, RuntimeCounters, RuntimeState, VoiceTelemetryEvent,
};
use crate::platform::output::{
    OutputCallback, OutputDeviceState, OutputFailure, OutputPlatform, OutputPreparation,
    OutputRecoveryCause, OutputRecoveryReason, OutputRecoveryRequest, OutputRecoveryResult,
    PreparedOutput,
};
use crate::playback::PlaybackCommand;
use crate::render::RenderQuantum;
use crate::spatial::RetiredSpatialSource;
use crossbeam_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

pub(crate) const MASTER_HEADROOM_DB: f32 = -6.0;

#[derive(Clone)]
pub(crate) struct EngineCommandReceivers {
    pub(crate) regular: Receiver<PlaybackCommand>,
    pub(crate) lifecycle: Receiver<PlaybackCommand>,
}

impl EngineCommandReceivers {
    pub(crate) fn new(
        regular: Receiver<PlaybackCommand>,
        lifecycle: Receiver<PlaybackCommand>,
    ) -> Self {
        Self { regular, lifecycle }
    }
}

pub(crate) struct EngineObservability {
    pub frames_processed: Arc<AtomicUsize>,
    pub underrun_count: Arc<AtomicUsize>,
    pub active_device_name: Arc<Mutex<Option<String>>>,
    pub event_receiver: Receiver<PetalSonicEvent>,
    pub voice_telemetry_receiver: Receiver<VoiceTelemetryEvent>,
    pub timing_receiver: Receiver<RenderTimingEvent>,
    pub counters: Arc<RuntimeCounters>,
}

pub(crate) struct EngineRuntimePorts {
    pub(crate) frames_processed: Arc<AtomicUsize>,
    pub(crate) underrun_count: Arc<AtomicUsize>,
    pub(crate) active_device_name: Arc<Mutex<Option<String>>>,
    pub(crate) event_sender: Sender<PetalSonicEvent>,
    pub(crate) voice_telemetry_sender: Sender<VoiceTelemetryEvent>,
    pub(crate) timing_sender: Sender<RenderTimingEvent>,
    pub(crate) counters: Arc<RuntimeCounters>,
}

pub(crate) struct EngineStartup {
    pub desc: PetalSonicWorldDesc,
    pub active_voice_count: Arc<AtomicUsize>,
    pub retirement_sender: Sender<VoiceId>,
    pub latest_spatial_frame: Arc<Mutex<Option<Arc<SpatialFrame>>>>,
    pub spatial_retirement_sender: Sender<Arc<SpatialFrame>>,
    pub latest_acoustic_response: Arc<Mutex<Option<Arc<AcousticResponse>>>>,
    pub acoustic_response_retirement_sender: Sender<Arc<AcousticResponse>>,
    pub acoustic_voice_input: AcousticVoiceInput,
    pub acoustic_scene_version: Arc<std::sync::atomic::AtomicU64>,
    pub environmental_acoustics_enabled: Arc<AtomicBool>,
    pub ports: EngineRuntimePorts,
}

/// Schedules logical stereo rendering and delegates physical output lifecycle.
pub(crate) struct PetalSonicEngine {
    desc: PetalSonicWorldDesc,
    output: Box<dyn OutputPlatform>,
    prepared_output: Option<PreparedOutput>,
    active_output: Option<OutputDeviceState>,
    is_running: Arc<AtomicBool>,
    frames_processed: Arc<AtomicUsize>,
    underrun_count: Arc<AtomicUsize>,
    current_device_name: Arc<Mutex<Option<String>>>,
    event_sender: Sender<PetalSonicEvent>,
    counters: Arc<RuntimeCounters>,
    render: Arc<Mutex<RenderQuantum>>,
    render_thread: Option<JoinHandle<()>>,
    backend_retirement_receiver: Receiver<RetiredSpatialSource>,
}

impl PetalSonicEngine {
    pub(crate) fn new_with_output(
        startup: EngineStartup,
        output: Box<dyn OutputPlatform>,
        command_receivers: EngineCommandReceivers,
        buses: Vec<BusParams>,
    ) -> Result<Self> {
        let desc = startup.desc.clone();
        let frames_processed = startup.ports.frames_processed.clone();
        let underrun_count = startup.ports.underrun_count.clone();
        let current_device_name = startup.ports.active_device_name.clone();
        let event_sender = startup.ports.event_sender.clone();
        let counters = startup.ports.counters.clone();
        let (retirement_sender, retirement_receiver) = crossbeam_channel::bounded(desc.max_voices);
        let render = RenderQuantum::new(startup, command_receivers, buses, retirement_sender)?;
        Ok(Self {
            desc,
            output,
            prepared_output: None,
            active_output: None,
            is_running: Arc::new(AtomicBool::new(false)),
            frames_processed,
            underrun_count,
            current_device_name,
            event_sender,
            counters,
            render: Arc::new(Mutex::new(render)),
            render_thread: None,
            backend_retirement_receiver: retirement_receiver,
        })
    }

    pub(crate) fn create_runtime_ports(
        desc: &PetalSonicWorldDesc,
    ) -> (EngineRuntimePorts, EngineObservability) {
        let frames_processed = Arc::new(AtomicUsize::new(0));
        let underrun_count = Arc::new(AtomicUsize::new(0));
        let active_device_name = Arc::new(Mutex::new(None));
        let counters = Arc::new(RuntimeCounters::default());
        let (event_sender, event_receiver) = crossbeam_channel::bounded(desc.event_queue_capacity);
        let (voice_telemetry_sender, voice_telemetry_receiver) =
            crossbeam_channel::bounded(desc.event_queue_capacity);
        let (timing_sender, timing_receiver) =
            crossbeam_channel::bounded(desc.timing_queue_capacity);
        (
            EngineRuntimePorts {
                frames_processed: frames_processed.clone(),
                underrun_count: underrun_count.clone(),
                active_device_name: active_device_name.clone(),
                event_sender,
                voice_telemetry_sender,
                timing_sender,
                counters: counters.clone(),
            },
            EngineObservability {
                frames_processed,
                underrun_count,
                active_device_name,
                event_receiver,
                voice_telemetry_receiver,
                timing_receiver,
                counters,
            },
        )
    }

    pub(crate) fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Relaxed)
    }

    pub(crate) fn start(&mut self) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }
        let prepared = match self.prepared_output.take() {
            Some(prepared) => prepared,
            None => match self.output.prepare(&self.desc.output_device) {
                OutputPreparation::Ready(prepared) => prepared,
                OutputPreparation::Unavailable | OutputPreparation::RequiresStop => {
                    return Err(PetalSonicError::AudioDevice(
                        "No selected output device is currently available".into(),
                    ));
                }
                OutputPreparation::Failed(_) => {
                    return Err(PetalSonicError::PermanentDeviceFailure(
                        "Unsupported sample format".into(),
                    ));
                }
            },
        };
        log::info!(
            "PetalSonic master headroom: {} dB (linear gain {:.3})",
            MASTER_HEADROOM_DB,
            crate::gain::db_to_linear(MASTER_HEADROOM_DB)
        );
        let consumer = self
            .render
            .lock()
            .map_err(|_| PetalSonicError::Engine("Render state is poisoned".into()))?
            .connect_output(prepared.device.sample_rate)?;
        self.is_running.store(true, Ordering::Release);
        if let Ok(mut render) = self.render.lock() {
            render.render();
            render.render();
        }
        let callback = OutputCallback::new(
            self.is_running.clone(),
            self.frames_processed.clone(),
            self.underrun_count.clone(),
            consumer,
            prepared.device.sample_rate,
        );
        let active = match self.output.open(prepared, callback) {
            Ok(active) => active,
            Err(error) => {
                self.is_running.store(false, Ordering::Release);
                if let Ok(mut render) = self.render.lock() {
                    render.disconnect_output();
                }
                return Err(error);
            }
        };
        let thread = match Self::spawn_render_thread(
            self.render.clone(),
            self.is_running.clone(),
            self.desc.block_size,
            self.desc.sample_rate,
        ) {
            Ok(thread) => thread,
            Err(error) => {
                self.is_running.store(false, Ordering::Release);
                let _ = self.output.stop();
                if let Ok(mut render) = self.render.lock() {
                    render.disconnect_output();
                }
                return Err(error);
            }
        };
        if let Ok(mut current) = self.current_device_name.lock() {
            *current = Some(active.diagnostic_name.clone());
        }
        self.counters
            .output_sample_rate
            .store(active.sample_rate as usize, Ordering::Relaxed);
        self.counters
            .output_channels
            .store(active.physical_channels as usize, Ordering::Relaxed);
        self.counters
            .device_generation
            .fetch_add(1, Ordering::Relaxed);
        self.active_output = Some(active);
        self.render_thread = Some(thread);
        Ok(())
    }

    fn spawn_render_thread(
        render: Arc<Mutex<RenderQuantum>>,
        is_running: Arc<AtomicBool>,
        block_size: usize,
        sample_rate: u32,
    ) -> Result<JoinHandle<()>> {
        let wake_interval = render
            .lock()
            .map_err(|_| PetalSonicError::Engine("Render state is poisoned".into()))?
            .schedule()
            .wake_interval(block_size, sample_rate);
        std::thread::Builder::new()
            .name("petalsonic-render".into())
            .spawn(move || {
                while is_running.load(Ordering::Acquire) {
                    if let Ok(mut render) = render.lock() {
                        render.render();
                    }
                    std::thread::park_timeout(wake_interval);
                }
            })
            .map_err(|error| {
                PetalSonicError::Engine(format!("Failed to start render thread: {error}"))
            })
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.is_running.store(false, Ordering::Release);
        if let Some(thread) = self.render_thread.take() {
            thread.thread().unpark();
            thread.join().map_err(|_| {
                PetalSonicError::Engine("Render thread panicked while shutting down".into())
            })?;
        }
        self.output.stop()?;
        if let Ok(mut render) = self.render.lock() {
            render.disconnect_output();
        }
        self.active_output = None;
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
        let Some(active) = &self.active_output else {
            return Some(OutputRecoveryReason::StreamFailure);
        };
        self.output
            .recovery_reason(&self.desc.output_device, active)
    }

    pub(crate) fn prepare_selected_output(&mut self) -> OutputPreparation {
        let preparation = self.output.prepare(&self.desc.output_device);
        if let OutputPreparation::Ready(prepared) = &preparation {
            self.prepared_output = Some(prepared.clone());
        }
        preparation
    }

    pub(crate) fn reconcile_output(
        &mut self,
        request: OutputRecoveryRequest,
    ) -> OutputRecoveryResult {
        if self.is_running() {
            if !request.probe {
                return OutputRecoveryResult::Stable;
            }
            match self.output_recovery_reason() {
                None => return OutputRecoveryResult::Stable,
                Some(OutputRecoveryReason::SelectionChanged) => {
                    if matches!(
                        self.prepare_selected_output(),
                        OutputPreparation::Unavailable
                    ) {
                        return OutputRecoveryResult::Stable;
                    }
                }
                Some(OutputRecoveryReason::StreamFailure) => {}
            }
            if self.stop().is_err() {
                return OutputRecoveryResult::Failed(OutputFailure::PlatformLifecycle);
            }
        }
        if let Ok(mut render) = self.render.lock() {
            render.advance_without_output(request.elapsed_without_output);
        }
        if !request.retry_now {
            return OutputRecoveryResult::Recovering(OutputRecoveryCause::DeviceUnavailable);
        }
        match self.start() {
            Ok(()) => OutputRecoveryResult::Running(
                self.active_output
                    .clone()
                    .expect("successful output start publishes typed device state"),
            ),
            Err(
                PetalSonicError::AudioFormat(_)
                | PetalSonicError::PermanentDeviceFailure(_)
                | PetalSonicError::BackendUnavailable { .. },
            ) => OutputRecoveryResult::Failed(OutputFailure::UnsupportedSampleFormat),
            Err(_) => OutputRecoveryResult::Recovering(OutputRecoveryCause::DeviceUnavailable),
        }
    }

    pub(crate) fn emit_runtime_state(&self, state: RuntimeState) {
        if self
            .event_sender
            .try_send(PetalSonicEvent::RuntimeStateChanged(state))
            .is_ok()
        {
            RuntimeCounters::observe_high_water(
                &self.counters.event_queue_high_water,
                self.event_sender.len(),
            );
        } else {
            self.counters.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for PetalSonicEngine {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
pub(crate) mod tests {
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
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            PROBE_ACTIVE.with(|active| {
                if active.get() {
                    PROBE_ACTIVITY.with(|count| count.set(count.get() + 1));
                }
            });
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            PROBE_ACTIVE.with(|active| {
                if active.get() {
                    PROBE_ACTIVITY.with(|count| count.set(count.get() + 1));
                }
            });
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    pub(crate) fn callback_memory_activity(operation: impl FnOnce()) -> usize {
        PROBE_ACTIVITY.with(|count| count.set(0));
        PROBE_ACTIVE.with(|active| active.set(true));
        operation();
        PROBE_ACTIVE.with(|active| active.set(false));
        PROBE_ACTIVITY.with(Cell::get)
    }
}
