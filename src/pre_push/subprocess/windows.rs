//! Windows process-boundary ownership and evidence.
//!
//! The child is created suspended behind an exact-process Drop guard, then
//! assigned immediately to a configured kill-on-close job. Only after ownership
//! transfers to the job may thread snapshot/open operations run. Cancellation
//! and deadline checks are adjacent to the sole ResumeThread call. Direct-child
//! handles and job accounting provide identity-stable cleanup evidence.

use std::{
    mem,
    os::windows::{io::AsRawHandle as _, process::CommandExt as _},
    ptr,
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_NO_MORE_FILES,
        ERROR_PIPE_NOT_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
    },
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        Pipes::PeekNamedPipe,
        Threading::{
            CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME, TerminateProcess,
        },
    },
};

use super::*;

pub(super) fn prepare_pipe(_pipe: &ChildPipe) -> io::Result<()> {
    Ok(())
}

pub(super) fn read_pipe(pipe: &mut ChildPipe, buffer: &mut [u8]) -> io::Result<PipeRead> {
    let handle = match pipe {
        ChildPipe::Stdout(pipe) => pipe.as_raw_handle() as HANDLE,
        ChildPipe::Stderr(pipe) => pipe.as_raw_handle() as HANDLE,
        #[cfg(test)]
        ChildPipe::Memory(_) => {
            return pipe
                .read_into(buffer)
                .map(|read| if read == 0 { PipeRead::Eof } else { PipeRead::Data(read) });
        }
    };
    let mut available = 0;
    // SAFETY: `handle` is the live read end of the anonymous child pipe.
    // Null data buffers request only an availability observation, and
    // `available` is a correctly sized exclusive output parameter.
    if unsafe {
        PeekNamedPipe(
            handle,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            &raw mut available,
            ptr::null_mut(),
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        return if pipe_eof(&error) { Ok(PipeRead::Eof) } else { Err(error) };
    }
    if available == 0 {
        return Ok(PipeRead::Pending);
    }
    let available = usize::try_from(available).unwrap_or(usize::MAX).min(buffer.len());
    match pipe.read_into(&mut buffer[..available]) {
        Ok(0) => Ok(PipeRead::Eof),
        Ok(read) => Ok(PipeRead::Data(read)),
        Err(error) if pipe_eof(&error) => Ok(PipeRead::Eof),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(PipeRead::Pending),
        Err(error) => Err(error),
    }
}

fn pipe_eof(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_BROKEN_PIPE as i32
                || code == ERROR_NO_DATA as i32
                || code == ERROR_PIPE_NOT_CONNECTED as i32
    )
}

struct Handle(HANDLE);

impl Handle {
    fn from_nullable(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() { Err(io::Error::last_os_error()) } else { Ok(Self(handle)) }
    }

