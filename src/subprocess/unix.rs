//! Unix process-boundary ownership and evidence.
//!
//! A successful spawn creates a new process group and immediately arms a
//! best-effort Drop fallback. Normal cleanup signals the stored original group
//! first and then the exact retained leader, so a leader that moved groups is
//! killed without signalling its new group. Reaping (or loss of Unix retention
//! proof) disarms numeric signalling before PID/PGID reuse can become possible.
//! Armed Drop also makes a bounded reap attempt after a successful exact-leader
//! kill. It reuses an active cleanup deadline and otherwise creates one fixed
//! interval for pre-cleanup unwinding. After a signal error it makes only one
//! nonblocking semantic observation, retrying interruptions within that same
//! bound, so Drop cannot wait forever on a live child it failed to kill.

use std::{
    fs::File,
    os::unix::{io::AsRawFd as _, process::CommandExt as _},
    sync::{Mutex, MutexGuard, TryLockError},
};

use super::*;

pub(super) struct OwnedChild {
    child: Child,
    process_group: libc::pid_t,
    foreground: Option<ForegroundLease>,
    drop_kill_armed: bool,
    cleanup_deadline: Option<Instant>,
}

pub(super) enum StartedChild {
    Owned(OwnedChild),
}

pub(super) enum StartError {
    BeforeSpawn(io::Error),
    BeforeSpawnCommand(CommandError),
    AfterSpawn { child: StartedChild, error: CommandError },
}

pub(super) struct Termination {
    group: Result<(), CommandError>,
    leader: Result<(), CommandError>,
}

static TERMINAL_FOREGROUND_LEASE: Mutex<()> = Mutex::new(());

pub(super) fn spawn_owned(
    command: &mut Command,
    deadline: Deadline,
    cancelled: &AtomicBool,
) -> Result<OwnedChild, StartError> {
    let foreground = ForegroundCandidate::acquire(deadline, cancelled)
        .map_err(StartError::BeforeSpawnCommand)?;
    command.process_group(0);
    let child = command.spawn().map_err(StartError::BeforeSpawn)?;
    let process_group = child.id() as libc::pid_t;
    let mut child = OwnedChild {
        child,
        process_group,
        foreground: None,
        drop_kill_armed: true,
        cleanup_deadline: None,
    };
    if let Some(foreground) = foreground {
        match foreground.transfer_to(process_group) {
            Ok(foreground) => child.foreground = Some(foreground),
            Err(error) => {
                let mut started = StartedChild::Owned(child);
                // A command can finish between spawn and the foreground
                // hand-off. Its vanished process group needs no terminal
                // lease; preserve that ordinary exit status instead of
                // misclassifying it as a startup failure.
                if matches!(started.leader_exited(deadline), Ok(true)) {
                    let StartedChild::Owned(child) = started;
                    return Ok(child);
                }
                return Err(StartError::AfterSpawn {
                    child: started,
                    error: io_error(IoStage::Start, &error),
                });
            }
        }
    }
    Ok(child)
}

/// A serialized opportunity to hand the controlling terminal to one child.
///
/// `/dev/tty` addresses the process's controlling terminal even though every
/// supervised child's standard input is intentionally null. Git, SSH, and
/// credential helpers can open that terminal themselves for an ordinary
/// interactive prompt. If this process is already a background job, the
/// caller has no terminal authority to lend and normal Unix job control is
/// left unchanged.
struct ForegroundCandidate {
    terminal: File,
    caller_group: libc::pid_t,
    serialization: MutexGuard<'static, ()>,
}

