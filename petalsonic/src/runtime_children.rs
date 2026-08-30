use crate::error::{PetalSonicError, Result};
use crate::events::RuntimeState;
use crossbeam_channel::Sender;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeChildKind {
    Acoustics,
    Output,
}

impl fmt::Display for RuntimeChildKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Acoustics => formatter.write_str("acoustics"),
            Self::Output => formatter.write_str("output"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ChildCancellation {
    requested: Arc<AtomicBool>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl ChildCancellation {
    pub(crate) fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            requested: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(wake),
        }
    }

    pub(crate) fn passive() -> Self {
        Self::new(|| {})
    }

    fn request(&self) {
        self.requested.store(true, Ordering::Release);
        (self.wake)();
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeChildFailure {
    detail: String,
}

impl RuntimeChildFailure {
    pub(crate) fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for RuntimeChildFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

pub(crate) type RuntimeChildResult = std::result::Result<(), RuntimeChildFailure>;

enum StartupStatus {
    Ready,
    Failed(PetalSonicError),
}

pub(crate) struct ChildStartup {
    sender: Option<Sender<StartupStatus>>,
}

impl ChildStartup {
    pub(crate) fn ready(mut self) -> RuntimeChildResult {
        self.sender
            .take()
            .expect("startup acknowledgement is consumed exactly once")
            .send(StartupStatus::Ready)
            .map_err(|_| RuntimeChildFailure::new("runtime owner dropped during startup"))
    }

    pub(crate) fn failed(mut self, error: PetalSonicError) -> RuntimeChildFailure {
        let detail = error.to_string();
        let _ = self
            .sender
            .take()
            .expect("startup acknowledgement is consumed exactly once")
            .send(StartupStatus::Failed(error));
        RuntimeChildFailure::new(detail)
    }
}

#[derive(Debug)]
enum ChildCompletion {
    Stopped,
    Failed(RuntimeChildFailure),
}

struct RuntimeChild {
    kind: RuntimeChildKind,
    cancellation: ChildCancellation,
    handle: JoinHandle<ChildCompletion>,
}

/// Owns every asynchronous child in one audio runtime.
///
/// Startup acknowledgement, cancellation, abnormal-exit classification, joining, panic
/// conversion, and the aggregate close result all stay behind this one interface. Callers never
/// receive a thread handle and child implementations never decide runtime health independently.
pub(crate) struct RuntimeChildren {
    runtime_state: Arc<AtomicU8>,
    children: Mutex<Vec<RuntimeChild>>,
}

impl RuntimeChildren {
    pub(crate) fn new(runtime_state: Arc<AtomicU8>) -> Self {
        Self {
            runtime_state,
            children: Mutex::new(Vec::with_capacity(2)),
        }
    }

    pub(crate) fn spawn(
        &mut self,
        kind: RuntimeChildKind,
        thread_name: &'static str,
        cancellation: ChildCancellation,
        run: impl FnOnce(ChildStartup, ChildCancellation) -> RuntimeChildResult + Send + 'static,
    ) -> Result<()> {
        let (startup_sender, startup_receiver) = crossbeam_channel::bounded(1);
        let task_cancellation = cancellation.clone();
        let state = self.runtime_state.clone();
        let handle = std::thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                let run_result = catch_unwind(AssertUnwindSafe(|| {
                    run(
                        ChildStartup {
                            sender: Some(startup_sender),
                        },
                        task_cancellation.clone(),
                    )
                }));
                let completion = match run_result {
                    Ok(Ok(())) if task_cancellation.is_requested() => ChildCompletion::Stopped,
                    Ok(Ok(())) => ChildCompletion::Failed(RuntimeChildFailure::new(
                        "exited without runtime cancellation",
                    )),
                    Ok(Err(failure)) => ChildCompletion::Failed(failure),
                    Err(_) => ChildCompletion::Failed(RuntimeChildFailure::new("panicked")),
                };
                if matches!(completion, ChildCompletion::Failed(_))
                    && !matches!(
                        load_runtime_state(&state),
                        RuntimeState::Closing | RuntimeState::Closed
                    )
                {
                    state.store(RuntimeState::Failed as u8, Ordering::Release);
                }
                completion
            })
            .map_err(|error| {
                PetalSonicError::Engine(format!("Failed to start {kind} child: {error}"))
            })?;

        match startup_receiver.recv() {
            Ok(StartupStatus::Ready) => {
                self.children
                    .get_mut()
                    .map_err(|_| {
                        PetalSonicError::Engine("Runtime children lock is poisoned".into())
                    })?
                    .push(RuntimeChild {
                        kind,
                        cancellation,
                        handle,
                    });
                Ok(())
            }
            Ok(StartupStatus::Failed(error)) => {
                let _ = handle.join();
                Err(error)
            }
            Err(_) => {
                let detail = match handle.join() {
                    Ok(ChildCompletion::Failed(failure)) => failure.to_string(),
                    Ok(ChildCompletion::Stopped) => "stopped during startup".into(),
                    Err(_) => "panicked during startup".into(),
                };
                Err(PetalSonicError::Engine(format!(
                    "{kind} child exited during startup: {detail}"
                )))
            }
        }
    }

    pub(crate) fn close(&self) -> Result<()> {
        let mut children = self
            .children
            .lock()
            .map_err(|_| PetalSonicError::Engine("Runtime children lock is poisoned".into()))?;
        let children = std::mem::take(&mut *children);
        for child in &children {
            child.cancellation.request();
            child.handle.thread().unpark();
        }

        let mut failures = Vec::new();
        for child in children {
            match child.handle.join() {
                Ok(ChildCompletion::Stopped) => {}
                Ok(ChildCompletion::Failed(failure)) => {
                    failures.push(format!("{}: {failure}", child.kind));
                }
                Err(_) => failures.push(format!("{}: join observed a panic", child.kind)),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(PetalSonicError::Engine(format!(
                "Runtime child shutdown failed: {}",
                failures.join("; ")
            )))
        }
    }
}

impl Drop for RuntimeChildren {
    fn drop(&mut self) {
        let _ = self.close();
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
