use std::{
    fmt,
    io::{self, Read},
    process::{Command, ExitStatus, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use command_group::{CommandGroup, GroupChild};

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// One finite execution deadline for destination-bound Git reads.
///
/// Callers pass this explicitly so every remote observation and acquisition
/// uses the same bounded process-lifecycle policy.
pub(super) const REMOTE_GIT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs one remote Git command without blocking GHerrit's Tokio runtime.
///
/// The deadline covers process execution. A fixed, bounded cleanup interval is
/// reserved after it for killing the complete process group, reaping its
/// leader, and draining both output pipes. Dropping this future requests the
/// same cleanup, so aborting an observation task cannot detach a Git process.
pub(super) async fn output(command: Command, timeout: Duration) -> Result<Output, CommandError> {
    let started = Instant::now();
    let deadline = started.checked_add(timeout).ok_or(CommandError::InvalidTimeout)?;
    let cleanup_deadline =
        deadline.checked_add(CLEANUP_TIMEOUT).ok_or(CommandError::InvalidTimeout)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut cancellation_guard = CancellationGuard::new(Arc::clone(&cancelled));

    let worker_cancelled = Arc::clone(&cancelled);
    let mut worker = tokio::task::spawn_blocking(move || {
        output_blocking(command, deadline, cleanup_deadline, &worker_cancelled)
    });

    let result =
        tokio::time::timeout_at(tokio::time::Instant::from_std(cleanup_deadline), &mut worker)
            .await;

    match result {
        Ok(Ok(result)) => {
            cancellation_guard.disarm();
            result
        }
        Ok(Err(_)) => {
            cancellation_guard.cancel();
            Err(CommandError::WorkerUnavailable)
        }
        Err(_) => {
            // `abort` prevents a queued blocking task from starting. Once a
            // blocking task has started, the cancellation flag is what makes
            // it kill and reap the process group.
            cancellation_guard.cancel();
            worker.abort();
            Err(CommandError::CleanupTimedOut)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CommandError {
    InvalidTimeout,
    TimedOut,
    CleanupTimedOut,
    WorkerUnavailable,
    Io { stage: IoStage, kind: io::ErrorKind },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IoStage {
    Start,
    Monitor,
    StartOutputReader,
    ReadOutput,
    Reap,
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTimeout => formatter.write_str("remote Git command timeout is invalid"),
            Self::TimedOut => formatter.write_str("remote Git command timed out"),
            Self::CleanupTimedOut => formatter.write_str("remote Git command cleanup timed out"),
            Self::WorkerUnavailable => {
                formatter.write_str("remote Git command worker did not complete")
            }
            Self::Io { stage, kind } => {
                write!(formatter, "remote Git command failed during {stage} ({kind:?})")
            }
        }
    }
}

impl std::error::Error for CommandError {}

impl fmt::Display for IoStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Start => "startup",
            Self::Monitor => "execution",
            Self::StartOutputReader => "output-reader startup",
            Self::ReadOutput => "output collection",
            Self::Reap => "cleanup",
        })
    }
}

struct CancellationGuard {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl CancellationGuard {
    fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self { cancelled, armed: true }
    }

    fn cancel(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
            self.armed = false;
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn output_blocking(
    mut command: Command,
    deadline: Instant,
    cleanup_deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Output, CommandError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CommandError::CleanupTimedOut);
    }
    if Instant::now() >= deadline {
        return Err(CommandError::TimedOut);
    }

    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child =
        spawn_command_group(&mut command).map_err(|error| io_error(IoStage::Start, &error))?;
    let stdout = match child.inner().stdout.take() {
        Some(stdout) => match PipeReader::start(stdout) {
            Ok(stdout) => stdout,
            Err(error) => return fail_after_spawn(child, None, cleanup_deadline, error),
        },
        None => {
            return fail_after_spawn(
                child,
                None,
                cleanup_deadline,
                CommandError::Io {
                    stage: IoStage::StartOutputReader,
                    kind: io::ErrorKind::BrokenPipe,
                },
            );
        }
    };
    let stderr = match child.inner().stderr.take() {
        Some(stderr) => match PipeReader::start(stderr) {
            Ok(stderr) => stderr,
            Err(error) => return fail_after_spawn(child, Some(stdout), cleanup_deadline, error),
        },
        None => {
            return fail_after_spawn(
                child,
                Some(stdout),
                cleanup_deadline,
                CommandError::Io {
                    stage: IoStage::StartOutputReader,
                    kind: io::ErrorKind::BrokenPipe,
                },
            );
        }
    };