impl ForegroundCandidate {
    fn acquire(deadline: Deadline, cancelled: &AtomicBool) -> Result<Option<Self>, CommandError> {
        let terminal = match File::options().read(true).write(true).open("/dev/tty") {
            Ok(terminal) => terminal,
            // A command without a controlling terminal remains fully usable
            // for noninteractive authentication. Failure to open this
            // optional interface is not a command-startup failure.
            Err(_) => return Ok(None),
        };
        let serialization = loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(CommandError::CleanupTimedOut);
            }
            deadline.check()?;
            match TERMINAL_FOREGROUND_LEASE.try_lock() {
                Ok(serialization) => break serialization,
                Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    thread::sleep(POLL_INTERVAL.min(deadline.remaining()));
                }
            }
        };
        // SAFETY: getpgrp has no preconditions and cannot fail.
        let caller_group = unsafe { libc::getpgrp() };
        let foreground = retry_interrupted_until(deadline.at, || {
            // SAFETY: `terminal` is a live descriptor for `/dev/tty`.
            let group = unsafe { libc::tcgetpgrp(terminal.as_raw_fd()) };
            if group == -1 { Err(io::Error::last_os_error()) } else { Ok(group) }
        });
        let foreground = match foreground {
            Some(Ok(foreground)) => foreground,
            Some(Err(error)) if error.raw_os_error() == Some(libc::ENOTTY) => return Ok(None),
            Some(Err(error)) => return Err(io_error(IoStage::Start, &error)),
            None => return Err(CommandError::TimedOut),
        };
        if foreground != caller_group {
            return Ok(None);
        }
        Ok(Some(Self { terminal, caller_group, serialization }))
    }

    fn transfer_to(self, child_group: libc::pid_t) -> io::Result<ForegroundLease> {
        set_terminal_foreground(self.terminal.as_raw_fd(), child_group)?;
        // The child can attempt a terminal read in the small interval between
        // spawn and the foreground hand-off. Continue the whole group after
        // the hand-off so a resulting SIGTTIN stop cannot become a timeout.
        // ESRCH only means that a very short command already exited.
        // SAFETY: the positive ID is the exact retained child's original
        // process group and the leader has not been reaped.
        let continued = unsafe { libc::kill(-child_group, libc::SIGCONT) };
        if continued == -1 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            let error = io::Error::last_os_error();
            // The terminal has already moved. Retain the lease so its Drop
            // restores the caller even though startup cannot safely continue.
            let mut lease = ForegroundLease {
                terminal: self.terminal,
                caller_group: self.caller_group,
                serialization: Some(self.serialization),
                active: true,
            };
            let _ = lease.restore_until(Instant::now() + CLEANUP_TIMEOUT);
            return Err(error);
        }
        Ok(ForegroundLease {
            terminal: self.terminal,
            caller_group: self.caller_group,
            serialization: Some(self.serialization),
            active: true,
        })
    }
}

/// Exclusive ownership of the controlling terminal's foreground slot.
///
/// The caller's process group remains live for the lease's whole lifetime, so
/// its numeric identity cannot be recycled. Explicit cleanup reports a failed
/// restoration; Drop retries best-effort so panic and cancellation cannot
/// knowingly leave the supervised group in the foreground.
struct ForegroundLease {
    terminal: File,
    caller_group: libc::pid_t,
    serialization: Option<MutexGuard<'static, ()>>,
    active: bool,
}

impl ForegroundLease {
    fn restore_until(&mut self, deadline: Instant) -> Result<(), CommandError> {
        if !self.active {
            return Ok(());
        }
        match retry_interrupted_until(deadline, || {
            set_terminal_foreground(self.terminal.as_raw_fd(), self.caller_group)
        }) {
            Some(Ok(())) => {
                self.active = false;
                // Release serialization as soon as restoration is complete;
                // the file may remain owned until the process boundary drops.
                drop(self.serialization.take());
                Ok(())
            }
            Some(Err(error)) => Err(io_error(IoStage::RestoreTerminal, &error)),
            None => Err(CommandError::CleanupTimedOut),
        }
    }
}

impl Drop for ForegroundLease {
    fn drop(&mut self) {
        if self.active {
            let _ = self.restore_until(Instant::now());
        }
    }
}