    fn from_snapshot(handle: HANDLE) -> io::Result<Self> {
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this wrapper is constructed only for an owned, valid
        // Win32 handle and closes it exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

struct Job {
    handle: Handle,
}

impl Job {
    fn create(_fault: &PlatformFault) -> io::Result<Self> {
        // SAFETY: null security attributes and name request a new private
        // job object. The returned handle is immediately owned by RAII.
        let handle = Handle::from_nullable(unsafe { CreateJobObjectW(ptr::null(), ptr::null()) })?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        #[cfg(test)]
        if _fault.is(PlatformFaultStage::ConfigureJob) {
            return Err(PlatformFault::injected_error());
        }
        // SAFETY: `limits` has the exact structure and byte size required
        // by JobObjectExtendedLimitInformation and lives for the call.
        if unsafe {
            SetInformationJobObject(
                handle.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(mem::size_of_val(&limits)).expect("job limits fit in u32"),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    fn active_processes(&self) -> io::Result<u32> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: `accounting` is the exact writable structure requested
        // by JobObjectBasicAccountingInformation and lives for the call.
        if unsafe {
            QueryInformationJobObject(
                self.handle.0,
                JobObjectBasicAccountingInformation,
                (&raw mut accounting).cast(),
                u32::try_from(mem::size_of_val(&accounting)).expect("job accounting fits in u32"),
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(accounting.ActiveProcesses)
    }

    fn terminate(&self) -> Result<(), CommandError> {
        // SAFETY: this is GHerrit's live private job handle. The exit code
        // is intentionally not exposed as a command result.
        if unsafe { TerminateJobObject(self.handle.0, 1) } == 0 {
            let error = io::Error::last_os_error();
            Err(io_error(IoStage::Terminate, &error))
        } else {
            Ok(())
        }
    }

    fn wait_empty(&self, deadline: Deadline) -> Result<(), CommandError> {
        loop {
            deadline.check()?;
            let active = self
                .active_processes()
                .map_err(|error| io_error(IoStage::ObserveBoundary, &error))?;
            if active == 0 {
                // Do not accept an observation which completed after the
                // cleanup deadline.
                return deadline.check();
            }
            thread::sleep(POLL_INTERVAL.min(deadline.remaining()));
        }
    }
}

struct DirectChildGuard {
    process: HANDLE,
    armed: bool,
}

impl DirectChildGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DirectChildGuard {
    fn drop(&mut self) {
        if self.armed {
            // SAFETY: the direct-child guard is declared before `child`,
            // so this exact borrowed process handle remains live while the
            // guard drops. This is an unwind-only fallback; ordinary paths
            // use bounded cleanup and report its result.
            unsafe { TerminateProcess(self.process, 1) };
        }
    }
}

pub(super) struct DirectChild {
    // Field order is significant for the guard's borrowed handle.
    guard: DirectChildGuard,
    child: Child,
    // The configured kill-on-close job exists before spawn, but this
    // direct child has not yet been assigned to it.
    job: Job,
}

impl DirectChild {
    fn into_owned(mut self) -> OwnedChild {
        self.guard.disarm();
        let Self { guard, child, job } = self;
        drop(guard);
        OwnedChild { child, job }
    }
}

pub(super) struct OwnedChild {
    child: Child,
    job: Job,
}

pub(super) enum StartedChild {
    Direct(DirectChild),
    Owned(OwnedChild),
}

pub(super) enum StartError {
    BeforeSpawn(io::Error),
    AfterSpawn { child: StartedChild, error: CommandError },
}

pub(super) type Termination = Result<(), CommandError>;

pub(super) fn spawn_owned(
    command: &mut Command,
    deadline: Deadline,
    cancelled: &AtomicBool,
    fault: &PlatformFault,
) -> Result<OwnedChild, StartError> {
    let job = Job::create(fault).map_err(StartError::BeforeSpawn)?;
    command.creation_flags(CREATE_SUSPENDED);
    // The child is directly owned and unable to execute from the instant
    // spawn returns until it is assigned to the job. Every ordinary Rust
    // error, panic, timeout, and future cancellation therefore has a
    // direct kill guard. An abrupt termination of the entire GHerrit
    // process in this very small pre-assignment interval is the one state
    // the job cannot cover, because Windows has not assigned the child yet.
    let child = command.spawn().map_err(StartError::BeforeSpawn)?;
    let guard = DirectChildGuard { process: child.as_raw_handle() as HANDLE, armed: true };
    let direct = DirectChild { guard, child, job };
    if let Err(error) = fault.observe(PlatformFaultStage::BeforeAssignment, direct.child.id()) {
        return Err(after_spawn_io(StartedChild::Direct(direct), error));
    }
    #[cfg(test)]
    if fault.injects(PlatformFaultStage::BeforeAssignment) {
        return Err(StartError::AfterSpawn {
            child: StartedChild::Direct(direct),
            error: io_error(IoStage::Start, &PlatformFault::injected_error()),
        });
    }

    if let Err(error) = fault.observe(PlatformFaultStage::BeforeAssignCall, direct.child.id()) {
        return Err(after_spawn_io(StartedChild::Direct(direct), error));
    }
    #[cfg(test)]
    if fault.injects(PlatformFaultStage::BeforeAssignCall) {
        return Err(StartError::AfterSpawn {
            child: StartedChild::Direct(direct),
            error: io_error(IoStage::Start, &PlatformFault::injected_error()),
        });
    }
    // SAFETY: both handles are live and exclusively owned here. The child
    // is still suspended, so it cannot create descendants before joining
    // the private job.
    if unsafe {
        AssignProcessToJobObject(direct.job.handle.0, direct.child.as_raw_handle() as HANDLE)
    } == 0
    {
        return Err(after_spawn_io(StartedChild::Direct(direct), io::Error::last_os_error()));
    }

    // Install job ownership immediately after assignment. Every later
    // startup failure, including a ToolHelp snapshot or OpenThread error,
    // is therefore covered by kill-on-close job ownership and bounded job
    // observation.
    let owned = direct.into_owned();
    if let Err(error) = fault.observe(PlatformFaultStage::ThreadLookup, owned.child.id()) {
        return Err(after_spawn_io(StartedChild::Owned(owned), error));
    }
    #[cfg(test)]
    if fault.injects(PlatformFaultStage::ThreadLookup) {
        return Err(StartError::AfterSpawn {
            child: StartedChild::Owned(owned),
            error: io_error(IoStage::Start, &PlatformFault::injected_error()),
        });
    }
    let thread = match suspended_thread(owned.child.id()) {
        Ok(thread) => thread,
        Err(error) => return Err(after_spawn_io(StartedChild::Owned(owned), error)),
    };

    if let Err(error) = fault.observe(PlatformFaultStage::BeforeResume, owned.child.id()) {
        return Err(after_spawn_io(StartedChild::Owned(owned), error));
    }
    #[cfg(test)]
    if fault.injects(PlatformFaultStage::BeforeResume) {
        return Err(StartError::AfterSpawn {
            child: StartedChild::Owned(owned),
            error: io_error(IoStage::Start, &PlatformFault::injected_error()),
        });
    }

    if let Err(error) = fault.observe(PlatformFaultStage::Resume, owned.child.id()) {
        return Err(after_spawn_io(StartedChild::Owned(owned), error));
    }
    // SAFETY: `thread` belongs to this still-suspended child. A return of
    // one is the only state proving that this call resumed the one
    // CREATE_SUSPENDED suspension.
    #[cfg(test)]
    if fault.injects(PlatformFaultStage::Resume) {
        return Err(StartError::AfterSpawn {
            child: StartedChild::Owned(owned),
            error: io_error(IoStage::Start, &PlatformFault::injected_error()),
        });
    }

    // This adjacent check is the startup authorization point: cancellation or
    // expiry observed before it prevents resume. Cancellation which linearizes
    // after authorization is handled by normal monitoring and bounded cleanup.
    // Keep no fallible work between authorization and ResumeThread.
    if let Err(error) = startup_check(deadline, cancelled) {
        return Err(StartError::AfterSpawn { child: StartedChild::Owned(owned), error });
    }
    let previous = unsafe { ResumeThread(thread.0) };
    if previous != 1 {
        let error = if previous == u32::MAX {
            io::Error::last_os_error()
        } else {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "new child thread did not have exactly one suspension",
            )
        };
        return Err(after_spawn_io(StartedChild::Owned(owned), error));
    }
    Ok(owned)
}

fn startup_check(deadline: Deadline, cancelled: &AtomicBool) -> Result<(), CommandError> {
    if cancelled.load(Ordering::Acquire) {
        Err(CommandError::CleanupTimedOut)
    } else {
        deadline.check()
    }
}

fn after_spawn_io(child: StartedChild, error: io::Error) -> StartError {
    StartError::AfterSpawn { child, error: io_error(IoStage::Start, &error) }
}

fn suspended_thread(process_id: u32) -> io::Result<Handle> {
    // SAFETY: flags request a system thread snapshot and the process ID is
    // ignored for TH32CS_SNAPTHREAD.
    let snapshot =
        Handle::from_snapshot(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
    let mut entry = THREADENTRY32 {
        dwSize: u32::try_from(mem::size_of::<THREADENTRY32>()).expect("thread entry fits in u32"),
        ..THREADENTRY32::default()
    };
    // SAFETY: `entry` is correctly sized, writable, and lives throughout
    // enumeration of this owned snapshot.
    if unsafe { Thread32First(snapshot.0, &mut entry) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut matching_thread = None;
    loop {
        if entry.th32OwnerProcessID == process_id
            && matching_thread.replace(entry.th32ThreadID).is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "new suspended child had more than one thread",
            ));
        }
        // SAFETY: same live snapshot and output entry as above.
        if unsafe { Thread32Next(snapshot.0, &mut entry) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(error);
        }
    }
    let thread_id = matching_thread.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "suspended child thread was absent from the system snapshot",
        )
    })?;
    // The matching process is still suspended, so its sole initial thread
    // cannot exit and have this ID recycled between the snapshot and
    // OpenThread.
    // SAFETY: thread_id was just observed as owned by this exact suspended
    // process and requests only the right required to resume it.
    Handle::from_nullable(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) })
}

