//! Closed physical-output and render-thread lifecycle.

use crate::config::{OutputDevicePolicy, PetalSonicWorldDesc};
use crate::engine::MASTER_HEADROOM_DB;
use crate::error::{PetalSonicError, Result};
use crate::events::RuntimeCounters;
use crate::platform::output::{
    OutputCallback, OutputDeviceState, OutputFailure, OutputPlatform, OutputPreparation,
    OutputRecoveryCause, OutputRecoveryReason, OutputRecoveryRequest, OutputRecoveryResult,
    PreparedOutput,
};
use crate::render::RenderQuantum;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

enum SessionState {
    Stopped,
    Prepared(PreparedOutput),
    Running {
        device: OutputDeviceState,
        signal: Arc<AtomicBool>,
        render_thread: JoinHandle<()>,
    },
}

/// Owns every physical-output lifecycle fact behind `reconcile` and `close`.
///
/// Device preparation, the logical-render connection, callback stream, render
/// thread, rollback ordering, physical-layout observation, and recovery all
/// transition through one closed state. Callers cannot partially prepare,
/// start, or stop an output.
pub(crate) struct OutputSession {
    output_device: OutputDevicePolicy,
    block_size: usize,
    sample_rate: u32,
    platform: Box<dyn OutputPlatform>,
    render: Arc<Mutex<RenderQuantum>>,
    frames_processed: Arc<AtomicUsize>,
    underrun_count: Arc<AtomicUsize>,
    current_device_name: Arc<Mutex<Option<String>>>,
    counters: Arc<RuntimeCounters>,
    state: SessionState,
}

impl OutputSession {
    pub(crate) fn new(
        desc: PetalSonicWorldDesc,
        platform: Box<dyn OutputPlatform>,
        render: Arc<Mutex<RenderQuantum>>,
        frames_processed: Arc<AtomicUsize>,
        underrun_count: Arc<AtomicUsize>,
        current_device_name: Arc<Mutex<Option<String>>>,
        counters: Arc<RuntimeCounters>,
    ) -> Self {
        Self {
            output_device: desc.output_device,
            block_size: desc.block_size,
            sample_rate: desc.sample_rate,
            platform,
            render,
            frames_processed,
            underrun_count,
            current_device_name,
            counters,
            state: SessionState::Stopped,
        }
    }

    pub(crate) fn reconcile(&mut self, request: OutputRecoveryRequest) -> OutputRecoveryResult {
        if let SessionState::Running { device, .. } = &self.state {
            if !request.probe {
                return OutputRecoveryResult::Stable;
            }
            match self.platform.recovery_reason(&self.output_device, device) {
                None => return OutputRecoveryResult::Stable,
                Some(OutputRecoveryReason::SelectionChanged) => {
                    match self.platform.prepare(&self.output_device) {
                        OutputPreparation::Ready(prepared) => {
                            if self.stop_running().is_err() {
                                return OutputRecoveryResult::Failed(
                                    OutputFailure::PlatformLifecycle,
                                );
                            }
                            self.state = SessionState::Prepared(prepared);
                        }
                        OutputPreparation::Unavailable => {
                            return OutputRecoveryResult::Stable;
                        }
                        OutputPreparation::RequiresStop => {
                            if self.stop_running().is_err() {
                                return OutputRecoveryResult::Failed(
                                    OutputFailure::PlatformLifecycle,
                                );
                            }
                        }
                        OutputPreparation::Failed(failure) => {
                            let _ = self.stop_running();
                            return OutputRecoveryResult::Failed(failure);
                        }
                    }
                }
                Some(OutputRecoveryReason::StreamFailure) => {
                    if self.stop_running().is_err() {
                        return OutputRecoveryResult::Failed(OutputFailure::PlatformLifecycle);
                    }
                }
            }
        }

        if let Ok(mut render) = self.render.lock() {
            render.advance_without_output(request.elapsed_without_output);
        }
        if !request.retry_now {
            return OutputRecoveryResult::Recovering(OutputRecoveryCause::DeviceUnavailable);
        }
        match self.start_prepared() {
            Ok(device) => OutputRecoveryResult::Running(device),
            Err(
                PetalSonicError::AudioFormat(_)
                | PetalSonicError::PermanentDeviceFailure(_)
                | PetalSonicError::BackendUnavailable { .. },
            ) => OutputRecoveryResult::Failed(OutputFailure::UnsupportedSampleFormat),
            Err(_) => OutputRecoveryResult::Recovering(OutputRecoveryCause::DeviceUnavailable),
        }
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        match self.state {
            SessionState::Running { .. } => self.stop_running(),
            SessionState::Prepared(_) => {
                self.state = SessionState::Stopped;
                self.platform.stop()
            }
            SessionState::Stopped => Ok(()),
        }
    }