/// Changes foreground ownership without allowing SIGTTOU to suspend the
/// worker which is reclaiming its terminal from a foreground child.
fn set_terminal_foreground(descriptor: libc::c_int, group: libc::pid_t) -> io::Result<()> {
    let _signal_mask = BlockedSignal::new(libc::SIGTTOU)?;
    // SAFETY: descriptor is a live controlling-terminal handle, and `group`
    // names a process group in the terminal's session.
    if unsafe { libc::tcsetpgrp(descriptor, group) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct BlockedSignal(libc::sigset_t);

impl BlockedSignal {
    fn new(signal: libc::c_int) -> io::Result<Self> {
        // SAFETY: both sets are correctly sized exclusive stack values. The
        // signal is a valid constant, and pthread_sigmask writes the previous
        // calling-thread mask to `previous`.
        unsafe {
            let mut blocked = std::mem::zeroed::<libc::sigset_t>();
            let mut previous = std::mem::zeroed::<libc::sigset_t>();
            if libc::sigemptyset(&mut blocked) == -1 || libc::sigaddset(&mut blocked, signal) == -1
            {
                return Err(io::Error::last_os_error());
            }
            let error = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous);
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            Ok(Self(previous))
        }
    }
}

impl Drop for BlockedSignal {
    fn drop(&mut self) {
        // SAFETY: `.0` is exactly the complete mask returned by the successful
        // pthread_sigmask call in `new` for this same worker thread.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut()) };
    }
}

pub(super) fn prepare_pipe(pipe: &ChildPipe) -> io::Result<()> {
    let descriptor = match pipe {
        ChildPipe::Stdout(pipe) => pipe.as_raw_fd(),
        ChildPipe::Stderr(pipe) => pipe.as_raw_fd(),
        #[cfg(test)]
        ChildPipe::Memory(_) => return Ok(()),
    };
    // SAFETY: descriptor is the live read end owned by `pipe`. F_GETFL
    // observes its complete current status flags so F_SETFL preserves every
    // existing flag while adding only O_NONBLOCK.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK == 0 {
        // SAFETY: same live descriptor; the argument preserves F_GETFL's
        // flags and adds the nonblocking status required by the worker loop.
        if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(super) fn read_pipe(pipe: &mut ChildPipe, buffer: &mut [u8]) -> io::Result<PipeRead> {
    match pipe.read_into(buffer) {
        Ok(0) => Ok(PipeRead::Eof),
        Ok(read) => Ok(PipeRead::Data(read)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(PipeRead::Pending),
        Err(error) => Err(error),
    }
}

impl StartedChild {
    pub(super) fn child_mut(&mut self) -> &mut Child {
        let Self::Owned(child) = self;
        &mut child.child
    }

    pub(super) fn leader_exited(&mut self, deadline: Deadline) -> Result<bool, CommandError> {
        let process_id = self.child_mut().id();
        // WNOWAIT is essential: the waitable leader keeps its PID, which
        // is also this command's process-group ID, from being reused until
        // GHerrit has signalled the exact group.
        deadline
            .retry_interrupted(|| {
                let mut information = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
                // SAFETY: `information` is a correctly sized exclusive output
                // buffer. The positive ID came from this live `Child`, and
                // WNOWAIT reports completion without releasing that ID for
                // reuse.
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        process_id as libc::id_t,
                        &mut information,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                if result == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(unsafe { information.si_pid() } != 0)
                }
            })?
            .map_err(|error| io_error(IoStage::Monitor, &error))
    }

    pub(super) fn begin_cleanup(&mut self, deadline: Deadline) {
        let Self::Owned(child) = self;
        child.cleanup_deadline = Some(deadline.at);
    }

    pub(super) fn restore_terminal(&mut self, deadline: Deadline) -> Result<(), CommandError> {
        let Self::Owned(child) = self;
        child.foreground.as_mut().map_or(Ok(()), |foreground| foreground.restore_until(deadline.at))
    }

    pub(super) fn terminate(&mut self) -> Termination {
        let Self::Owned(child) = self;
        // SAFETY: process_group is the retained leader's exact positive
        // PID. Negating it addresses that process group. The leader is not
        // reaped until after this call, so the PGID cannot name an
        // unrelated group here.
        let group_result = unsafe { libc::kill(-child.process_group, libc::SIGKILL) };
        let group = if group_result == 0 {
            Ok(())
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(io_error(IoStage::Terminate, &error))
            }
        };
        // A process-group leader may move itself into another existing
        // group in the same session. Signal the stored original group
        // first, then the exact retained leader, without ever signalling
        // the leader's new group.
        let leader = child.child.kill().map_err(|error| io_error(IoStage::Terminate, &error));
        Termination { group, leader }
    }

    pub(super) fn wait_empty(
        &mut self,
        deadline: Deadline,
    ) -> Result<BoundaryEvidence, CommandError> {
        // The exact PGID remains retained until the leader reap. Absence
        // can be observed without a reuse ambiguity only after that reap.
        deadline.check().map(|()| BoundaryEvidence::Unproven)
    }

    pub(super) fn finish_boundary(
        &mut self,
        deadline: Deadline,
        _before_reap: BoundaryEvidence,
        leader: LeaderEvidence,
        unproven_error: CommandError,
    ) -> Result<BoundaryProof, CommandError> {
        if leader != LeaderEvidence::Reaped {
            return Err(unproven_error);
        }
        let Self::Owned(child) = self;
        loop {
            deadline.check()?;
            // SAFETY: signal zero changes no process. If the just-reaped
            // PGID was recycled, this observes the unrelated group and
            // conservatively waits until the cleanup deadline; it never
            // sends a signal to that group.
            let result = unsafe { libc::kill(-child.process_group, 0) };
            if result == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return deadline.check().map(|()| BoundaryProof::Empty);
                }
                if error.raw_os_error() != Some(libc::EPERM) {
                    return Err(io_error(IoStage::ObserveBoundary, &error));
                }
            }
            thread::sleep(POLL_INTERVAL.min(deadline.remaining()));
        }
    }

    pub(super) fn resolve_termination(
        &mut self,
        mut termination: Termination,
        injected: Option<CommandError>,
        boundary: Option<BoundaryProof>,
        leader: LeaderEvidence,
    ) -> Result<(), CommandError> {
        if let Some(error) = injected {
            termination.group = Err(error);
            termination.leader = Err(error);
        }
        if boundary == Some(BoundaryProof::Empty) {
            termination.group = Ok(());
        }
        if leader == LeaderEvidence::Reaped {
            termination.leader = Ok(());
        }
        termination.group?;
        termination.leader
    }

    pub(super) fn disarm_drop_kill(&mut self) {
        let Self::Owned(child) = self;
        child.drop_kill_armed = false;
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let now = Instant::now();
        let deadline = drop_reap_deadline(self.cleanup_deadline, now);
        if self.drop_kill_armed {
            // SAFETY: while the leader remains waitable, its positive PID
            // retains the original process-group ID. Signal that group before
            // the exact leader, because the leader may have moved to another
            // group which must not be signalled. Drop has no error channel.
            unsafe { libc::kill(-self.process_group, libc::SIGKILL) };
            if self.child.kill().is_ok() {
                loop {
                    match retry_interrupted_until(deadline, || self.child.try_wait()) {
                        Some(Ok(Some(_))) | Some(Err(_)) | None => break,
                        Some(Ok(None)) if Instant::now() < deadline => {
                            thread::sleep(
                                POLL_INTERVAL
                                    .min(deadline.saturating_duration_since(Instant::now())),
                            );
                        }
                        Some(Ok(None)) => break,
                    }
                }
            } else {
                // The group signal may already have ended the leader. Reap
                // only when the exact retained Child reports completion
                // immediately; never block after failure to signal it.
                let _ = retry_interrupted_until(deadline, || self.child.try_wait());
            }
        }
        if let Some(foreground) = &mut self.foreground {
            let _ = foreground.restore_until(deadline);
        }
    }
}

