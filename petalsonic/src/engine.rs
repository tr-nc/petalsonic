use crate::acoustic_propagation::{AcousticResponse, AcousticVoiceInput};
use crate::config::PetalSonicWorldDesc;
use crate::domain::{BusParams, SpatialFrame, VoiceId};
use crate::error::Result;
use crate::events::{
    PetalSonicEvent, RenderTimingEvent, RuntimeCounters, RuntimeState, VoiceTelemetryEvent,
};
use crate::output_session::OutputSession;
use crate::platform::output::{OutputPlatform, OutputRecoveryRequest, OutputRecoveryResult};
use crate::playback::PlaybackCommand;
use crate::realtime_latest::RealtimeConsumer;
use crate::render::RenderQuantum;
use crate::spatial::RetiredSpatialSource;
use crossbeam_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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
    pub spatial_frames: RealtimeConsumer<SpatialFrame>,
    pub acoustic_responses: RealtimeConsumer<AcousticResponse>,
    pub acoustic_voice_input: AcousticVoiceInput,
    pub acoustic_scene_version: Arc<std::sync::atomic::AtomicU64>,
    pub environmental_acoustics_enabled: Arc<AtomicBool>,
    pub ports: EngineRuntimePorts,
}

/// Schedules logical stereo rendering and delegates physical output lifecycle.
pub(crate) struct PetalSonicEngine {
    output: OutputSession,
    event_sender: Sender<PetalSonicEvent>,
    counters: Arc<RuntimeCounters>,
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
        let render = Arc::new(Mutex::new(RenderQuantum::new(
            startup,
            command_receivers,
            buses,
            retirement_sender,
        )?));
        Ok(Self {
            output: OutputSession::new(
                desc,
                output,
                render,
                frames_processed,
                underrun_count,
                current_device_name,
                counters.clone(),
            ),
            event_sender,
            counters,
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

    pub(crate) fn drain_retired_backend_resources(&mut self) {
        while self.backend_retirement_receiver.try_recv().is_ok() {}
    }

    pub(crate) fn reconcile_output(
        &mut self,
        request: OutputRecoveryRequest,
    ) -> OutputRecoveryResult {
        self.output.reconcile(request)
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        self.output.close()?;
        self.drain_retired_backend_resources();
        Ok(())
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
        let _ = self.close();
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
