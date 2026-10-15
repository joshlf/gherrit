use std::{
    any::Any,
    fmt, panic,
    path::PathBuf,
    sync::{
        mpsc::{self, Receiver, RecvTimeoutError},
        Arc, RwLock,
    },
    thread,
    time::Duration,
};

use tempfile::TempDir;

use super::{mock_server, TestEnvironment};
const MOCK_SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const MOCK_SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

pub struct MockServerInfo {
    pub url: String,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    completion_rx: Option<Receiver<thread::Result<()>>>,
}

pub(super) enum MockServerStopError {
    Panicked(Box<dyn Any + Send + 'static>),
    TimedOut(Duration),
    Disconnected,
}

impl fmt::Display for MockServerStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Panicked(_) => formatter.write_str("mock server thread panicked"),
            Self::TimedOut(timeout) => {
                write!(formatter, "mock server did not stop within {timeout:?}")
            }
            Self::Disconnected => {
                formatter.write_str("mock server thread exited without reporting its result")
            }
        }
    }
}

impl MockServerInfo {
    pub(super) fn start(
        state: Arc<RwLock<mock_server::MockState>>,
        remote_path: PathBuf,
        system_git: PathBuf,
        test_environment: TestEnvironment,
        fixture_dir: Arc<TempDir>,
    ) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let (completion_tx, completion_rx) = mpsc::channel();

        thread::Builder::new()
            .name("gherrit-mock-server".to_string())
            .spawn(move || {
                let result = panic::catch_unwind(panic::AssertUnwindSafe(move || {
                    // Keep the fixture directory alive if this thread must be
                    // detached after a pathological shutdown timeout.
                    let _fixture_dir = fixture_dir;
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to build mock server runtime");
                    runtime.block_on(mock_server::run_mock_server(
                        state,
                        remote_path,
                        system_git,
                        test_environment,
                        ready_tx,
                        shutdown_rx,
                    ));
                }));
                let _ = completion_tx.send(result);
            })
            .expect("Failed to spawn mock server thread");

        let url = match ready_rx.recv_timeout(MOCK_SERVER_STARTUP_TIMEOUT) {
            Ok(url) => url,
            Err(startup_error) => match Self::stop(Some(shutdown_tx), &completion_rx) {
                Ok(()) => panic!(
                    "Mock server did not become ready within \
                     {MOCK_SERVER_STARTUP_TIMEOUT:?}: {startup_error}"
                ),
                Err(MockServerStopError::Panicked(server_panic)) => {
                    panic::resume_unwind(server_panic)
                }
                Err(stop_error) => panic!(
                    "Mock server did not become ready within \
                     {MOCK_SERVER_STARTUP_TIMEOUT:?}: {startup_error}; {stop_error}"
                ),
            },
        };

        Self { url, shutdown_tx: Some(shutdown_tx), completion_rx: Some(completion_rx) }
    }

    pub(super) fn stop(
        shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
        completion_rx: &Receiver<thread::Result<()>>,
    ) -> Result<(), MockServerStopError> {
        if let Some(shutdown_tx) = shutdown_tx {
            let _ = shutdown_tx.send(());
        }

        wait_for_server_completion(completion_rx, MOCK_SERVER_SHUTDOWN_TIMEOUT)
    }
}

pub(super) fn wait_for_server_completion(
    completion_rx: &Receiver<thread::Result<()>>,
    timeout: Duration,
) -> Result<(), MockServerStopError> {
    match completion_rx.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(server_panic)) => Err(MockServerStopError::Panicked(server_panic)),
        Err(RecvTimeoutError::Timeout) => Err(MockServerStopError::TimedOut(timeout)),
        Err(RecvTimeoutError::Disconnected) => Err(MockServerStopError::Disconnected),
    }
}

impl Drop for MockServerInfo {
    fn drop(&mut self) {
        let completion_rx = self.completion_rx.take().expect("mock server completion receiver");
        if let Err(error) = Self::stop(self.shutdown_tx.take(), &completion_rx) {
            if thread::panicking() {
                eprintln!("Mock server teardown also failed: {error}");
            } else if let MockServerStopError::Panicked(server_panic) = error {
                panic::resume_unwind(server_panic);
            } else {
                panic!("Mock server teardown failed: {error}");
            }
        }
    }
}
