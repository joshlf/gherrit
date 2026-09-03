//! Bounded, cancellation-aware ownership for remote command processes.
//!
//! Unix children start as process-group leaders. When the caller owns a
//! controlling terminal, that group receives a serialized foreground lease so
//! ordinary Git and SSH prompts remain usable. Both terminal restoration and
//! an armed bounded group/leader kill-and-reap fallback cover every exit path
//! until the leader is reaped or wait no longer proves that its numeric
//! identity is retained. Windows children start suspended, receive a
//! direct-child guard, and join a private kill-on-close job before any fallible
//! thread discovery or resume. Ordinary paths perform bounded termination,
//! boundary observation, reaping, and pipe draining; drop guards cover panic
//! and future-cancellation unwinding.
//!
//! This module alone sequences cleanup and combines its evidence. The selected
//! concrete platform module supplies process ownership and proof operations;
//! there is deliberately no trait or generalized process-boundary abstraction.
//! A caller-visible timeout without successful cleanup proof is not evidence
//! that the operating-system process boundary is already gone.

#[cfg(all(windows, test))]
use std::sync::mpsc;
use std::{
    fmt,
    io::{self, Read},
    process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
// This is scheduler hand-off slack for the outer async hard bound, not an
// extension of the worker's five-second cleanup interval.
const SUPERVISOR_GRACE: Duration = Duration::from_secs(1);

/// Maximum stdout retained from one remote Git command.
///
/// Sixty-four MiB accommodates hundreds of thousands of ordinary `ls-remote`
/// records while making the command boundary finite even for a malformed or
/// adversarial remote. Observing the first byte beyond this cap stops execution
/// and enters bounded cleanup immediately.
pub(super) const REMOTE_GIT_STDOUT_LIMIT: usize = 64 * 1024 * 1024;
/// Maximum stderr retained for a diagnostic after normal process completion.
///
/// The suffix is retained because a local pre-push hook writes immediately
/// before Git's final failure line. Earlier stderr is still drained and
/// counted. It never turns a completed command into an operational failure,
/// and no output is exposed at all after a timeout, cancellation, overflow,
/// reader failure, or cleanup failure.
const STDERR_RETAIN_LIMIT: usize = 16 * 1024;
const PIPE_BUFFER_SIZE: usize = 16 * 1024;

/// One finite execution deadline for destination-bound Git commands.
///
/// Callers pass this explicitly so every remote Git operation uses the same
/// bounded process-lifecycle policy.
pub(super) const REMOTE_GIT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs one remote Git command without blocking GHerrit's Tokio runtime.
///
/// The deadline covers process execution. A fixed, bounded cleanup interval is
/// started when execution actually stops for killing the owned process
/// boundary and reaping its leader. After normal leader completion, finite
/// buffered output is also drained to EOF under that same deadline; failure
/// paths close the pipe handles without exposing output. On Unix the boundary
/// includes descendants which remain in the process group GHerrit created and
/// remain signalable by GHerrit; a descendant which deliberately changes its
/// group or credentials is outside the guarantee. On Windows it is the owned
/// kill-on-close job object. Dropping this future requests the same bounded
/// cleanup.
pub(super) async fn output(
    command: Command,
    timeout: Duration,
) -> Result<CommandOutput, CommandError> {
    output_with_stdout_limit(command, timeout, REMOTE_GIT_STDOUT_LIMIT).await
}

pub(super) async fn output_with_stdout_limit(
    command: Command,
    timeout: Duration,
    stdout_limit: usize,
) -> Result<CommandOutput, CommandError> {
    output_with_faults(command, timeout, stdout_limit, Faults::NONE).await
}

#[cfg(test)]
async fn output_with_injected_stdout_failure(
    command: Command,
    timeout: Duration,
) -> Result<CommandOutput, CommandError> {
    output_with_faults(
        command,
        timeout,
        REMOTE_GIT_STDOUT_LIMIT,
        Faults { reader: ReaderFault::ReadStdout, ..Faults::NONE },
    )
    .await
}

async fn output_with_faults(
    command: Command,
    timeout: Duration,
    stdout_limit: usize,
    faults: Faults,
) -> Result<CommandOutput, CommandError> {
    let started = Instant::now();
    let deadline_at = started.checked_add(timeout).ok_or(CommandError::InvalidTimeout)?;
    let supervisor_deadline = deadline_at
        .checked_add(CLEANUP_TIMEOUT)
        .and_then(|deadline| deadline.checked_add(SUPERVISOR_GRACE))
        .ok_or(CommandError::InvalidTimeout)?;
    let deadline = Deadline::execution(deadline_at, faults.deadline);
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut cancellation_guard = CancellationGuard::new(Arc::clone(&cancelled));

    let worker_cancelled = Arc::clone(&cancelled);
    let mut worker = tokio::task::spawn_blocking(move || {
        output_blocking(command, deadline, stdout_limit, faults, &worker_cancelled)
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
            // blocking task has started, the cancellation flag makes it
            // signal the owned boundary and reap the leader.
            cancellation_guard.cancel();
            worker.abort();
            Err(CommandError::CleanupTimedOut)
        }
    }
}

struct Faults {
    reader: ReaderFault,
    deadline: DeadlineFault,
    termination: TerminationFault,
    boundary: BoundaryFault,
    #[cfg(windows)]
    platform: PlatformFault,
}

impl Faults {
    const NONE: Self = Self {
        reader: ReaderFault::None,
        deadline: DeadlineFault::None,
        termination: TerminationFault::None,
        boundary: BoundaryFault::None,
        #[cfg(windows)]
        platform: PlatformFault::None,
    };
}

#[cfg(windows)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum PlatformFaultStage {
    ConfigureJob,
    BeforeAssignment,
    BeforeAssignCall,
    ThreadLookup,
    BeforeResume,
    Resume,
}