impl StartedChild {
    pub(super) fn child_mut(&mut self) -> &mut Child {
        match self {
            Self::Direct(child) => &mut child.child,
            Self::Owned(child) => &mut child.child,
        }
    }

    pub(super) fn leader_exited(&mut self, deadline: Deadline) -> Result<bool, CommandError> {
        deadline
            .retry_interrupted(|| self.child_mut().try_wait())?
            .map(|status| status.is_some())
            .map_err(|error| io_error(IoStage::Monitor, &error))
    }

    pub(super) fn begin_cleanup(&mut self, _deadline: Deadline) {
        // Closing the job/guard handles is nonblocking, so Windows Drop does
        // not need to retain the ordinary cleanup deadline.
    }

    pub(super) fn restore_terminal(&mut self, _deadline: Deadline) -> Result<(), CommandError> {
        Ok(())
    }

    pub(super) fn terminate(&mut self) -> Termination {
        match self {
            Self::Direct(child) => {
                child.child.kill().map_err(|error| io_error(IoStage::Terminate, &error))
            }
            Self::Owned(child) => child.job.terminate(),
        }
    }

    pub(super) fn wait_empty(
        &mut self,
        deadline: Deadline,
    ) -> Result<BoundaryEvidence, CommandError> {
        match self {
            // The direct child was created suspended and therefore could
            // not create descendants. Its exact handle is reaped below.
            Self::Direct(_) => deadline.check().map(|()| BoundaryEvidence::Unproven),
            Self::Owned(child) => child.job.wait_empty(deadline).map(|()| BoundaryEvidence::Empty),
        }
    }

