use std::{
    fmt,
    io::{self, Read},
    process::{Command, ExitStatus, Stdio},
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
// This is scheduler hand-off slack for the outer async hard bound, not an
// extension of the worker's five-second cleanup interval.
const SUPERVISOR_GRACE: Duration = Duration::from_secs(1);

/// Maximum stdout retained from one remote Git command.
///
/// Sixty-four MiB accommodates hundreds of thousands of ordinary `ls-remote`
/// records while making the command boundary finite even for a malformed or
/// adversarial remote. Bytes beyond this cap are still drained before the
/// command fails. Stderr is always drained but never retained.
const STDOUT_LIMIT: usize = 64 * 1024 * 1024;
const PIPE_BUFFER_SIZE: usize = 16 * 1024;

/// One finite execution deadline for destination-bound Git reads.
///
/// Callers pass this explicitly so every remote observation and acquisition
/// uses the same bounded process-lifecycle policy.
pub(super) const REMOTE_GIT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs one remote Git command without blocking GHerrit's Tokio runtime.
///
/// The deadline covers process execution. A fixed, bounded cleanup interval is
/// started when execution actually stops for killing the owned process
/// boundary, reaping its leader, and draining both output pipes. On Unix that
/// boundary includes descendants which remain in the process group GHerrit
/// created; a descendant which deliberately escapes the group is outside the
/// guarantee. On Windows it is the owned kill-on-drop job object. Dropping
/// this future requests the same bounded cleanup.
pub(super) async fn output(
    command: Command,
    timeout: Duration,
) -> Result<CommandOutput, CommandError> {
    output_with_stdout_limit(command, timeout, STDOUT_LIMIT).await
}

async fn output_with_stdout_limit(
    command: Command,
    timeout: Duration,
    stdout_limit: usize,
) -> Result<CommandOutput, CommandError> {
    output_with_reader_fault(command, timeout, stdout_limit, ReaderFault::None).await
}

#[cfg(test)]
async fn output_with_injected_stdout_failure(
    command: Command,
    timeout: Duration,
) -> Result<CommandOutput, CommandError> {
    output_with_reader_fault(command, timeout, STDOUT_LIMIT, ReaderFault::Stdout).await
}

async fn output_with_reader_fault(
    command: Command,
    timeout: Duration,
    stdout_limit: usize,
    reader_fault: ReaderFault,
) -> Result<CommandOutput, CommandError> {
    let started = Instant::now();
    let deadline = started.checked_add(timeout).ok_or(CommandError::InvalidTimeout)?;
    let supervisor_deadline = deadline
        .checked_add(CLEANUP_TIMEOUT)
        .and_then(|deadline| deadline.checked_add(SUPERVISOR_GRACE))
        .ok_or(CommandError::InvalidTimeout)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut cancellation_guard = CancellationGuard::new(Arc::clone(&cancelled));

    let worker_cancelled = Arc::clone(&cancelled);
    let mut worker = tokio::task::spawn_blocking(move || {
        output_blocking(command, deadline, stdout_limit, reader_fault, &worker_cancelled)
    });

    let result =
        tokio::time::timeout_at(tokio::time::Instant::from_std(supervisor_deadline), &mut worker)
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

#[derive(Clone, Copy)]
enum ReaderFault {
    None,
    #[cfg(test)]
    Stdout,
}

impl ReaderFault {
    fn stdout_fails(self) -> bool {
        #[cfg(test)]
        if matches!(self, Self::Stdout) {
            return true;
        }
        false
    }
}

/// The deliberately small result exposed to remote-command consumers.
///
/// Its debug form reports only non-sensitive status and byte counts. In
/// particular, captured stderr has no accessor and therefore cannot become
/// part of a caller's diagnostic by accident.
pub(super) struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr_bytes: u64,
}

impl CommandOutput {
    pub(super) fn status(&self) -> &ExitStatus {
        &self.status
    }

    pub(super) fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub(super) fn stderr_bytes(&self) -> u64 {
        self.stderr_bytes
    }
}

impl fmt::Debug for CommandOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandOutput")
            .field("status", &self.status)
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr_bytes)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CommandError {
    InvalidTimeout,
    TimedOut,
    StdoutTooLarge { limit: usize },
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
            Self::StdoutTooLarge { limit } => {
                write!(formatter, "remote Git command stdout exceeded the {limit}-byte limit")
            }
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
    stdout_limit: usize,
    reader_fault: ReaderFault,
    cancelled: &AtomicBool,
) -> Result<CommandOutput, CommandError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CommandError::CleanupTimedOut);
    }
    if Instant::now() >= deadline {
        return Err(CommandError::TimedOut);
    }

    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child =
        spawn_command_group(&mut command).map_err(|error| io_error(IoStage::Start, &error))?;
    let mut stdout = match child.inner().stdout.take() {
        Some(stdout) => {
            match PipeReader::start(stdout, stdout_limit, reader_fault.stdout_fails()) {
                Ok(stdout) => PipeState::Active(stdout),
                Err(error) => return fail_after_spawn(child, None, error),
            }
        }
        None => {
            return fail_after_spawn(
                child,
                None,
                CommandError::Io {
                    stage: IoStage::StartOutputReader,
                    kind: io::ErrorKind::BrokenPipe,
                },
            );
        }
    };
    let mut stderr = match child.inner().stderr.take() {
        Some(stderr) => match PipeReader::start(stderr, 0, false) {
            Ok(stderr) => PipeState::Active(stderr),
            Err(error) => return fail_after_spawn(child, Some(stdout), error),
        },
        None => {
            return fail_after_spawn(
                child,
                Some(stdout),
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
        if let Err(error) = stdout.poll() {
            break Stopped::Reader(error);
        }
        if let Err(error) = stderr.poll() {
            break Stopped::Reader(error);
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
    // be recycled before this signal. This lets us terminate descendants which
    // remain in the group even when they closed both output pipes before the
    // leader exited. A descendant which deliberately escapes that group is
    // outside this guarantee. Windows has an owned job-object handle rather
    // than a numeric process-group ID.
    let cleanup_started = Instant::now();
    let _ = child.kill();
    let cleanup_deadline = cleanup_deadline(cleanup_started)?;
    let status = reap_leader(&mut child, cleanup_deadline);
    let stdout = stdout.finish(cleanup_deadline);
    let stderr = stderr.finish(cleanup_deadline);
    let status = status?;
    let stdout = stdout?;
    let stderr = stderr?;

    // Once the leader completed, exceeding the stdout boundary is the command
    // result even when its status was nonzero. Without acknowledged command
    // completion, the timeout, cancellation, or monitor failure remains the
    // result instead of being masked by bytes observed along the way.
    match stopped {
        Stopped::Complete if stdout.overflowed => {
            Err(CommandError::StdoutTooLarge { limit: stdout_limit })
        }
        Stopped::Complete => {
            Ok(CommandOutput { status, stdout: stdout.retained, stderr_bytes: stderr.total_bytes })
        }
        Stopped::TimedOut => Err(CommandError::TimedOut),
        // Cancellation is observable only by the task which dropped this
        // future. If an outer hard deadline still observes the worker result,
        // cleanup—not a third command outcome—is what failed.
        Stopped::Cancelled => Err(CommandError::CleanupTimedOut),
        Stopped::Monitor(error) => Err(error),
        Stopped::Reader(error) => Err(error),
    }
}

fn fail_after_spawn(
    mut child: GroupChild,
    stdout: Option<PipeState>,
    error: CommandError,
) -> Result<CommandOutput, CommandError> {
    let cleanup_started = Instant::now();
    let _ = child.kill();
    let cleanup_deadline = cleanup_deadline(cleanup_started)?;
    let reap = reap_leader(&mut child, cleanup_deadline);
    let stdout = stdout.map(|stdout| stdout.finish(cleanup_deadline));
    reap?;
    if let Some(stdout) = stdout {
        stdout?;
    }
    Err(error)
}

fn cleanup_deadline(started: Instant) -> Result<Instant, CommandError> {
    started.checked_add(CLEANUP_TIMEOUT).ok_or(CommandError::InvalidTimeout)
}

enum Stopped {
    Complete,
    TimedOut,
    Cancelled,
    Monitor(CommandError),
    Reader(CommandError),
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
    result: Receiver<io::Result<PipeCapture>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PipeReader {
    fn start(
        mut pipe: impl Read + Send + 'static,
        retained_limit: usize,
        fail_immediately: bool,
    ) -> Result<Self, CommandError> {
        let (sender, result) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("gherrit-remote-git-output".to_owned())
            .spawn(move || {
                if fail_immediately {
                    let _ = sender.send(Err(io::Error::other("injected reader failure")));
                    return;
                }
                let mut capture = PipeCapture::default();
                let mut buffer = [0; PIPE_BUFFER_SIZE];
                let result = loop {
                    match pipe.read(&mut buffer) {
                        Ok(0) => break Ok(capture),
                        Ok(read) => capture.record(&buffer[..read], retained_limit),
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) => break Err(error),
                    }
                };
                let _ = sender.send(result);
            })
            .map_err(|error| io_error(IoStage::StartOutputReader, &error))?;
        Ok(Self { result, thread: Some(thread) })
    }

    fn poll(&mut self) -> Result<Option<PipeCapture>, CommandError> {
        match self.result.try_recv() {
            Ok(result) => self.complete(result).map(Some),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(self.disconnected()),
        }
    }

    fn finish(mut self, deadline: Instant) -> Result<PipeCapture, CommandError> {
        let result = self.result.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        if matches!(&result, Err(RecvTimeoutError::Timeout)) {
            // Joining a reader which has not observed EOF would defeat the
            // cleanup deadline. The process group has already been killed;
            // this case means the operating-system lifecycle boundary itself
            // failed, so return the bounded cleanup error.
            return Err(CommandError::CleanupTimedOut);
        }

        match result {
            Ok(result) => self.complete(result),
            Err(RecvTimeoutError::Disconnected) => Err(self.disconnected()),
            Err(RecvTimeoutError::Timeout) => unreachable!("handled before joining the reader"),
        }
    }

    fn complete(&mut self, result: io::Result<PipeCapture>) -> Result<PipeCapture, CommandError> {
        let thread = self.thread.take().expect("pipe reader completion observed twice");
        if thread.join().is_err() {
            return Err(CommandError::Io {
                stage: IoStage::ReadOutput,
                kind: io::ErrorKind::Other,
            });
        }
        result.map_err(|error| io_error(IoStage::ReadOutput, &error))
    }

    fn disconnected(&mut self) -> CommandError {
        let thread = self.thread.take().expect("pipe reader completion observed twice");
        let kind =
            if thread.join().is_err() { io::ErrorKind::Other } else { io::ErrorKind::BrokenPipe };
        CommandError::Io { stage: IoStage::ReadOutput, kind }
    }
}

enum PipeState {
    Active(PipeReader),
    Complete(PipeCapture),
    Failed(CommandError),
}

impl PipeState {
    fn poll(&mut self) -> Result<(), CommandError> {
        let Self::Active(reader) = self else {
            return Ok(());
        };
        match reader.poll() {
            Ok(Some(capture)) => *self = Self::Complete(capture),
            Ok(None) => {}
            Err(error) => {
                *self = Self::Failed(error);
                return Err(error);
            }
        }
        Ok(())
    }

    fn finish(self, deadline: Instant) -> Result<PipeCapture, CommandError> {
        match self {
            Self::Active(reader) => reader.finish(deadline),
            Self::Complete(capture) => Ok(capture),
            Self::Failed(error) => Err(error),
        }
    }
}

#[derive(Default)]
struct PipeCapture {
    retained: Vec<u8>,
    total_bytes: u64,
    overflowed: bool,
}

impl PipeCapture {
    fn record(&mut self, bytes: &[u8], retained_limit: usize) {
        self.total_bytes =
            self.total_bytes.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let remaining = retained_limit.saturating_sub(self.retained.len());
        let retained = remaining.min(bytes.len());
        self.retained.extend_from_slice(&bytes[..retained]);
        self.overflowed |= retained != bytes.len();
    }
}

fn io_error(stage: IoStage, error: &io::Error) -> CommandError {
    CommandError::Io { stage, kind: error.kind() }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        io::Write as _,
        process,
        time::{Duration, Instant},
    };
    #[cfg(unix)]
    use std::{
        ffi::CString,
        os::{
            fd::AsRawFd,
            unix::{ffi::OsStrExt, fs::OpenOptionsExt, process::ExitStatusExt},
        },
        path::Path,
    };

    use super::*;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);
    const REEXEC_MODE: &str = "GHERRIT_SUBPROCESS_TEST_MODE";
    const REEXEC_BYTES: &str = "GHERRIT_SUBPROCESS_TEST_BYTES";
    const REEXEC_MARKER: &str = "GHERRIT_SUBPROCESS_TEST_MARKER";
    const REEXEC_SECRET: &str = "GHERRIT_SUBPROCESS_TEST_SECRET";
    #[cfg(unix)]
    const REEXEC_LIFETIME: &str = "GHERRIT_SUBPROCESS_TEST_LIFETIME";
    #[cfg(unix)]
    const REEXEC_READY: &str = "GHERRIT_SUBPROCESS_TEST_READY";
    const REEXEC_TEST: &str = "pre_push::subprocess::tests::reexec_helper";

    fn reexec(mode: &str) -> Command {
        let mut command = Command::new(env::current_exe().unwrap());
        command.args(["--exact", REEXEC_TEST, "--nocapture"]).env(REEXEC_MODE, mode);
        command
    }

    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn reexec_helper() {
        let Ok(mode) = env::var(REEXEC_MODE) else { return };
        match mode.as_str() {
            "binary-stdout" => {
                std::io::stdout().write_all(&[1, 128, 255]).unwrap();
            }
            "nonzero" => process::exit(23),
            "large-both" => {
                let bytes = reexec_bytes();
                let chunk = 8 * 1024;
                let mut stdout = std::io::stdout().lock();
                let mut stderr = std::io::stderr().lock();
                let mut remaining = bytes;
                while remaining != 0 {
                    let written = remaining.min(chunk);
                    stdout.write_all(&vec![b'o'; written]).unwrap();
                    stderr.write_all(&vec![b'e'; written]).unwrap();
                    remaining -= written;
                }
            }
            "stderr-only" => {
                std::io::stderr().write_all(&vec![b'e'; reexec_bytes()]).unwrap();
            }
            "stdout-overflow" => {
                let bytes = reexec_bytes();
                std::io::stdout().write_all(&vec![b'o'; bytes]).unwrap();
                std::io::stderr().write_all(&vec![b'e'; bytes]).unwrap();
                fs::write(env::var_os(REEXEC_MARKER).unwrap(), b"complete").unwrap();
            }
            "private-overflow" => {
                let secret = env::var(REEXEC_SECRET).unwrap();
                for _ in 0..64 {
                    std::io::stdout().write_all(secret.as_bytes()).unwrap();
                    std::io::stderr().write_all(secret.as_bytes()).unwrap();
                }
            }
            "null-stdin" => {
                let mut byte = [0];
                let status = match std::io::stdin().read(&mut byte) {
                    Ok(0) => 0,
                    _ => 91,
                };
                process::exit(status);
            }
            "leader-waits" => {
                let mut descendant = reexec("sleep").spawn().unwrap();
                descendant.wait().unwrap();
            }
            "leader-exits" => {
                reexec("sleep").spawn().unwrap();
                process::exit(23);
            }
            #[cfg(unix)]
            "leader-waits-probed" => {
                let mut descendant = reexec("probe-sleep");
                descendant.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
                let descendant = descendant.spawn().unwrap();
                // Dropping this ordinary Child leaves it running in the
                // inherited group. The readiness read keeps this leader alive
                // until that descendant proves membership and opens its
                // lifetime writer.
                drop(descendant);
                assert_eq!(fs::read(env::var_os(REEXEC_READY).unwrap()).unwrap(), b"ready\n");
                fs::write(env::var_os(REEXEC_MARKER).unwrap(), b"ready").unwrap();
                thread::sleep(Duration::from_secs(30));
            }
            #[cfg(unix)]
            "leader-exits-probed" => {
                let mut descendant = reexec("probe-sleep");
                descendant.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
                let descendant = descendant.spawn().unwrap();
                // Dropping this ordinary Child leaves it running in the
                // inherited group. The readiness read keeps this leader alive
                // until that descendant proves membership and opens its
                // lifetime writer.
                drop(descendant);
                assert_eq!(fs::read(env::var_os(REEXEC_READY).unwrap()).unwrap(), b"ready\n");
                process::exit(29);
            }
            "sleep" => thread::sleep(Duration::from_secs(10)),
            #[cfg(unix)]
            "stdout-closes-then-waits" => {
                // SAFETY: the fixture owns stdout and closes it once, then
                // exits without trying to write to it again.
                assert_eq!(unsafe { libc::close(libc::STDOUT_FILENO) }, 0);
                thread::sleep(Duration::from_millis(150));
            }
            #[cfg(unix)]
            "probe-sleep" => {
                for signal in [libc::SIGHUP, libc::SIGTERM] {
                    // SAFETY: this isolated fixture installs only the standard
                    // ignore disposition for two valid signal numbers.
                    let previous = unsafe { libc::signal(signal, libc::SIG_IGN) };
                    assert_ne!(previous, libc::SIG_ERR);
                }
                // SAFETY: these calls only read the current process identity
                // and its live parent's identity.
                let process_group = unsafe { libc::getpgrp() };
                let parent = unsafe { libc::getppid() };
                assert_eq!(process_group, parent);
                let mut lifetime = fs::OpenOptions::new()
                    .write(true)
                    .open(env::var_os(REEXEC_LIFETIME).unwrap())
                    .unwrap();
                lifetime.write_all(b"ready\n").unwrap();
                fs::write(env::var_os(REEXEC_READY).unwrap(), b"ready\n").unwrap();
                thread::sleep(Duration::from_secs(30));
            }
            "marker" => {
                fs::write(env::var_os(REEXEC_MARKER).unwrap(), b"started").unwrap();
            }
            other => panic!("unknown subprocess re-exec mode {other}"),
        }

        // Avoid libtest adding a success suffix to controlled stdout. The
        // direct and bounded invocations still receive the same prefix.
        process::exit(0);
    }

    fn reexec_bytes() -> usize {
        env::var(REEXEC_BYTES).unwrap().parse().unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runs_a_platform_native_process() {
        let mut command = Command::new(env::current_exe().unwrap());
        command.arg("--help");

        let output = output(command, TEST_TIMEOUT).await.unwrap();

        assert!(output.status().success());
        assert!(!output.stdout().is_empty() || output.stderr_bytes() != 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preserves_binary_stdout_exactly() {
        let expected = reexec("binary-stdout").output().unwrap();
        let output = output(reexec("binary-stdout"), TEST_TIMEOUT).await.unwrap();

        assert_eq!(output.status(), &expected.status);
        assert_eq!(output.stdout(), expected.stdout);
        assert!(output.stdout().ends_with(&[1, 128, 255]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preserves_nonzero_exit_status() {
        let output = output(reexec("nonzero"), TEST_TIMEOUT).await.unwrap();

        assert_eq!(output.status().code(), Some(23));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn preserves_signal_exit_status() {
        let output = output(shell("kill -TERM $$"), TEST_TIMEOUT).await.unwrap();

        assert_eq!(output.status().code(), None);
        assert_eq!(output.status().signal(), Some(libc::SIGTERM));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drains_stdout_and_stderr_without_deadlock() {
        let bytes = 1024 * 1024;
        let mut command = reexec("large-both");
        command.env(REEXEC_BYTES, bytes.to_string());
        let output = output(command, TEST_TIMEOUT).await.unwrap();

        assert!(output.stdout().ends_with(&vec![b'o'; bytes]));
        assert_eq!(output.stderr_bytes(), bytes as u64);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn normal_stdout_eof_does_not_stop_the_leader() {
        let started = Instant::now();
        let output = output(reexec("stdout-closes-then-waits"), TEST_TIMEOUT).await.unwrap();

        assert!(output.status().success());
        assert!(started.elapsed() >= Duration::from_millis(100));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supplies_null_stdin() {
        let output = output(reexec("null-stdin"), TEST_TIMEOUT).await.unwrap();

        assert!(output.status().success());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_terminates_a_hanging_descendant() {
        let started = Instant::now();
        let error = output(reexec("leader-waits"), Duration::from_millis(100)).await.unwrap_err();

        assert_eq!(error, CommandError::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "process-group cleanup took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn leader_exit_terminates_a_descendant_retaining_pipes() {
        let started = Instant::now();
        let output = output(reexec("leader-exits"), TEST_TIMEOUT).await.unwrap();

        assert_eq!(output.status().code(), Some(23));
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
        let lifetime_path = directory.path().join("descendant-lifetime");
        let lifetime = ProcessProbe::start(&lifetime_path);
        let ready_path = directory.path().join("descendant-ready");
        create_fifo(&ready_path);
        // Redirection closes the command pipes, while exec preserves the
        // group-leader identity assigned to this shell.
        let mut command = shell("exec \"$1\" --exact \"$2\" --nocapture >/dev/null 2>&1");
        command
            .arg("gherrit-test")
            .arg(env::current_exe().unwrap())
            .arg(REEXEC_TEST)
            .env(REEXEC_MODE, "leader-exits-probed")
            .env(REEXEC_LIFETIME, &lifetime_path)
            .env(REEXEC_READY, &ready_path);

        let output = output(command, TEST_TIMEOUT).await.unwrap();

        assert_eq!(output.status().code(), Some(29));
        assert!(output.stdout().is_empty());
        assert_eq!(output.stderr_bytes(), 0);
        lifetime.wait_closed();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn zero_timeout_does_not_start_the_command() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("started");
        let mut command = reexec("marker");
        command.env(REEXEC_MARKER, &marker);

        let error = output(command, Duration::ZERO).await.unwrap_err();

        assert_eq!(error, CommandError::TimedOut);
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn aborting_the_future_terminates_the_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let lifetime_path = directory.path().join("descendant-lifetime");
        let lifetime = ProcessProbe::start(&lifetime_path);
        let ready_path = directory.path().join("descendant-ready");
        create_fifo(&ready_path);
        let marker = directory.path().join("leader-ready");
        let mut command = shell("exec \"$1\" --exact \"$2\" --nocapture >/dev/null 2>&1");
        command
            .arg("gherrit-test")
            .arg(env::current_exe().unwrap())
            .arg(REEXEC_TEST)
            .env(REEXEC_MODE, "leader-waits-probed")
            .env(REEXEC_LIFETIME, &lifetime_path)
            .env(REEXEC_READY, &ready_path)
            .env(REEXEC_MARKER, &marker);
        let task = tokio::spawn(output(command, Duration::from_secs(30)));
        wait_for_marker(&marker).await;

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        lifetime.wait_closed();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reader_failure_stops_and_cleans_up_immediately() {
        let started = Instant::now();
        let error = output_with_injected_stdout_failure(reexec("leader-waits"), TEST_TIMEOUT)
            .await
            .unwrap_err();

        assert_eq!(
            error,
            CommandError::Io { stage: IoStage::ReadOutput, kind: io::ErrorKind::Other }
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "reader failure did not start immediate cleanup: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stdout_overflow_fails_after_draining_the_process() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("complete");
        let limit = 64 * 1024;
        let bytes = 1024 * 1024;
        let mut command = reexec("stdout-overflow");
        command.env(REEXEC_BYTES, bytes.to_string()).env(REEXEC_MARKER, &marker);

        let error = output_with_stdout_limit(command, TEST_TIMEOUT, limit).await.unwrap_err();

        assert_eq!(error, CommandError::StdoutTooLarge { limit });
        assert_eq!(fs::read(marker).unwrap(), b"complete");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn excessive_stderr_is_drained_without_retention() {
        let bytes = 2 * 1024 * 1024;
        let mut command = reexec("stderr-only");
        command.env(REEXEC_BYTES, bytes.to_string());

        let output = output(command, TEST_TIMEOUT).await.unwrap();

        assert!(output.status().success());
        assert_eq!(output.stderr_bytes(), bytes as u64);
        assert!(!format!("{output:?}").contains(&"e".repeat(128)));
    }

    #[test]
    fn byte_counts_saturate_instead_of_overflowing() {
        let mut capture = PipeCapture { total_bytes: u64::MAX - 1, ..PipeCapture::default() };

        capture.record(b"abcd", 0);

        assert_eq!(capture.total_bytes, u64::MAX);
    }

    #[test]
    fn pipe_capture_preserves_the_exact_prefix_at_its_limit() {
        let mut capture = PipeCapture::default();

        capture.record(&[0, 128], 3);
        capture.record(&[255, 1], 3);

        assert_eq!(capture.retained, [0, 128, 255]);
        assert_eq!(capture.total_bytes, 4);
        assert!(capture.overflowed);
    }

    #[test]
    fn cleanup_deadline_starts_when_cleanup_begins() {
        let execution_started = Instant::now();
        let execution_deadline = execution_started + Duration::from_secs(120);
        let cleanup_started = execution_started + Duration::from_secs(7);

        let actual = cleanup_deadline(cleanup_started).unwrap();

        assert_eq!(actual, cleanup_started + CLEANUP_TIMEOUT);
        assert_ne!(actual, execution_deadline + CLEANUP_TIMEOUT);
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

    #[tokio::test(flavor = "current_thread")]
    async fn overflow_errors_do_not_reveal_output() {
        let secret = "secret-output-that-must-not-appear";
        let mut command = reexec("private-overflow");
        command.env(REEXEC_SECRET, secret);

        let error = output_with_stdout_limit(command, TEST_TIMEOUT, 32).await.unwrap_err();
        let display = error.to_string();
        let debug = format!("{error:?}");

        assert_eq!(error, CommandError::StdoutTooLarge { limit: 32 });
        assert!(!display.contains(secret), "{display}");
        assert!(!debug.contains(secret), "{debug}");
    }

    #[cfg(unix)]
    async fn wait_for_marker(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if matches!(fs::read(path).as_deref(), Ok(b"ready")) {
                return;
            }
            assert!(Instant::now() < deadline, "command did not publish its readiness marker");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[cfg(unix)]
    fn create_fifo(path: &Path) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path` supplies a live, NUL-terminated string for the
        // duration of this call, and the mode contains only permission bits.
        let result = unsafe { libc::mkfifo(path.as_ptr(), libc::S_IRUSR | libc::S_IWUSR) };
        if result == -1 {
            panic!("failed to create process probe: {}", io::Error::last_os_error());
        }
    }

    #[cfg(unix)]
    struct ProcessProbe {
        reader: fs::File,
        keepalive: Option<fs::File>,
    }

    #[cfg(unix)]
    impl ProcessProbe {
        fn start(path: &Path) -> Self {
            // A keepalive writer prevents EOF before the exact fixture opens
            // its writer and publishes the ready record.
            create_fifo(path);
            let reader = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .unwrap();
            let keepalive = fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .unwrap();
            Self { reader, keepalive: Some(keepalive) }
        }

        fn wait_closed(mut self) {
            drop(self.keepalive.take());
            let mut ready = [0; 6];
            self.reader.read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"ready\n");

            // The exact descendant is the only possible writer after the
            // keepalive closes. EOF therefore proves that it exited, without
            // consulting a recyclable PID or PGID. Keep the replaced probe's
            // two-second post-completion bound.
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut unexpected = [0];
            match self.reader.read(&mut unexpected) {
                Ok(0) => return,
                Ok(_) => panic!("process probe contained unexpected lifetime data"),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => panic!("failed to read process probe EOF: {error}"),
            }
            // A FIFO close can land between the read and poll registration.
            // Re-read after each bounded wait so EOF itself remains the
            // identity-stable observation even if HUP was not latched.
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "process probe remained open");
                let wait = remaining.min(POLL_INTERVAL);
                let timeout = i32::try_from(wait.as_millis().max(1)).unwrap_or(i32::MAX);
                let mut event = libc::pollfd {
                    fd: self.reader.as_raw_fd(),
                    events: libc::POLLIN | libc::POLLHUP,
                    revents: 0,
                };
                // SAFETY: `event` is a valid one-element pollfd buffer and
                // `timeout` is a finite nonnegative millisecond count.
                let result = unsafe { libc::poll(&mut event, 1, timeout) };
                if result == -1 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    panic!("failed to observe process probe: {error}");
                }
                match self.reader.read(&mut unexpected) {
                    Ok(0) => return,
                    Ok(_) => panic!("process probe contained unexpected lifetime data"),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => panic!("failed to read process probe EOF: {error}"),
                }
            }
        }
    }
}