#[cfg(windows)]
enum PlatformFault {
    None,
    #[cfg(test)]
    Inject {
        stage: PlatformFaultStage,
        fail: bool,
        spawned: mpsc::Sender<u32>,
        proceed: mpsc::Receiver<()>,
    },
}

#[cfg(windows)]
impl PlatformFault {
    #[cfg(test)]
    fn is(&self, expected: PlatformFaultStage) -> bool {
        matches!(self, Self::Inject { stage, .. } if *stage == expected)
    }

    fn observe(&self, _stage: PlatformFaultStage, _process_id: u32) -> io::Result<()> {
        match self {
            Self::None => Ok(()),
            #[cfg(test)]
            Self::Inject { stage, spawned, proceed, .. } if *stage == _stage => {
                spawned
                    .send(_process_id)
                    .map_err(|_| io::Error::other("Windows startup observer stopped"))?;
                proceed
                    .recv()
                    .map_err(|_| io::Error::other("Windows startup observer did not continue"))
            }
            #[cfg(test)]
            Self::Inject { .. } => Ok(()),
        }
    }

    #[cfg(test)]
    fn injects(&self, expected: PlatformFaultStage) -> bool {
        matches!(self, Self::Inject { stage, fail: true, .. } if *stage == expected)
    }

    #[cfg(test)]
    fn injected_error() -> io::Error {
        io::Error::other("injected Windows startup failure")
    }
}

#[derive(Clone, Copy)]
enum ReaderFault {
    None,
    #[cfg(test)]
    StartStdout,
    #[cfg(test)]
    StartStderr,
    #[cfg(test)]
    ReadStdout,
    #[cfg(test)]
    ReadStderr,
}

impl ReaderFault {
    fn stdout_start_fails(self) -> bool {
        #[cfg(test)]
        if matches!(self, Self::StartStdout) {
            return true;
        }
        false
    }

    fn stdout_fails(self) -> bool {
        #[cfg(test)]
        if matches!(self, Self::ReadStdout) {
            return true;
        }
        false
    }

    fn stderr_start_fails(self) -> bool {
        #[cfg(test)]
        if matches!(self, Self::StartStderr) {
            return true;
        }
        false
    }

    fn stderr_fails(self) -> bool {
        #[cfg(test)]
        if matches!(self, Self::ReadStderr) {
            return true;
        }
        false
    }
}

#[derive(Clone, Copy)]
enum TerminationFault {
    None,
    #[cfg(test)]
    FailureAfterCleanup,
    #[cfg(test)]
    FailureWithoutProof,
}