    fn start_prepared(&mut self) -> Result<OutputDeviceState> {
        let prepared = match std::mem::replace(&mut self.state, SessionState::Stopped) {
            SessionState::Prepared(prepared) => prepared,
            SessionState::Stopped => match self.platform.prepare(&self.output_device) {
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
            running @ SessionState::Running { .. } => {
                self.state = running;
                return self.active_device().ok_or_else(|| {
                    PetalSonicError::Engine("Running output lost its device state".into())
                });
            }
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
        let signal = Arc::new(AtomicBool::new(true));
        if let Ok(mut render) = self.render.lock() {
            render.render();
            render.render();
        }
        let callback = OutputCallback::new(
            signal.clone(),
            self.frames_processed.clone(),
            self.underrun_count.clone(),
            consumer,
            prepared.device.sample_rate,
        );
        let active = match self.platform.open(prepared, callback) {
            Ok(active) => active,
            Err(error) => {
                signal.store(false, Ordering::Release);
                let _ = self.platform.stop();
                self.disconnect_render();
                return Err(error);
            }
        };
        let render_thread = match Self::spawn_render_thread(
            self.render.clone(),
            signal.clone(),
            self.block_size,
            self.sample_rate,
        ) {
            Ok(thread) => thread,
            Err(error) => {
                signal.store(false, Ordering::Release);
                let _ = self.platform.stop();
                self.disconnect_render();
                return Err(error);
            }
        };
        self.publish_running_device(&active);
        self.state = SessionState::Running {
            device: active.clone(),
            signal,
            render_thread,
        };
        Ok(active)
    }

    fn stop_running(&mut self) -> Result<()> {
        let SessionState::Running {
            signal,
            render_thread,
            ..
        } = std::mem::replace(&mut self.state, SessionState::Stopped)
        else {
            return Ok(());
        };
        signal.store(false, Ordering::Release);
        render_thread.thread().unpark();
        let thread_result = render_thread.join().map_err(|_| {
            PetalSonicError::Engine("Render thread panicked while shutting down".into())
        });
        let platform_result = self.platform.stop();
        self.disconnect_render();
        if let Ok(mut current) = self.current_device_name.lock() {
            *current = None;
        }
        thread_result.and(platform_result)
    }

    fn disconnect_render(&self) {
        if let Ok(mut render) = self.render.lock() {
            render.disconnect_output();
        }
    }

    fn active_device(&self) -> Option<OutputDeviceState> {
        match &self.state {
            SessionState::Running { device, .. } => Some(device.clone()),
            SessionState::Stopped | SessionState::Prepared(_) => None,
        }
    }

    fn publish_running_device(&self, active: &OutputDeviceState) {
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
    }

    fn spawn_render_thread(
        render: Arc<Mutex<RenderQuantum>>,
        signal: Arc<AtomicBool>,
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
                while signal.load(Ordering::Acquire) {
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
}

impl Drop for OutputSession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acoustic_propagation::AcousticVoiceInput;
    use crate::domain::BusParams;
    use crate::engine::{EngineCommandReceivers, EngineStartup, PetalSonicEngine};
    use crate::platform::output::fake::{FakeDevice, FakeOutputHandle, FakeOutputPlatform};
    use crate::realtime_latest::RealtimeLatest;
    use crate::render::RenderQuantum;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn session_fixture(
        devices: Vec<FakeDevice>,
        selected: usize,
    ) -> (OutputSession, FakeOutputHandle) {
        let desc = PetalSonicWorldDesc::default();
        let (platform, handle) = FakeOutputPlatform::scripted(devices, Some(selected));
        let (regular_sender, regular) = crossbeam_channel::bounded(desc.control_queue_capacity);
        let (lifecycle_sender, lifecycle) =
            crossbeam_channel::bounded(desc.lifecycle_queue_capacity);
        let (retirement_sender, _) = crossbeam_channel::bounded(desc.max_voices);
        let (backend_retirement_sender, _) = crossbeam_channel::bounded(desc.max_voices);
        let (_spatial_publisher, spatial_frames) = RealtimeLatest::bounded(1);
        let (_acoustic_publisher, acoustic_responses) = RealtimeLatest::bounded(2);
        let (ports, observability) = PetalSonicEngine::create_runtime_ports(&desc);
        let startup = EngineStartup {
            desc: desc.clone(),
            active_voice_count: Arc::new(AtomicUsize::new(0)),
            retirement_sender,
            spatial_frames,
            acoustic_responses,
            acoustic_voice_input: AcousticVoiceInput::isolated(desc.max_voices),
            acoustic_scene_version: Arc::new(AtomicU64::new(0)),
            environmental_acoustics_enabled: Arc::new(AtomicBool::new(true)),
            ports,
        };
        let render = RenderQuantum::new(
            startup,
            EngineCommandReceivers::new(regular, lifecycle),
            vec![BusParams::default()],
            backend_retirement_sender,
        )
        .unwrap();
        // The render quantum owns the receiving endpoints for the fixture's
        // lifetime; senders only keep command channels connected.
        let _command_owners = (regular_sender, lifecycle_sender);
        (
            OutputSession::new(
                desc,
                Box::new(platform),
                Arc::new(Mutex::new(render)),
                observability.frames_processed,
                observability.underrun_count,
                observability.active_device_name,
                observability.counters,
            ),
            handle,
        )
    }

    fn retry(probe: bool) -> OutputRecoveryRequest {
        OutputRecoveryRequest {
            probe,
            retry_now: true,
            elapsed_without_output: Duration::ZERO,
        }
    }

    #[test]
    fn reconcile_starts_one_complete_running_session() {
        let (mut session, handle) = session_fixture(vec![FakeDevice::stereo("A", 48_000)], 0);

        assert!(matches!(
            session.reconcile(retry(false)),
            OutputRecoveryResult::Running(ref device) if device.diagnostic_name == "A"
        ));
        assert_eq!(handle.actions(), ["prepare", "open"]);
    }

    #[test]
    fn failed_open_rolls_back_before_a_later_retry() {
        let (mut session, handle) = session_fixture(vec![FakeDevice::stereo("A", 48_000)], 0);
        handle.fail_next_open();

        assert_eq!(
            session.reconcile(retry(false)),
            OutputRecoveryResult::Recovering(OutputRecoveryCause::DeviceUnavailable)
        );
        assert!(matches!(
            session.reconcile(retry(false)),
            OutputRecoveryResult::Running(ref device) if device.diagnostic_name == "A"
        ));
        assert_eq!(
            handle.actions(),
            ["prepare", "open", "stop", "prepare", "open"]
        );
    }

    #[test]
    fn reconcile_replaces_a_running_device_as_one_transition() {
        let devices = vec![FakeDevice::stereo("A", 48_000), {
            let mut device = FakeDevice::stereo("B", 44_100);
            device.state.physical_channels = 6;
            device
        }];
        let (mut session, handle) = session_fixture(devices, 0);
        assert!(matches!(
            session.reconcile(retry(false)),
            OutputRecoveryResult::Running(_)
        ));
        handle.set_selected(Some(1));

        assert!(matches!(
            session.reconcile(retry(true)),
            OutputRecoveryResult::Running(ref device)
                if device.diagnostic_name == "B"
                    && device.sample_rate == 44_100
                    && device.physical_channels == 6
        ));
        assert_eq!(
            handle.actions(),
            ["prepare", "open", "prepare", "stop", "open"]
        );
    }

    #[test]
    fn close_is_idempotent_and_leaves_the_session_stopped() {
        let (mut session, handle) = session_fixture(vec![FakeDevice::stereo("A", 48_000)], 0);
        assert!(matches!(
            session.reconcile(retry(false)),
            OutputRecoveryResult::Running(_)
        ));

        session.close().unwrap();
        session.close().unwrap();

        assert_eq!(handle.actions(), ["prepare", "open", "stop"]);
    }
}