    pub(super) fn finish_boundary(
        &mut self,
        deadline: Deadline,
        before_reap: BoundaryEvidence,
        leader: LeaderEvidence,
        unproven_error: CommandError,
    ) -> Result<BoundaryProof, CommandError> {
        match self {
            // The direct child never executed. Its exact signaled process
            // handle is the complete boundary.
            Self::Direct(_) if leader == LeaderEvidence::Reaped => {
                deadline.check().map(|()| BoundaryProof::Empty)
            }
            Self::Direct(_) => Err(unproven_error),
            // Reuse a positive pre-reap observation. If it failed or supplied
            // no proof, query the exact still-owned job again after reap; a
            // later Empty supersedes a transient first observation error.
            Self::Owned(_) if before_reap == BoundaryEvidence::Empty => {
                deadline.check().map(|()| BoundaryProof::Empty)
            }
            Self::Owned(child) => child.job.wait_empty(deadline).map(|()| BoundaryProof::Empty),
        }
    }

    pub(super) fn resolve_termination(
        &mut self,
        termination: Termination,
        injected: Option<CommandError>,
        boundary: Option<BoundaryProof>,
        leader: LeaderEvidence,
    ) -> Result<(), CommandError> {
        let termination = injected.map_or(termination, Err);
        match self {
            // A successful reap proves that the exact suspended direct
            // child is gone; it never ran and could not make descendants.
            Self::Direct(child) if leader == LeaderEvidence::Reaped => {
                child.guard.disarm();
                Ok(())
            }
            // ActiveProcesses == 0 is the authoritative proof that the
            // exact owned job is empty, even if TerminateJobObject raced
            // with natural completion and returned an error.
            Self::Owned(_) if boundary == Some(BoundaryProof::Empty) => Ok(()),
            _ => termination,
        }
    }

    pub(super) fn disarm_drop_kill(&mut self) {
        if let Self::Direct(child) = self {
            child.guard.disarm();
        }
    }
}