impl TerminationFault {
    fn error(self) -> Option<CommandError> {
        #[cfg(test)]
        if matches!(self, Self::FailureAfterCleanup | Self::FailureWithoutProof) {
            return Some(CommandError::Io {
                stage: IoStage::Terminate,
                kind: io::ErrorKind::PermissionDenied,
            });
        }
        None
    }

    fn withholds_proof(self) -> bool {
        #[cfg(test)]
        if matches!(self, Self::FailureWithoutProof) {
            return true;
        }
        false
    }
}

#[derive(Clone, Copy)]
enum BoundaryFault {
    None,
    #[cfg(test)]
    Failure,
    #[cfg(all(windows, test))]
    FirstObservationFailure,
}

impl BoundaryFault {
    fn inject_initial(
        self,
        result: Result<BoundaryEvidence, CommandError>,
    ) -> Result<BoundaryEvidence, CommandError> {
        #[cfg(all(windows, test))]
        if matches!(self, Self::FirstObservationFailure) {
            return Err(CommandError::Io {
                stage: IoStage::ObserveBoundary,
                kind: io::ErrorKind::Other,
            });
        }
        result
    }

    fn inject_final(
        self,
        result: Result<BoundaryProof, CommandError>,
    ) -> Result<BoundaryProof, CommandError> {
        #[cfg(test)]
        if matches!(self, Self::Failure) {
            return Err(CommandError::Io {
                stage: IoStage::ObserveBoundary,
                kind: io::ErrorKind::Other,
            });
        }
        result
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DeadlineFault {
    None,
    ExecutionLeaderExit,
    ExecutionPipeReader,
    CleanupReap,
    CleanupPipeReader,
}

#[derive(Clone, Copy)]
struct Deadline {
    at: Instant,
    timeout_error: CommandError,
    fault: DeadlineFault,
}

impl Deadline {
    fn execution(at: Instant, fault: DeadlineFault) -> Self {
        Self { at, timeout_error: CommandError::TimedOut, fault }
    }

    fn cleanup(started: Instant, execution_at: Instant, fault: DeadlineFault) -> Self {
        let local_at = started
            .checked_add(CLEANUP_TIMEOUT)
            .expect("the fixed cleanup interval must fit in Instant");
        let supervisor_at = execution_at
            .checked_add(CLEANUP_TIMEOUT)
            .expect("the fixed cleanup interval must fit in Instant");
        let at = local_at.min(supervisor_at);
        Self { at, timeout_error: CommandError::CleanupTimedOut, fault }
    }

    fn check(self) -> Result<(), CommandError> {
        if Instant::now() >= self.at { Err(self.timeout_error) } else { Ok(()) }
    }

    fn check_completion(self, injected_at: DeadlineFault) -> Result<(), CommandError> {
        if self.fault == injected_at { Err(self.timeout_error) } else { self.check() }
    }

    fn remaining(self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    /// Runs a nonblocking operating-system observation, retrying `EINTR`
    /// without converting it into lost ownership evidence.
    ///
    /// The deadline is checked before every attempt, so an interruption storm
    /// cannot extend either the execution or cleanup interval.
    fn retry_interrupted<T>(
        self,
        mut operation: impl FnMut() -> io::Result<T>,
    ) -> Result<io::Result<T>, CommandError> {
        loop {
            self.check()?;
            match operation() {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => return Ok(result),
            }
        }
    }
}

/// The deliberately small result exposed to remote-command consumers.
///
/// Its debug form reports only non-sensitive status and byte counts. Captured
/// stderr is available in production only through the destination-aware
/// renderer, which applies conservative redaction and terminal escaping before
/// returning text to a caller. Tests can inspect the bounded raw capture to
/// verify this module's retention policy.
pub(super) struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stderr_bytes: u64,
}

impl CommandOutput {
    pub(super) fn status(&self) -> &ExitStatus {
        &self.status
    }

    pub(super) fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    /// Renders the only caller-visible form of captured stderr.
    ///
    /// Keeping the raw bytes behind this destination-aware boundary prevents
    /// a future remote-command caller from accidentally formatting private
    /// transport output without the required redaction and terminal escaping.
    pub(super) fn child_diagnostic(
        &self,
        destination: &super::destination::PushDestination,
    ) -> Option<String> {
        destination.render_child_diagnostic(&self.stderr, self.stderr_bytes)
    }

    #[cfg(test)]
    fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    #[cfg(test)]
    fn stderr_bytes(&self) -> u64 {
        self.stderr_bytes
    }

    #[cfg(test)]
    pub(super) fn stderr_truncated(&self) -> bool {
        self.stderr_bytes > u64::try_from(self.stderr.len()).unwrap_or(u64::MAX)
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
    Terminate,
    ObserveBoundary,
    RestoreTerminal,
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
            Self::Terminate => "termination",
            Self::ObserveBoundary => "process-boundary observation",
            Self::RestoreTerminal => "terminal restoration",
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
    deadline: Deadline,
    stdout_limit: usize,
    faults: Faults,
    cancelled: &AtomicBool,
) -> Result<CommandOutput, CommandError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CommandError::CleanupTimedOut);
    }
    deadline.check()?;

    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    let started = spawn_owned(&mut command, deadline, cancelled);
    #[cfg(windows)]
    let started = spawn_owned(&mut command, deadline, cancelled, &faults.platform);
    let child = match started {
        Ok(child) => child,
        Err(StartError::BeforeSpawn(error)) => {
            return Err(io_error(IoStage::Start, &error));
        }
        #[cfg(unix)]
        Err(StartError::BeforeSpawnCommand(error)) => return Err(error),
        Err(StartError::AfterSpawn { child, error }) => {
            return fail_after_spawn(child, Readers::None, error, faults, deadline);
        }
    };
    let mut child = StartedChild::Owned(child);
    let mut stdout = match child.child_mut().stdout.take() {
        Some(stdout) => match if faults.reader.stdout_start_fails() {
            Err(CommandError::Io { stage: IoStage::StartOutputReader, kind: io::ErrorKind::Other })
        } else {
            PipeReader::start(
                ChildPipe::Stdout(stdout),
                stdout_limit,
                Retention::Prefix,
                true,
                faults.reader.stdout_fails(),
                deadline,
            )
        } {
            Ok(stdout) => stdout,
            Err(error) => return fail_after_spawn(child, Readers::None, error, faults, deadline),
        },
        None => {
            return fail_after_spawn(
                child,
                Readers::None,
                CommandError::Io {
                    stage: IoStage::StartOutputReader,
                    kind: io::ErrorKind::BrokenPipe,
                },
                faults,
                deadline,
            );
        }
    };
    let mut stderr = match child.child_mut().stderr.take() {
        Some(stderr) => {
            match if faults.reader.stderr_start_fails() {
                Err(CommandError::Io {
                    stage: IoStage::StartOutputReader,
                    kind: io::ErrorKind::Other,
                })
            } else {
                PipeReader::start(
                    ChildPipe::Stderr(stderr),
                    STDERR_RETAIN_LIMIT,
                    Retention::Suffix,
                    false,
                    faults.reader.stderr_fails(),
                    deadline,
                )
            } {
                Ok(stderr) => stderr,
                Err(error) => {
                    return fail_after_spawn(
                        child,
                        Readers::Stdout(stdout),
                        error,
                        faults,
                        deadline,
                    );
                }
            }
        }
        None => {
            return fail_after_spawn(
                child,
                Readers::Stdout(stdout),
                CommandError::Io {
                    stage: IoStage::StartOutputReader,
                    kind: io::ErrorKind::BrokenPipe,
                },
                faults,
                deadline,
            );
        }
    };