fn drop_reap_deadline(active_cleanup: Option<Instant>, now: Instant) -> Instant {
    active_cleanup.unwrap_or_else(|| {
        now.checked_add(CLEANUP_TIMEOUT).expect("the fixed cleanup interval must fit in Instant")
    })
}

/// Returns the first non-interrupted result, or `None` once the existing Drop
/// bound no longer permits retrying an interrupted attempt. The operation is
/// always attempted once, even when `deadline` has already elapsed.
fn retry_interrupted_until<T>(
    deadline: Instant,
    mut operation: impl FnMut() -> io::Result<T>,
) -> Option<io::Result<T>> {
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                if Instant::now() >= deadline {
                    return None;
                }
            }
            result => return Some(result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_reuses_an_expired_active_cleanup_deadline() {
        let now = Instant::now();
        let expired = now.checked_sub(Duration::from_secs(1)).unwrap();

        assert_eq!(drop_reap_deadline(Some(expired), now), expired);
        assert_eq!(drop_reap_deadline(None, now), now.checked_add(CLEANUP_TIMEOUT).unwrap());
    }

    #[test]
    fn interrupted_drop_observations_are_retried_only_within_the_bound() {
        let mut attempts = 0;
        let result = retry_interrupted_until(Instant::now() + Duration::from_secs(1), || {
            attempts += 1;
            if attempts < 3 { Err(io::Error::from(io::ErrorKind::Interrupted)) } else { Ok(7) }
        });

        assert_eq!(result.unwrap().unwrap(), 7);
        assert_eq!(attempts, 3);

        let mut expired_attempts = 0;
        let expired = retry_interrupted_until(Instant::now(), || {
            expired_attempts += 1;
            Err::<(), _>(io::Error::from(io::ErrorKind::Interrupted))
        });
        assert!(expired.is_none());
        assert_eq!(expired_attempts, 1);
    }
}