    let stopped = loop {
        if cancelled.load(Ordering::Acquire) {
            break Stopped::Cancelled;
        }
        match leader_exited(&mut child) {
            Ok(true) => break Stopped::Complete,
            Ok(false) if Instant::now() < deadline => {
                thread::sleep(
                    POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(false) => break Stopped::TimedOut,
            Err(error) => break Stopped::Monitor(io_error(IoStage::Monitor, &error)),
        }
    };

    // On Unix, `leader_exited` deliberately leaves the leader waitable. Its
    // PID, and therefore the process-group ID created by command-group, cannot
    // be recycled before this signal. This lets us terminate descendants even
    // when they closed both output pipes before the leader exited. Windows has
    // an owned job-object handle rather than a numeric process-group ID.
    let _ = child.kill();
    let status = reap_leader(&mut child, cleanup_deadline);
    let stdout = stdout.finish(cleanup_deadline);
    let stderr = stderr.finish(cleanup_deadline);
    let status = status?;
    let stdout = stdout?;
    let stderr = stderr?;

    match stopped {
        Stopped::Complete => Ok(Output { status, stdout, stderr }),
        Stopped::TimedOut => Err(CommandError::TimedOut),
        // Cancellation is observable only by the task which dropped this
        // future. If an outer hard deadline still observes the worker result,
        // cleanup—not a third command outcome—is what failed.
        Stopped::Cancelled => Err(CommandError::CleanupTimedOut),
        Stopped::Monitor(error) => Err(error),
    }
}

fn fail_after_spawn(
    mut child: GroupChild,
    stdout: Option<PipeReader>,
    cleanup_deadline: Instant,
    error: CommandError,
) -> Result<Output, CommandError> {
    let _ = child.kill();
    let reap = reap_leader(&mut child, cleanup_deadline);
    let stdout = stdout.map(|stdout| stdout.finish(cleanup_deadline));
    reap?;
    if let Some(stdout) = stdout {
        stdout?;
    }
    Err(error)
}

enum Stopped {
    Complete,
    TimedOut,
    Cancelled,
    Monitor(CommandError),
}

fn spawn_command_group(command: &mut Command) -> io::Result<GroupChild> {
    #[cfg(windows)]
    {
        command.group().kill_on_drop(true).spawn()
    }
    #[cfg(not(windows))]
    {
        command.group_spawn()
    }
}

#[cfg(unix)]
fn leader_exited(child: &mut GroupChild) -> io::Result<bool> {
    // SAFETY: An all-zero `siginfo_t` is a valid output buffer. `waitid`
    // receives its exact size, writes only through this exclusive pointer, and
    // returns before `si_pid` reads it. WNOWAIT is the critical property: it
    // reports an exited leader without releasing its PID for reuse.
    let mut information = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut information,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { information.si_pid() } != 0)
}

#[cfg(windows)]
fn leader_exited(child: &mut GroupChild) -> io::Result<bool> {
    child.inner().try_wait().map(|status| status.is_some())
}

fn reap_leader(child: &mut GroupChild, deadline: Instant) -> Result<ExitStatus, CommandError> {
    loop {
        match child.inner().try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) => return Err(CommandError::CleanupTimedOut),
            Err(error) => return Err(io_error(IoStage::Reap, &error)),
        }
    }
}

struct PipeReader {
    result: Receiver<io::Result<Vec<u8>>>,
    thread: thread::JoinHandle<()>,
}

impl PipeReader {
    fn start(mut pipe: impl Read + Send + 'static) -> Result<Self, CommandError> {
        let (sender, result) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("gherrit-remote-git-output".to_owned())
            .spawn(move || {
                let mut bytes = Vec::new();
                let result = pipe.read_to_end(&mut bytes).map(|_| bytes);
                let _ = sender.send(result);
            })
            .map_err(|error| io_error(IoStage::StartOutputReader, &error))?;
        Ok(Self { result, thread })
    }

    fn finish(self, deadline: Instant) -> Result<Vec<u8>, CommandError> {
        let result = self.result.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        if matches!(&result, Err(RecvTimeoutError::Timeout)) {
            // Joining a reader which has not observed EOF would defeat the
            // cleanup deadline. The process group has already been killed;
            // this case means the operating-system lifecycle boundary itself
            // failed, so return the bounded cleanup error.
            return Err(CommandError::CleanupTimedOut);
        }

        if self.thread.join().is_err() {
            return Err(CommandError::Io {
                stage: IoStage::ReadOutput,
                kind: io::ErrorKind::Other,
            });
        }
        match result {
            Ok(result) => result.map_err(|error| io_error(IoStage::ReadOutput, &error)),
            Err(RecvTimeoutError::Disconnected) => Err(CommandError::Io {
                stage: IoStage::ReadOutput,
                kind: io::ErrorKind::BrokenPipe,
            }),
            Err(RecvTimeoutError::Timeout) => unreachable!("handled before joining the reader"),
        }
    }
}