    let mut stdout_first = true;
    let stopped = loop {
        if cancelled.load(Ordering::Acquire) {
            break Stopped::Cancelled;
        }
        if deadline.check().is_err() {
            break Stopped::TimedOut;
        }
        let (first, second) =
            if stdout_first { (&mut stdout, &mut stderr) } else { (&mut stderr, &mut stdout) };
        let mut made_progress = match first.poll(deadline, DeadlineFault::ExecutionPipeReader) {
            Ok(progress) => progress == PipeProgress::Progress,
            Err(error) => {
                break if error == CommandError::TimedOut {
                    Stopped::TimedOut
                } else {
                    Stopped::Reader(error)
                };
            }
        };
        if cancelled.load(Ordering::Acquire) {
            break Stopped::Cancelled;
        }
        if deadline.check().is_err() {
            break Stopped::TimedOut;
        }
        match second.poll(deadline, DeadlineFault::ExecutionPipeReader) {
            Ok(progress) => made_progress |= progress == PipeProgress::Progress,
            Err(error) => {
                break if error == CommandError::TimedOut {
                    Stopped::TimedOut
                } else {
                    Stopped::Reader(error)
                };
            }
        }
        stdout_first = !stdout_first;
        if cancelled.load(Ordering::Acquire) {
            break Stopped::Cancelled;
        }
        if deadline.check().is_err() {
            break Stopped::TimedOut;
        }
        let leader = child.leader_exited(deadline);
        let deadline_result = if matches!(&leader, Ok(true)) {
            deadline.check_completion(DeadlineFault::ExecutionLeaderExit)
        } else {
            deadline.check()
        };
        if deadline_result.is_err() {
            break Stopped::TimedOut;
        }
        match leader {
            Ok(true) => break Stopped::Complete,
            Ok(false) if made_progress => {}
            Ok(false) => thread::sleep(POLL_INTERVAL.min(deadline.remaining())),
            Err(error) => break Stopped::Monitor(error),
        }
    };

    let readers = if matches!(&stopped, Stopped::Complete) {
        CleanupReaders::Capture { stdout, stderr }
    } else {
        CleanupReaders::Discard(Readers::Both { stdout, stderr })
    };
    let cleaned = cleanup(child, readers, faults, deadline)?;

    // Any unresolved ownership or cleanup failure returned above takes
    // precedence. Once the boundary is proven, only normal completion exposes
    // captured output; every other stop reports its execution-stage reason.
    match stopped {
        Stopped::Complete => {
            let Some(CapturedPipes { stdout, stderr }) = cleaned.readers else {
                unreachable!("normal completion captures both output pipes")
            };
            debug_assert!(!stdout.overflowed, "stdout overflow must stop execution immediately");
            Ok(CommandOutput {
                status: cleaned.status,
                stdout: stdout.retained,
                stderr: stderr.retained,
                stderr_bytes: stderr.total_bytes,
            })
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
    child: StartedChild,
    readers: Readers,
    error: CommandError,
    faults: Faults,
    execution_deadline: Deadline,
) -> Result<CommandOutput, CommandError> {
    cleanup(child, CleanupReaders::Discard(readers), faults, execution_deadline)?;
    Err(error)
}

struct Cleaned {
    status: ExitStatus,
    readers: Option<CapturedPipes>,
}

struct CapturedPipes {
    stdout: PipeCapture,
    stderr: PipeCapture,
}

enum Readers {
    None,
    Stdout(PipeReader),
    Both { stdout: PipeReader, stderr: PipeReader },
}

impl Readers {
    fn discard(self) {
        match self {
            Self::None => {}
            Self::Stdout(stdout) => drop(stdout),
            Self::Both { stdout, stderr } => {
                drop(stdout);
                drop(stderr);
            }
        }
    }
}

enum CleanupReaders {
    Discard(Readers),
    Capture { stdout: PipeReader, stderr: PipeReader },
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BoundaryEvidence {
    Unproven,
    #[cfg(windows)]
    Empty,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BoundaryProof {
    Empty,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LeaderEvidence {
    Retained,
    Reaped,
    RetentionUncertain,
}

fn cleanup(
    mut child: StartedChild,
    readers: CleanupReaders,
    faults: Faults,
    execution_deadline: Deadline,
) -> Result<Cleaned, CommandError> {
    let deadline = Deadline::cleanup(Instant::now(), execution_deadline.at, faults.deadline);
    child.begin_cleanup(deadline);
    let readers = match readers {
        CleanupReaders::Capture { stdout, stderr } => Some((stdout, stderr)),
        // No output is exposed after timeout, cancellation, overflow, monitor
        // failure, read failure, or startup failure. Drop the owned read handles
        // immediately; an escaped writer cannot retain worker resources.
        CleanupReaders::Discard(readers) => {
            readers.discard();
            None
        }
    };

    // Run every cleanup operation even when an earlier one fails. In
    // particular, the exact child must be reaped and every owned pipe handle
    // must either be closed or, for normal completion, drained to EOF on all
    // post-spawn paths. The first unresolved error is returned only after all
    // bounded cleanup work has been attempted; later exact evidence may
    // supersede an earlier transient operation error.
    let termination = child.terminate();
    let initial_boundary = faults.boundary.inject_initial(child.wait_empty(deadline));
    let reaped = reap_leader(&mut child, deadline);
    let leader = reaped.evidence();
    let before_reap = initial_boundary.as_ref().copied().unwrap_or(BoundaryEvidence::Unproven);
    let unproven_error = reaped.error().unwrap_or(CommandError::CleanupTimedOut);
    let mut boundary = child.finish_boundary(deadline, before_reap, leader, unproven_error);
    if faults.termination.withholds_proof() {
        boundary = Err(CommandError::CleanupTimedOut);
    }
    let boundary = faults.boundary.inject_final(boundary);
    let terminal = child.restore_terminal(deadline);
    let readers = readers.map(|(stdout, stderr)| finish_pipe_pair(stdout, stderr, deadline));
    let termination = child.resolve_termination(
        termination,
        faults.termination.error(),
        boundary.as_ref().ok().copied(),
        leader,
    );

    termination?;
    boundary?;
    terminal?;
    let status = reaped.status()?;
    let readers = match readers {
        Some((stdout, stderr)) => Some(CapturedPipes { stdout: stdout?, stderr: stderr? }),
        None => None,
    };
    Ok(Cleaned { status, readers })
}

enum Stopped {
    Complete,
    TimedOut,
    Cancelled,
    Monitor(CommandError),
    Reader(CommandError),
}

#[cfg(unix)]
#[path = "unix.rs"]
mod platform;

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(all(unix, test))]
use platform::OwnedChild;
use platform::{StartError, StartedChild, spawn_owned};

enum ReapOutcome {
    Complete(ExitStatus),
    ReapedAfterDeadline(CommandError),
    RetentionUncertain(CommandError),
    NotReaped(CommandError),
}

impl ReapOutcome {
    fn evidence(&self) -> LeaderEvidence {
        match self {
            Self::Complete(_) | Self::ReapedAfterDeadline(_) => LeaderEvidence::Reaped,
            Self::RetentionUncertain(_) => LeaderEvidence::RetentionUncertain,
            Self::NotReaped(_) => LeaderEvidence::Retained,
        }
    }

    fn error(&self) -> Option<CommandError> {
        match self {
            Self::Complete(_) => None,
            Self::ReapedAfterDeadline(error)
            | Self::RetentionUncertain(error)
            | Self::NotReaped(error) => Some(*error),
        }
    }

    fn status(self) -> Result<ExitStatus, CommandError> {
        match self {
            Self::Complete(status) => Ok(status),
            Self::ReapedAfterDeadline(error)
            | Self::RetentionUncertain(error)
            | Self::NotReaped(error) => Err(error),
        }
    }
}

fn reap_leader(child: &mut StartedChild, deadline: Deadline) -> ReapOutcome {
    let outcome = reap_child(child.child_mut(), deadline);
    let evidence = outcome.evidence();
    if evidence == LeaderEvidence::Reaped {
        // This must be the first operation after a successful reap. In
        // particular, Unix must never retain an armed signal-on-drop fallback
        // once the process-group ID can be recycled.
        child.disarm_drop_kill();
    }
    #[cfg(unix)]
    if evidence == LeaderEvidence::RetentionUncertain {
        // A failed Unix wait no longer proves that the numeric PID/PGID is
        // retained. Conservatively give up the signal-on-drop fallback rather
        // than risk signalling a recycled identity during unwinding.
        child.disarm_drop_kill();
    }
    outcome
}

fn reap_child(child: &mut Child, deadline: Deadline) -> ReapOutcome {
    loop {
        let status = match deadline.retry_interrupted(|| child.try_wait()) {
            Ok(status) => status,
            Err(error) => return ReapOutcome::NotReaped(error),
        };
        if matches!(&status, Ok(Some(_))) {
            if let Err(error) = deadline.check_completion(DeadlineFault::CleanupReap) {
                return ReapOutcome::ReapedAfterDeadline(error);
            }
        } else if let Err(error) = deadline.check() {
            return ReapOutcome::NotReaped(error);
        }
        match status {
            Ok(Some(status)) => return ReapOutcome::Complete(status),
            Ok(None) => thread::sleep(POLL_INTERVAL.min(deadline.remaining())),
            Err(error) => {
                return ReapOutcome::RetentionUncertain(io_error(IoStage::Reap, &error));
            }
        }
    }
}

enum ChildPipe {
    Stdout(ChildStdout),
    Stderr(ChildStderr),
    #[cfg(test)]
    Memory(std::io::Cursor<Vec<u8>>),
}

impl ChildPipe {
    fn read_into(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Stdout(pipe) => pipe.read(buffer),
            Self::Stderr(pipe) => pipe.read(buffer),
            #[cfg(test)]
            Self::Memory(pipe) => pipe.read(buffer),
        }
    }
}

enum PipeRead {
    Pending,
    Data(usize),
    Eof,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PipeProgress {
    Idle,
    Progress,
}

struct PipeReader {
    pipe: Option<ChildPipe>,
    capture: PipeCapture,
    retained_limit: usize,
    retention: Retention,
    overflow_is_error: bool,
    fail_immediately: bool,
}

#[derive(Clone, Copy)]
enum Retention {
    Prefix,
    Suffix,
}

impl PipeReader {
    fn start(
        pipe: ChildPipe,
        retained_limit: usize,
        retention: Retention,
        overflow_is_error: bool,
        fail_immediately: bool,
        deadline: Deadline,
    ) -> Result<Self, CommandError> {
        deadline
            .retry_interrupted(|| platform::prepare_pipe(&pipe))?
            .map_err(|error| io_error(IoStage::StartOutputReader, &error))?;
        Ok(Self {
            pipe: Some(pipe),
            capture: PipeCapture::default(),
            retained_limit,
            retention,
            overflow_is_error,
            fail_immediately,
        })
    }

    /// Reads at most one fixed quantum so stdout, stderr, cancellation, the
    /// deadline, and leader state are all observed fairly.
    fn poll(
        &mut self,
        deadline: Deadline,
        completion_fault: DeadlineFault,
    ) -> Result<PipeProgress, CommandError> {
        let Some(pipe) = self.pipe.as_mut() else {
            return Ok(PipeProgress::Idle);
        };
        if self.fail_immediately {
            self.fail_immediately = false;
            self.pipe.take();
            deadline.check_completion(completion_fault)?;
            return Err(CommandError::Io {
                stage: IoStage::ReadOutput,
                kind: io::ErrorKind::Other,
            });
        }

        let mut buffer = [0; PIPE_BUFFER_SIZE];
        let observation =
            match deadline.retry_interrupted(|| platform::read_pipe(pipe, &mut buffer)) {
                Ok(observation) => observation,
                Err(error) => {
                    self.pipe.take();
                    return Err(error);
                }
            };
        let observation = match observation {
            Ok(observation) => observation,
            Err(error) => {
                self.pipe.take();
                deadline.check_completion(completion_fault)?;
                return Err(io_error(IoStage::ReadOutput, &error));
            }
        };
        match observation {
            PipeRead::Pending => {
                if let Err(error) = deadline.check() {
                    self.pipe.take();
                    Err(error)
                } else {
                    Ok(PipeProgress::Idle)
                }
            }
            PipeRead::Data(read) => {
                self.capture.record(&buffer[..read], self.retained_limit, self.retention);
                if self.overflow_is_error && self.capture.overflowed {
                    self.pipe.take();
                    deadline.check_completion(completion_fault)?;
                    return Err(CommandError::StdoutTooLarge { limit: self.retained_limit });
                }
                if let Err(error) = deadline.check() {
                    self.pipe.take();
                    Err(error)
                } else {
                    Ok(PipeProgress::Progress)
                }
            }
            PipeRead::Eof => {
                self.pipe.take();
                deadline.check_completion(completion_fault)?;
                Ok(PipeProgress::Progress)
            }
        }
    }

    fn is_finished(&self) -> bool {
        self.pipe.is_none()
    }

    #[cfg(test)]
    fn finish(mut self, deadline: Deadline) -> Result<PipeCapture, CommandError> {
        let mut result = self.is_finished().then_some(Ok(()));
        while result.is_none() {
            if let Err(error) = deadline.check() {
                self.pipe.take();
                result = Some(Err(error));
                break;
            }
            let progress = finish_pipe_step(&mut self, &mut result, deadline);
            if progress == PipeProgress::Idle {
                thread::sleep(POLL_INTERVAL.min(deadline.remaining()));
            }
        }
        result.expect("pipe finish result is missing").map(|()| self.capture)
    }
}

fn finish_pipe_pair(
    mut stdout: PipeReader,
    mut stderr: PipeReader,
    deadline: Deadline,
) -> (Result<PipeCapture, CommandError>, Result<PipeCapture, CommandError>) {
    let mut stdout_result = stdout.is_finished().then_some(Ok(()));
    let mut stderr_result = stderr.is_finished().then_some(Ok(()));
    let mut stdout_first = true;
    while stdout_result.is_none() || stderr_result.is_none() {
        if let Err(error) = deadline.check() {
            expire_pipe(&mut stdout, &mut stdout_result, error);
            expire_pipe(&mut stderr, &mut stderr_result, error);
            break;
        }
        let made_progress = if stdout_first {
            let first = finish_pipe_step(&mut stdout, &mut stdout_result, deadline);
            if let Err(error) = deadline.check() {
                expire_pipe(&mut stdout, &mut stdout_result, error);
                expire_pipe(&mut stderr, &mut stderr_result, error);
                break;
            }
            let second = finish_pipe_step(&mut stderr, &mut stderr_result, deadline);
            first == PipeProgress::Progress || second == PipeProgress::Progress
        } else {
            let first = finish_pipe_step(&mut stderr, &mut stderr_result, deadline);
            if let Err(error) = deadline.check() {
                expire_pipe(&mut stdout, &mut stdout_result, error);
                expire_pipe(&mut stderr, &mut stderr_result, error);
                break;
            }
            let second = finish_pipe_step(&mut stdout, &mut stdout_result, deadline);
            first == PipeProgress::Progress || second == PipeProgress::Progress
        };
        stdout_first = !stdout_first;
        if !made_progress {
            thread::sleep(POLL_INTERVAL.min(deadline.remaining()));
        }
    }
    (
        stdout_result.expect("stdout finish result is missing").map(|()| stdout.capture),
        stderr_result.expect("stderr finish result is missing").map(|()| stderr.capture),
    )
}

fn finish_pipe_step(
    reader: &mut PipeReader,
    result: &mut Option<Result<(), CommandError>>,
    deadline: Deadline,
) -> PipeProgress {
    if result.is_some() {
        return PipeProgress::Idle;
    }
    match reader.poll(deadline, DeadlineFault::CleanupPipeReader) {
        Ok(progress) => {
            if reader.is_finished() {
                *result = Some(Ok(()));
            }
            progress
        }
        Err(error) => {
            *result = Some(Err(error));
            PipeProgress::Progress
        }
    }
}

fn expire_pipe(
    reader: &mut PipeReader,
    result: &mut Option<Result<(), CommandError>>,
    error: CommandError,
) {
    if result.is_none() {
        reader.pipe.take();
        *result = Some(Err(error));
    }
}

#[derive(Default)]
struct PipeCapture {
    retained: Vec<u8>,
    total_bytes: u64,
    overflowed: bool,
}

impl PipeCapture {
    fn record(&mut self, bytes: &[u8], retained_limit: usize, retention: Retention) {
        self.total_bytes =
            self.total_bytes.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        match retention {
            Retention::Prefix => {
                let remaining = retained_limit.saturating_sub(self.retained.len());
                let retained = remaining.min(bytes.len());
                self.retained.extend_from_slice(&bytes[..retained]);
                self.overflowed |= retained != bytes.len();
            }
            Retention::Suffix => {
                let combined = self.retained.len().saturating_add(bytes.len());
                self.overflowed |= combined > retained_limit;
                if bytes.len() >= retained_limit {
                    self.retained.clear();
                    self.retained.extend_from_slice(&bytes[bytes.len() - retained_limit..]);
                } else {
                    let discard = combined.saturating_sub(retained_limit);
                    self.retained.drain(..discard);
                    self.retained.extend_from_slice(bytes);
                }
            }
        }
    }
}

fn io_error(stage: IoStage, error: &io::Error) -> CommandError {
    CommandError::Io { stage, kind: error.kind() }
}

#[cfg(test)]
mod tests;