fn io_error(stage: IoStage, error: &io::Error) -> CommandError {
    CommandError::Io { stage, kind: error.kind() }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    #[cfg(unix)]
    use std::{fs, os::unix::process::ExitStatusExt, path::Path, time::Instant};

    use super::*;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runs_a_platform_native_process() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.arg("--help");

        let output = output(command, TEST_TIMEOUT).await.unwrap();

        assert!(output.status.success());
        assert!(!output.stdout.is_empty() || !output.stderr.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn preserves_binary_stdout_and_stderr() {
        let output =
            output(shell("printf '\\001\\200\\377'; printf '\\002\\201\\376' >&2"), TEST_TIMEOUT)
                .await
                .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout, [1, 128, 255]);
        assert_eq!(output.stderr, [2, 129, 254]);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn preserves_nonzero_exit_status() {
        let output = output(shell("exit 23"), TEST_TIMEOUT).await.unwrap();

        assert_eq!(output.status.code(), Some(23));
        assert_eq!(output.status.signal(), None);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn preserves_signal_exit_status() {
        let output = output(shell("kill -TERM $$"), TEST_TIMEOUT).await.unwrap();

        assert_eq!(output.status.code(), None);
        assert_eq!(output.status.signal(), Some(libc::SIGTERM));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn drains_stdout_and_stderr_without_deadlock() {
        let output = output(
            shell("i=0; while [ \"$i\" -lt 70000 ]; do printf o; printf e >&2; i=$((i + 1)); done"),
            TEST_TIMEOUT,
        )
        .await
        .unwrap();

        assert_eq!(output.stdout, vec![b'o'; 70_000]);
        assert_eq!(output.stderr, vec![b'e'; 70_000]);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn supplies_null_stdin() {
        let output =
            output(shell("if IFS= read -r value; then exit 91; else exit 0; fi"), TEST_TIMEOUT)
                .await
                .unwrap();

        assert!(output.status.success());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn timeout_terminates_a_hanging_descendant() {
        let started = Instant::now();
        let error = output(shell("(while :; do sleep 10; done) & wait"), Duration::from_millis(25))
            .await
            .unwrap_err();

        assert_eq!(error, CommandError::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "process-group cleanup took {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn leader_exit_terminates_a_descendant_retaining_pipes() {
        let started = Instant::now();
        let output = output(shell("(exec sleep 10) & exit 23"), TEST_TIMEOUT).await.unwrap();

        assert_eq!(output.status.code(), Some(23));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "descendant retained command pipes for {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn leader_exit_terminates_a_descendant_that_closed_both_pipes() {
        let directory = tempfile::tempdir().unwrap();
        let group_file = directory.path().join("group-pid");
        let descendant_file = directory.path().join("descendant-pid");
        let mut command = shell(
            "printf '%s' \"$$\" > \"$1\"; \
             (trap '' HUP TERM; while :; do sleep 10; done) \
             </dev/null >/dev/null 2>&1 & \
             printf '%s' \"$!\" > \"$2\"; exit 29",
        );
        command.arg("gherrit-test").arg(&group_file).arg(&descendant_file);

        let output = output(command, TEST_TIMEOUT).await.unwrap();
        let group = wait_for_pid(&group_file).await;
        let descendant = wait_for_pid(&descendant_file).await;

        assert_eq!(output.status.code(), Some(29));
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        wait_until_process_group_exits(group).await;
        wait_until_process_exits(descendant).await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn zero_timeout_does_not_start_the_command() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("started");
        let mut command = shell("printf started > \"$1\"");
        command.arg("gherrit-test").arg(&marker);

        let error = output(command, Duration::ZERO).await.unwrap_err();

        assert_eq!(error, CommandError::TimedOut);
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn aborting_the_future_terminates_the_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let mut command = shell("printf '%s' \"$$\" > \"$1\"; while :; do sleep 10; done");
        command.arg("gherrit-test").arg(&pid_file);
        let task = tokio::spawn(output(command, Duration::from_secs(30)));
        let pid = wait_for_pid(&pid_file).await;

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        wait_until_process_group_exits(pid).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn errors_do_not_reveal_command_contents() {
        let secret = "secret-destination-that-must-not-appear";
        let command = Command::new(format!("/definitely/missing/{secret}"));

        let error = output(command, TEST_TIMEOUT).await.unwrap_err();
        let display = error.to_string();
        let debug = format!("{error:?}");

        assert!(!display.contains(secret), "{display}");
        assert!(!debug.contains(secret), "{debug}");
    }

    #[cfg(unix)]
    async fn wait_for_pid(path: &Path) -> libc::pid_t {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(pid) = fs::read_to_string(path) {
                return pid.parse().unwrap();
            }
            assert!(Instant::now() < deadline, "command did not publish its PID");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[cfg(unix)]
    async fn wait_until_process_group_exits(pid: libc::pid_t) {
        wait_until_process_is_absent(-pid).await;
    }

    #[cfg(unix)]
    async fn wait_until_process_exits(pid: libc::pid_t) {
        wait_until_process_is_absent(pid).await;
    }

    #[cfg(unix)]
    async fn wait_until_process_is_absent(target: libc::pid_t) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let result = unsafe { libc::kill(target, 0) };
            if result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                return;
            }
            if Instant::now() >= deadline {
                unsafe {
                    libc::kill(target, libc::SIGKILL);
                }
                panic!("cancelled command process target {target} remained alive");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}
