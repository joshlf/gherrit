//! Cross-platform subprocess fixtures and contract tests.
//!
//! Re-execution is a test-only adapter; every fixture clears its environment
//! and restores only documented bootstrap data.
//! Exact descendant lifetime is witnessed with bounded loopback TCP connections,
//! not FIFOs, so EOF is an identity-stable process-exit observation on all targets.

use std::{
    env, fs,
    io::{Read as _, Seek as _, Write as _},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process,
    time::{Duration, Instant},
};
#[cfg(unix)]
use std::{
    os::unix::process::ExitStatusExt,
    panic::{AssertUnwindSafe, catch_unwind},
};

use super::*;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const REEXEC_MODE: &str = "GHERRIT_SUBPROCESS_TEST_MODE";
const REEXEC_BYTES: &str = "GHERRIT_SUBPROCESS_TEST_BYTES";
const REEXEC_MARKER: &str = "GHERRIT_SUBPROCESS_TEST_MARKER";
const REEXEC_SECRET: &str = "GHERRIT_SUBPROCESS_TEST_SECRET";
const REEXEC_LIFETIME: &str = "GHERRIT_SUBPROCESS_TEST_LIFETIME";
const REEXEC_READY: &str = "GHERRIT_SUBPROCESS_TEST_READY";
#[cfg(unix)]
const REEXEC_TARGET_PROCESS_GROUP: &str = "GHERRIT_SUBPROCESS_TEST_TARGET_PROCESS_GROUP";
const REEXEC_TEST: &str = "pre_push::subprocess::tests::reexec_helper";

fn reexec(mode: &str) -> Command {
    let mut command = Command::new(env::current_exe().unwrap());
    // Re-exec fixtures are a final, test-only adapter boundary. They need
    // only their explicit mode and per-fixture bootstrap values; ambient
    // credentials, proxy settings, Git configuration, and a stale test
    // mode must never cross this boundary.
    clear_environment(&mut command);
    command.args(["--exact", REEXEC_TEST, "--nocapture"]).env(REEXEC_MODE, mode);
    command
}

fn clear_environment(command: &mut Command) {
    command.env_clear();
    #[cfg(windows)]
    if let Some(system_root) = env::var_os("SystemRoot") {
        // Windows child bootstrap and system runtime lookup require the
        // canonical system root. WINDIR is its legacy system alias; neither
        // value carries user, network, Git, proxy, or credential state.
        command.env("SystemRoot", &system_root).env("WINDIR", system_root);
    }
}

#[cfg(unix)]
fn shell(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    clear_environment(&mut command);
    command.arg("-c").arg(script);
    command
}

#[test]
fn reexec_helper() {
    let Ok(mode) = env::var(REEXEC_MODE) else {
        return;
    };
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
        "stdout-sized" => {
            std::io::stdout().write_all(&vec![b'o'; reexec_bytes()]).unwrap();
        }
        "stdout-overflow" => {
            std::io::stdout().write_all(&vec![b'o'; reexec_bytes()]).unwrap();
            std::io::stdout().flush().unwrap();
            thread::sleep(Duration::from_secs(30));
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
        "copy-stdin" => {
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input).unwrap();
            std::io::stdout().write_all(&input).unwrap();
        }
        "read-only-stdin" => {
            let mut input = borrowed_standard_input();
            assert!(input.write_all(b"corrupt").is_err());
            input.rewind().unwrap();
            let mut bytes = Vec::new();
            input.read_to_end(&mut bytes).unwrap();
            std::io::stdout().write_all(&bytes).unwrap();
        }
        "clean-environment" => {
            for (name, _) in env::vars_os() {
                let allowed = name == REEXEC_MODE;
                #[cfg(target_os = "macos")]
                // Core Foundation synthesizes this locale bootstrap after
                // process creation even when the supplied environment block
                // was otherwise empty.
                let allowed = allowed || name == "__CF_USER_TEXT_ENCODING";
                #[cfg(windows)]
                let allowed = allowed || name == "SystemRoot" || name == "WINDIR";
                assert!(allowed, "fixture inherited unexpected environment variable {name:?}");
            }
        }
        "leader-waits" => {
            let mut descendant = reexec("sleep").spawn().unwrap();
            descendant.wait().unwrap();
        }
        "leader-exits" => {
            reexec("sleep").spawn().unwrap();
            process::exit(23);
        }
        "leader-waits-probed" => {
            close_output_streams();
            let mut descendant = reexec("probe-sleep");
            descendant
                .env(REEXEC_LIFETIME, env::var_os(REEXEC_LIFETIME).unwrap())
                .env(REEXEC_READY, env::var_os(REEXEC_READY).unwrap())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let descendant = descendant.spawn().unwrap();
            drop(descendant);
            wait_for_marker_blocking(Path::new(&env::var_os(REEXEC_READY).unwrap()));
            fs::write(env::var_os(REEXEC_MARKER).unwrap(), b"ready").unwrap();
            thread::sleep(Duration::from_secs(30));
        }
        "leader-exits-probed" => {
            close_output_streams();
            let mut descendant = reexec("probe-sleep");
            descendant
                .env(REEXEC_LIFETIME, env::var_os(REEXEC_LIFETIME).unwrap())
                .env(REEXEC_READY, env::var_os(REEXEC_READY).unwrap())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let descendant = descendant.spawn().unwrap();
            drop(descendant);
            wait_for_marker_blocking(Path::new(&env::var_os(REEXEC_READY).unwrap()));
            process::exit(29);
        }
        "sleep" => thread::sleep(Duration::from_secs(10)),
        "stdout-closes-then-waits" => {
            close_stdout();
            thread::sleep(Duration::from_millis(150));
        }
        "probe-sleep" => {
            close_output_streams();
            #[cfg(unix)]
            if let Ok(process_group) = env::var(REEXEC_TARGET_PROCESS_GROUP) {
                let process_group = process_group.parse::<libc::pid_t>().unwrap();
                // SAFETY: this fixture changes only its own process group,
                // joining the same-session group supplied by its parent.
                assert_eq!(
                    unsafe { libc::setpgid(0, process_group) },
                    0,
                    "failed to move fixture into process group: {}",
                    io::Error::last_os_error()
                );
            }
            let address: SocketAddr = env::var(REEXEC_LIFETIME).unwrap().parse().unwrap();
            let mut lifetime = TcpStream::connect(address).unwrap();
            lifetime.write_all(b"ready\n").unwrap();
            let mut acknowledgement = [0; 4];
            lifetime.read_exact(&mut acknowledgement).unwrap();
            assert_eq!(&acknowledgement, b"ack\n");
            fs::write(env::var_os(REEXEC_READY).unwrap(), b"ready").unwrap();
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

fn copy_stdin_output(bytes: &[u8]) -> process::Output {
    let mut input = tempfile::tempfile().unwrap();
    input.write_all(bytes).unwrap();
    input.rewind().unwrap();
    reexec("copy-stdin").stdin(Stdio::from(input)).output().unwrap()
}

fn borrowed_standard_input() -> std::mem::ManuallyDrop<fs::File> {
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd as _;

        // SAFETY: the fixture borrows its process-owned standard input. The
        // `ManuallyDrop` wrapper prevents `File` from closing that descriptor.
        std::mem::ManuallyDrop::new(unsafe { fs::File::from_raw_fd(libc::STDIN_FILENO) })
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{FromRawHandle as _, RawHandle};

        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::Console::{GetStdHandle, STD_INPUT_HANDLE},
        };

        // SAFETY: the fixture borrows its process-owned standard-input handle.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        assert!(!handle.is_null() && handle != INVALID_HANDLE_VALUE);
        // SAFETY: the handle is live and `ManuallyDrop` prevents `File` from
        // taking responsibility for closing the borrowed standard handle.
        std::mem::ManuallyDrop::new(unsafe { fs::File::from_raw_handle(handle as RawHandle) })
    }
}

fn wait_for_marker_blocking(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if matches!(fs::read(path).as_deref(), Ok(b"ready")) {
            return;
        }
        assert!(Instant::now() < deadline, "descendant did not publish readiness");
        thread::sleep(POLL_INTERVAL);
    }
}

fn close_output_streams() {
    close_stdout();
    close_stderr();
}

#[cfg(unix)]
fn close_stdout() {
    close_file_descriptor(libc::STDOUT_FILENO);
}

#[cfg(unix)]
fn close_stderr() {
    close_file_descriptor(libc::STDERR_FILENO);
}

#[cfg(unix)]
fn close_file_descriptor(descriptor: libc::c_int) {
    // SAFETY: each re-exec fixture owns this standard descriptor and calls
    // this helper at most once before exiting without further output.
    assert_eq!(unsafe { libc::close(descriptor) }, 0);
}

#[cfg(windows)]
fn close_stdout() {
    use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;

    close_standard_handle(STD_OUTPUT_HANDLE);
}

#[cfg(windows)]
fn close_stderr() {
    use windows_sys::Win32::System::Console::STD_ERROR_HANDLE;

    close_standard_handle(STD_ERROR_HANDLE);
}

#[cfg(windows)]
fn close_standard_handle(kind: windows_sys::Win32::System::Console::STD_HANDLE) {
    use std::ptr;

    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Console::{GetStdHandle, SetStdHandle},
    };

    // SAFETY: the fixture replaces its own process-wide standard handle
    // before closing the previously owned handle exactly once.
    let handle = unsafe { GetStdHandle(kind) };
    assert_ne!(unsafe { SetStdHandle(kind, ptr::null_mut()) }, 0);
    if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
        assert_ne!(unsafe { CloseHandle(handle) }, 0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn runs_a_platform_native_process() {
    let mut command = Command::new(env::current_exe().unwrap());
    clear_environment(&mut command);
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

#[tokio::test(flavor = "current_thread")]
async fn retains_a_bounded_diagnostic_only_after_normal_completion() {
    let bytes = 37;
    let mut command = reexec("stderr-only");
    command.env(REEXEC_BYTES, bytes.to_string());

    let output = output(command, TEST_TIMEOUT).await.unwrap();

    assert_eq!(output.stderr(), &vec![b'e'; bytes]);
    assert_eq!(output.stderr_bytes(), bytes as u64);
    assert!(!output.stderr_truncated());
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
    assert_eq!(output.stderr(), &vec![b'e'; STDERR_RETAIN_LIMIT]);
    assert_eq!(output.stderr_bytes(), bytes as u64);
    assert!(output.stderr_truncated());
}

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
async fn supplies_immediate_eof_from_an_empty_regular_file() {
    let input = RegularFileStdinBuilder::new().unwrap().finish().unwrap();

    let output =
        output_with_regular_file_stdin(reexec("null-stdin"), input, TEST_TIMEOUT).await.unwrap();

    assert!(output.status().success());
}

#[tokio::test(flavor = "current_thread")]
async fn regular_file_stdin_is_rewound_and_can_exceed_pipe_capacity() {
    let bytes = vec![b'i'; 1024 * 1024];
    let mut input = RegularFileStdinBuilder::new().unwrap();
    input.write_all(&bytes).unwrap();
    let input = input.finish().unwrap();
    let expected = copy_stdin_output(&bytes);

    let output =
        output_with_regular_file_stdin(reexec("copy-stdin"), input, TEST_TIMEOUT).await.unwrap();

    assert!(output.status().success());
    assert_eq!(output.stdout(), expected.stdout);
}

#[tokio::test(flavor = "current_thread")]
async fn regular_file_stdin_accepts_the_exact_limit() {
    let bytes = b"12345678";
    let mut input = RegularFileStdinBuilder::with_limit(bytes.len() as u64).unwrap();
    input.write_all(bytes).unwrap();

    let output =
        output_with_regular_file_stdin(reexec("copy-stdin"), input.finish().unwrap(), TEST_TIMEOUT)
            .await
            .unwrap();

    assert!(output.status().success());
    assert_eq!(output.stdout(), copy_stdin_output(bytes).stdout);
}

#[tokio::test(flavor = "current_thread")]
async fn regular_file_stdin_rejects_overflow_without_writing_it() {
    let bytes = b"12345678";
    let mut input = RegularFileStdinBuilder::with_limit(bytes.len() as u64).unwrap();
    input.write_all(bytes).unwrap();

    let error = input.write_all(b"9").unwrap_err();
    assert!(error.to_string().contains("8-byte limit"));

    let output =
        output_with_regular_file_stdin(reexec("copy-stdin"), input.finish().unwrap(), TEST_TIMEOUT)
            .await
            .unwrap();
    assert_eq!(output.stdout(), copy_stdin_output(bytes).stdout);
}

#[test]
fn regular_file_stdin_finish_rejects_a_changed_file() {
    let mut input = RegularFileStdinBuilder::with_limit(8).unwrap();
    input.write_all(b"input").unwrap();
    input.file.as_file().set_len(0).unwrap();

    let error = match input.finish() {
        Ok(_) => panic!("accepted a regular-file input whose size changed"),
        Err(error) => error,
    };
    assert_eq!(error, CommandError::StdinChanged);
}

#[test]
fn finishing_regular_file_stdin_removes_its_name_and_closes_gherrits_writer() {
    let mut input = RegularFileStdinBuilder::with_limit(8).unwrap();
    input.write_all(b"input").unwrap();
    let path = input.file.path().to_owned();
    let mut input = input.finish().unwrap();

    assert!(!path.exists());
    assert!(input.file.write_all(b"corrupt").is_err());
    input.file.rewind().unwrap();
    let mut bytes = Vec::new();
    input.file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"input");
}

#[tokio::test(flavor = "current_thread")]
async fn child_cannot_write_to_regular_file_stdin() {
    let bytes = b"bounded input";
    let mut input = RegularFileStdinBuilder::new().unwrap();
    input.write_all(bytes).unwrap();

    let output = output_with_regular_file_stdin(
        reexec("read-only-stdin"),
        input.finish().unwrap(),
        TEST_TIMEOUT,
    )
    .await
    .unwrap();

    assert!(output.status().success());
    assert_eq!(output.stdout(), copy_stdin_output(bytes).stdout);
}

#[tokio::test(flavor = "current_thread")]
async fn reexec_environment_contains_only_documented_bootstrap_values() {
    let output = output(reexec("clean-environment"), TEST_TIMEOUT).await.unwrap();

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

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn timeout_kills_the_exact_leader_after_it_moves_process_groups() {
    let directory = tempfile::tempdir().unwrap();
    let lifetime = ProcessProbe::start();
    let ready = directory.path().join("leader-ready");
    let mut command = reexec("probe-sleep");
    command
        .env(REEXEC_LIFETIME, lifetime.address().to_string())
        .env(REEXEC_READY, &ready)
        // SAFETY: getpgrp has no preconditions and returns this test
        // process's same-session group, which the fixture may join.
        .env(REEXEC_TARGET_PROCESS_GROUP, unsafe { libc::getpgrp() }.to_string());
    let task = tokio::spawn(output(command, Duration::from_millis(250)));
    wait_for_marker(&ready).await;

    let error = task.await.unwrap().unwrap_err();

    assert_eq!(error, CommandError::TimedOut);
    lifetime.wait_closed();
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

#[tokio::test(flavor = "current_thread")]
async fn leader_exit_terminates_a_descendant_that_closed_both_pipes() {
    let directory = tempfile::tempdir().unwrap();
    let lifetime = ProcessProbe::start();
    let ready_path = directory.path().join("descendant-ready");
    let mut command = reexec("leader-exits-probed");
    command.env(REEXEC_LIFETIME, lifetime.address().to_string()).env(REEXEC_READY, &ready_path);

    let output = output(command, TEST_TIMEOUT).await.unwrap();

    assert_eq!(output.status().code(), Some(29));
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

#[tokio::test(flavor = "current_thread")]
async fn zero_timeout_with_regular_stdin_does_not_start_the_command() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("started");
    let mut command = reexec("marker");
    command.env(REEXEC_MARKER, &marker);
    let mut input = RegularFileStdinBuilder::new().unwrap();
    input.write_all(b"bounded input").unwrap();

    let error = output_with_regular_file_stdin(command, input.finish().unwrap(), Duration::ZERO)
        .await
        .unwrap_err();

    assert_eq!(error, CommandError::TimedOut);
    assert!(!marker.exists());
}

#[cfg(windows)]
#[tokio::test(flavor = "current_thread")]
async fn job_configuration_failure_does_not_start_the_command() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("started");
    let mut command = reexec("marker");
    command.env(REEXEC_MARKER, &marker);
    let (faults, spawned, _proceed) = windows_fault(PlatformFaultStage::ConfigureJob);

    let error = output_with_faults(command, TEST_TIMEOUT, REMOTE_GIT_STDOUT_LIMIT, faults)
        .await
        .unwrap_err();

    assert_eq!(error, CommandError::Io { stage: IoStage::Start, kind: io::ErrorKind::Other });
    assert!(spawned.try_recv().is_err());
    assert!(!marker.exists());
}

#[cfg(windows)]
#[tokio::test(flavor = "current_thread")]
async fn failure_before_assignment_cleans_up_the_exact_child() {
    assert_windows_startup_cleanup(PlatformFaultStage::BeforeAssignment).await;
}

#[cfg(windows)]
#[tokio::test(flavor = "current_thread")]
async fn injected_failure_before_assign_call_cleans_up_the_exact_child() {
    assert_windows_startup_cleanup(PlatformFaultStage::BeforeAssignCall).await;
}

#[cfg(windows)]
#[tokio::test(flavor = "current_thread")]
async fn thread_lookup_failure_empties_the_already_owned_job() {
    assert_windows_startup_cleanup(PlatformFaultStage::ThreadLookup).await;
}

#[cfg(windows)]
#[tokio::test(flavor = "current_thread")]
async fn failure_before_resume_empties_the_owned_job() {
    assert_windows_startup_cleanup(PlatformFaultStage::BeforeResume).await;
}

#[cfg(windows)]
#[tokio::test(flavor = "current_thread")]
async fn resume_failure_empties_the_owned_job() {
    assert_windows_startup_cleanup(PlatformFaultStage::Resume).await;
}

#[cfg(windows)]
fn windows_fault(stage: PlatformFaultStage) -> (Faults, mpsc::Receiver<u32>, mpsc::Sender<()>) {
    windows_startup_barrier(stage, true)
}

#[cfg(windows)]
fn windows_startup_barrier(
    stage: PlatformFaultStage,
    fail: bool,
) -> (Faults, mpsc::Receiver<u32>, mpsc::Sender<()>) {
    let (spawned_sender, spawned) = mpsc::channel();
    let (proceed, proceed_receiver) = mpsc::channel();
    let platform =
        PlatformFault::Inject { stage, fail, spawned: spawned_sender, proceed: proceed_receiver };
    (Faults { platform, ..Faults::NONE }, spawned, proceed)
}

#[cfg(windows)]
async fn assert_windows_startup_cleanup(stage: PlatformFaultStage) {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("started");
    let mut command = reexec("marker");
    command.env(REEXEC_MARKER, &marker);
    let (faults, spawned, proceed) = windows_fault(stage);
    let task =
        tokio::spawn(output_with_faults(command, TEST_TIMEOUT, REMOTE_GIT_STDOUT_LIMIT, faults));
    let process_id =
        tokio::task::spawn_blocking(move || spawned.recv_timeout(Duration::from_secs(2)).unwrap())
            .await
            .unwrap();
    let process = ObservedProcess::open(process_id);
    proceed.send(()).unwrap();

    let error = task.await.unwrap().unwrap_err();

    assert_eq!(error, CommandError::Io { stage: IoStage::Start, kind: io::ErrorKind::Other });
    process.assert_exited();
    assert!(!marker.exists(), "the suspended startup fault ran the command");
}

#[cfg(windows)]
#[tokio::test(flavor = "current_thread")]
async fn startup_timeout_immediately_before_resume_never_runs_the_child() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("started");
    let mut command = reexec("marker");
    command.env(REEXEC_MARKER, &marker);
    let (faults, spawned, proceed) =
        windows_startup_barrier(PlatformFaultStage::BeforeResume, false);
    let task = tokio::spawn(output_with_faults(
        command,
        Duration::from_millis(50),
        REMOTE_GIT_STDOUT_LIMIT,
        faults,
    ));
    let process_id =
        tokio::task::spawn_blocking(move || spawned.recv_timeout(Duration::from_secs(2)).unwrap())
            .await
            .unwrap();
    let process = ObservedProcess::open(process_id);
    tokio::time::sleep(Duration::from_millis(100)).await;
    proceed.send(()).unwrap();

    let error = task.await.unwrap().unwrap_err();

    assert_eq!(error, CommandError::TimedOut);
    process.assert_exited();
    assert!(!marker.exists(), "the timed-out suspended child was resumed");
}

#[cfg(windows)]
#[tokio::test(flavor = "current_thread")]
async fn startup_cancellation_immediately_before_resume_never_runs_the_child() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("started");
    let mut command = reexec("marker");
    command.env(REEXEC_MARKER, &marker);
    let (faults, spawned, proceed) =
        windows_startup_barrier(PlatformFaultStage::BeforeResume, false);
    let task =
        tokio::spawn(output_with_faults(command, TEST_TIMEOUT, REMOTE_GIT_STDOUT_LIMIT, faults));
    let process_id =
        tokio::task::spawn_blocking(move || spawned.recv_timeout(Duration::from_secs(2)).unwrap())
            .await
            .unwrap();
    let process = ObservedProcess::open(process_id);

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    proceed.send(()).unwrap();

    process.assert_exited();
    assert!(!marker.exists(), "the canceled suspended child was resumed");
}

#[cfg(windows)]
struct ObservedProcess(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl ObservedProcess {
    fn open(process_id: u32) -> Self {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};

        // SAFETY: process_id was observed directly from the newly spawned
        // suspended Child, and this requests only wait permission.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
        assert!(!handle.is_null(), "failed to retain exact child process");
        Self(handle)
    }

    fn assert_exited(&self) {
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0, System::Threading::WaitForSingleObject,
        };

        // The retained handle cannot be recycled to another process. A
        // signaled handle therefore independently proves that the exact
        // observed child exited during the bounded cleanup attempt.
        assert_eq!(unsafe { WaitForSingleObject(self.0, 2_000) }, WAIT_OBJECT_0);
    }
}

#[cfg(windows)]
impl Drop for ObservedProcess {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        // SAFETY: this test owns the valid OpenProcess handle.
        unsafe { CloseHandle(self.0) };
    }
}

#[tokio::test(flavor = "current_thread")]
async fn leader_exit_observed_at_the_execution_deadline_fails() {
    let faults = Faults { deadline: DeadlineFault::ExecutionLeaderExit, ..Faults::NONE };

    let error =
        output_with_faults(reexec("nonzero"), TEST_TIMEOUT, REMOTE_GIT_STDOUT_LIMIT, faults)
            .await
            .unwrap_err();

    assert_eq!(error, CommandError::TimedOut);
}

#[tokio::test(flavor = "current_thread")]
async fn reap_observed_at_the_cleanup_deadline_fails() {
    let faults = Faults { deadline: DeadlineFault::CleanupReap, ..Faults::NONE };

    let error =
        output_with_faults(reexec("nonzero"), TEST_TIMEOUT, REMOTE_GIT_STDOUT_LIMIT, faults)
            .await
            .unwrap_err();

    assert_eq!(error, CommandError::CleanupTimedOut);
}

#[tokio::test(flavor = "current_thread")]
async fn termination_failure_is_resolved_by_exact_empty_boundary_evidence() {
    let faults = Faults { termination: TerminationFault::FailureAfterCleanup, ..Faults::NONE };

    let output =
        output_with_faults(reexec("nonzero"), TEST_TIMEOUT, REMOTE_GIT_STDOUT_LIMIT, faults)
            .await
            .unwrap();

    assert_eq!(output.status().code(), Some(23));
}

#[tokio::test(flavor = "current_thread")]
async fn termination_failure_without_boundary_proof_is_returned() {
    let faults = Faults { termination: TerminationFault::FailureWithoutProof, ..Faults::NONE };

    let error =
        output_with_faults(reexec("nonzero"), TEST_TIMEOUT, REMOTE_GIT_STDOUT_LIMIT, faults)
            .await
            .unwrap_err();

    assert_eq!(
        error,
        CommandError::Io { stage: IoStage::Terminate, kind: io::ErrorKind::PermissionDenied }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn boundary_observation_failure_is_returned_after_cleanup() {
    let faults = Faults { boundary: BoundaryFault::Failure, ..Faults::NONE };

    let error = output_with_faults(
        reexec("sleep"),
        Duration::from_millis(50),
        REMOTE_GIT_STDOUT_LIMIT,
        faults,
    )
    .await
    .unwrap_err();

    assert_eq!(
        error,
        CommandError::Io { stage: IoStage::ObserveBoundary, kind: io::ErrorKind::Other }
    );
}

#[cfg(windows)]
#[tokio::test(flavor = "current_thread")]
async fn later_job_empty_proof_supersedes_a_transient_first_observation_failure() {
    let faults = Faults { boundary: BoundaryFault::FirstObservationFailure, ..Faults::NONE };

    let output =
        output_with_faults(reexec("nonzero"), TEST_TIMEOUT, REMOTE_GIT_STDOUT_LIMIT, faults)
            .await
            .unwrap();

    assert_eq!(output.status().code(), Some(23));
}

#[tokio::test(flavor = "current_thread")]
async fn stdout_reader_startup_failure_cleans_up_the_spawned_child() {
    let faults = Faults { reader: ReaderFault::StartStdout, ..Faults::NONE };
    let started = Instant::now();

    let error = output_with_faults(reexec("sleep"), TEST_TIMEOUT, REMOTE_GIT_STDOUT_LIMIT, faults)
        .await
        .unwrap_err();

    assert_eq!(
        error,
        CommandError::Io { stage: IoStage::StartOutputReader, kind: io::ErrorKind::Other }
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test(flavor = "current_thread")]
async fn stderr_reader_startup_failure_discards_stdout_and_reaps_the_child() {
    let secret = "partial-stdout-must-not-escape-startup-failure";
    let mut command = reexec("private-overflow");
    command.env(REEXEC_SECRET, secret);
    let faults = Faults { reader: ReaderFault::StartStderr, ..Faults::NONE };
    let started = Instant::now();

    let error = output_with_faults(command, TEST_TIMEOUT, REMOTE_GIT_STDOUT_LIMIT, faults)
        .await
        .unwrap_err();

    assert_eq!(
        error,
        CommandError::Io { stage: IoStage::StartOutputReader, kind: io::ErrorKind::Other }
    );
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{error:?}").contains(secret));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn pipe_poll_observed_at_the_execution_deadline_fails() {
    let deadline =
        Deadline::execution(Instant::now() + TEST_TIMEOUT, DeadlineFault::ExecutionPipeReader);
    let mut reader = PipeReader::start(
        ChildPipe::Memory(std::io::Cursor::new(b"complete".to_vec())),
        8,
        Retention::Prefix,
        true,
        false,
        deadline,
    )
    .unwrap();
    let test_deadline = Instant::now() + TEST_TIMEOUT;

    loop {
        match reader.poll(deadline, DeadlineFault::ExecutionPipeReader) {
            Err(error) => {
                assert_eq!(error, CommandError::TimedOut);
                break;
            }
            Ok(_) => {
                assert!(Instant::now() < test_deadline, "pipe reader did not finish");
                thread::yield_now();
            }
        }
    }
}

#[test]
fn pipe_finish_observed_at_the_cleanup_deadline_fails() {
    let now = Instant::now();
    let deadline = Deadline::cleanup(now, now + TEST_TIMEOUT, DeadlineFault::CleanupPipeReader);
    let reader = PipeReader::start(
        ChildPipe::Memory(std::io::Cursor::new(b"complete".to_vec())),
        8,
        Retention::Prefix,
        true,
        false,
        deadline,
    )
    .unwrap();

    let error = match reader.finish(deadline) {
        Ok(_) => panic!("accepted pipe completion at the deadline"),
        Err(error) => error,
    };

    assert_eq!(error, CommandError::CleanupTimedOut);
}

#[tokio::test(flavor = "current_thread")]
async fn aborting_the_future_terminates_the_owned_process_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let lifetime = ProcessProbe::start();
    let ready_path = directory.path().join("descendant-ready");
    let marker = directory.path().join("leader-ready");
    let mut command = reexec("leader-waits-probed");
    command
        .env(REEXEC_LIFETIME, lifetime.address().to_string())
        .env(REEXEC_READY, &ready_path)
        .env(REEXEC_MARKER, &marker);
    let task = tokio::spawn(output(command, Duration::from_secs(30)));
    wait_for_marker(&marker).await;

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    lifetime.wait_closed();
}

#[tokio::test(flavor = "current_thread")]
async fn aborting_a_regular_stdin_command_preserves_cleanup_semantics() {
    let directory = tempfile::tempdir().unwrap();
    let lifetime = ProcessProbe::start();
    let ready_path = directory.path().join("descendant-ready");
    let marker = directory.path().join("leader-ready");
    let mut command = reexec("leader-waits-probed");
    command
        .env(REEXEC_LIFETIME, lifetime.address().to_string())
        .env(REEXEC_READY, &ready_path)
        .env(REEXEC_MARKER, &marker);
    let mut input = RegularFileStdinBuilder::new().unwrap();
    input.write_all(b"bounded input").unwrap();
    let task = tokio::spawn(output_with_regular_file_stdin(
        command,
        input.finish().unwrap(),
        Duration::from_secs(30),
    ));
    wait_for_marker(&marker).await;

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    lifetime.wait_closed();
}

#[cfg(unix)]
#[test]
fn unwind_drop_kills_the_owned_process_group() {
    let directory = tempfile::tempdir().unwrap();
    let lifetime = ProcessProbe::start();
    let descendant_ready = directory.path().join("descendant-ready");
    let leader_ready = directory.path().join("leader-ready");
    let mut command = reexec("leader-waits-probed");
    command
        .env(REEXEC_LIFETIME, lifetime.address().to_string())
        .env(REEXEC_READY, &descendant_ready)
        .env(REEXEC_MARKER, &leader_ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let owned = match spawn_owned(&mut command) {
        Ok(child) => child,
        Err(StartError::BeforeSpawn(error)) => panic!("failed to spawn fixture: {error}"),
    };
    let mut child = StartedChild::Owned(owned);
    let leader = child.child_mut().id() as libc::pid_t;
    wait_for_marker_blocking(&leader_ready);

    let panicked = catch_unwind(AssertUnwindSafe(move || {
        let _child = child;
        panic!("injected unwind after owned spawn");
    }));

    assert!(panicked.is_err());
    lifetime.wait_closed();
    assert_unix_fixture_reaped(leader);
}

#[cfg(unix)]
#[test]
fn unwind_drop_kills_the_exact_leader_after_it_moves_process_groups() {
    let directory = tempfile::tempdir().unwrap();
    let lifetime = ProcessProbe::start();
    let ready = directory.path().join("leader-ready");
    let mut command = reexec("probe-sleep");
    command
        .env(REEXEC_LIFETIME, lifetime.address().to_string())
        .env(REEXEC_READY, &ready)
        // SAFETY: getpgrp has no preconditions and returns this test
        // process's same-session group, which the fixture may join.
        .env(REEXEC_TARGET_PROCESS_GROUP, unsafe { libc::getpgrp() }.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let owned = match spawn_owned(&mut command) {
        Ok(child) => child,
        Err(StartError::BeforeSpawn(error)) => panic!("failed to spawn fixture: {error}"),
    };
    let mut child = StartedChild::Owned(owned);
    let leader = child.child_mut().id() as libc::pid_t;
    wait_for_marker_blocking(&ready);

    let panicked = catch_unwind(AssertUnwindSafe(move || {
        let _child = child;
        panic!("injected unwind after process-group escape");
    }));

    assert!(panicked.is_err());
    lifetime.wait_closed();
    assert_unix_fixture_reaped(leader);
}

#[cfg(unix)]
fn assert_unix_fixture_reaped(process_id: libc::pid_t) {
    let mut status = 0;
    // SAFETY: process_id identifies the exact retained leader created by this
    // test. WNOHANG cannot block and reports whether Drop left a zombie.
    let result = unsafe { libc::waitpid(process_id, &mut status, libc::WNOHANG) };
    assert_eq!(result, -1, "Drop left the exact leader waitable (waitpid returned {result})");
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
}

#[tokio::test(flavor = "current_thread")]
async fn reader_failure_stops_and_cleans_up_immediately() {
    let started = Instant::now();
    let error = output_with_injected_stdout_failure(reexec("leader-waits"), TEST_TIMEOUT)
        .await
        .unwrap_err();

    assert_eq!(error, CommandError::Io { stage: IoStage::ReadOutput, kind: io::ErrorKind::Other });
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "reader failure did not start immediate cleanup: {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn stderr_reader_failure_in_the_second_poll_slot_discards_partial_output() {
    let secret = "partial-output-must-not-escape-stderr-read-failure";
    let mut command = reexec("private-overflow");
    command.env(REEXEC_SECRET, secret);
    let faults = Faults { reader: ReaderFault::ReadStderr, ..Faults::NONE };
    let started = Instant::now();

    let error = output_with_faults(command, TEST_TIMEOUT, REMOTE_GIT_STDOUT_LIMIT, faults)
        .await
        .unwrap_err();

    assert_eq!(error, CommandError::Io { stage: IoStage::ReadOutput, kind: io::ErrorKind::Other });
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{error:?}").contains(secret));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "stderr reader failure did not start immediate cleanup: {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn stdout_at_the_exact_limit_succeeds() {
    let bytes = 64 * 1024;
    let mut expected_command = reexec("stdout-sized");
    expected_command.env(REEXEC_BYTES, bytes.to_string());
    let expected = expected_command.output().unwrap();
    let limit = expected.stdout.len();
    let mut command = reexec("stdout-sized");
    command.env(REEXEC_BYTES, bytes.to_string());

    let output = output_with_stdout_limit(command, TEST_TIMEOUT, limit).await.unwrap();

    assert!(output.status().success());
    assert_eq!(output.stdout(), expected.stdout);
}

#[tokio::test(flavor = "current_thread")]
async fn stdout_overflow_stops_execution_and_enters_cleanup_immediately() {
    let bytes = 64 * 1024;
    let mut exact_command = reexec("stdout-sized");
    exact_command.env(REEXEC_BYTES, bytes.to_string());
    let limit = exact_command.output().unwrap().stdout.len();
    let mut command = reexec("stdout-overflow");
    command.env(REEXEC_BYTES, (bytes + 1).to_string());
    let started = Instant::now();

    let error = output_with_stdout_limit(command, TEST_TIMEOUT, limit).await.unwrap_err();

    assert_eq!(error, CommandError::StdoutTooLarge { limit });
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "stdout overflow did not start immediate cleanup: {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn excessive_stderr_is_drained_with_a_bounded_suffix() {
    let bytes = 2 * 1024 * 1024;
    let mut command = reexec("stderr-only");
    command.env(REEXEC_BYTES, bytes.to_string());

    let output = output(command, TEST_TIMEOUT).await.unwrap();

    assert!(output.status().success());
    assert_eq!(output.stderr(), &vec![b'e'; STDERR_RETAIN_LIMIT]);
    assert_eq!(output.stderr_bytes(), bytes as u64);
    assert!(output.stderr_truncated());
    assert!(!format!("{output:?}").contains(&"e".repeat(128)));
}

#[test]
fn byte_counts_saturate_instead_of_overflowing() {
    let mut capture = PipeCapture { total_bytes: u64::MAX - 1, ..PipeCapture::default() };

    capture.record(b"abcd", 0, Retention::Prefix);

    assert_eq!(capture.total_bytes, u64::MAX);
}

#[test]
fn pipe_capture_preserves_the_exact_prefix_at_its_limit() {
    let mut capture = PipeCapture::default();

    capture.record(&[0, 128], 3, Retention::Prefix);
    capture.record(&[255, 1], 3, Retention::Prefix);

    assert_eq!(capture.retained, [0, 128, 255]);
    assert_eq!(capture.total_bytes, 4);
    assert!(capture.overflowed);
}

#[test]
fn pipe_capture_preserves_the_exact_suffix_at_its_limit() {
    let mut capture = PipeCapture::default();

    capture.record(&[0, 1], 3, Retention::Suffix);
    capture.record(&[2, 3], 3, Retention::Suffix);
    capture.record(&[4, 5, 6, 7], 3, Retention::Suffix);

    assert_eq!(capture.retained, [5, 6, 7]);
    assert_eq!(capture.total_bytes, 8);
    assert!(capture.overflowed);
}

#[test]
fn cleanup_deadline_uses_the_earlier_local_or_supervisor_bound() {
    let execution_started = Instant::now();
    let execution_deadline = execution_started + Duration::from_secs(120);
    let cleanup_started = execution_started + Duration::from_secs(7);

    let actual = Deadline::cleanup(cleanup_started, execution_deadline, DeadlineFault::None);

    assert_eq!(actual.at, cleanup_started + CLEANUP_TIMEOUT);
    assert_eq!(actual.timeout_error, CommandError::CleanupTimedOut);

    let delayed_cleanup = execution_deadline + Duration::from_secs(1);
    let actual = Deadline::cleanup(delayed_cleanup, execution_deadline, DeadlineFault::None);
    assert_eq!(actual.at, execution_deadline + CLEANUP_TIMEOUT);
}

#[test]
fn interrupted_io_observations_retry_without_losing_retention_evidence() {
    let mut attempts = 0;
    let deadline = Deadline::execution(Instant::now() + TEST_TIMEOUT, DeadlineFault::None);

    let result = deadline
        .retry_interrupted(|| {
            attempts += 1;
            if attempts < 3 { Err(io::Error::from(io::ErrorKind::Interrupted)) } else { Ok(11) }
        })
        .unwrap()
        .unwrap();

    assert_eq!(result, 11);
    assert_eq!(attempts, 3);
}

#[test]
fn remote_git_execution_timeout_is_two_minutes() {
    assert_eq!(REMOTE_GIT_EXECUTION_TIMEOUT, Duration::from_secs(120));
}

#[tokio::test(flavor = "current_thread")]
async fn errors_do_not_reveal_command_contents() {
    let secret = "secret-destination-that-must-not-appear";
    let mut command = Command::new(format!("/definitely/missing/{secret}"));
    clear_environment(&mut command);

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

struct ProcessProbe {
    address: SocketAddr,
    observer: thread::JoinHandle<io::Result<()>>,
}

impl ProcessProbe {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let observer = thread::spawn(move || observe_process_lifetime(listener));
        Self { address, observer }
    }

    fn address(&self) -> SocketAddr {
        self.address
    }

    fn wait_closed(self) {
        self.observer.join().expect("process-probe observer panicked").unwrap();
    }
}

fn observe_process_lifetime(listener: TcpListener) -> io::Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(2);

    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + TIMEOUT;
    let mut connection = loop {
        match listener.accept() {
            Ok((connection, _)) => break connection,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(
                    POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process probe was not connected before its deadline",
                ));
            }
            Err(error) => return Err(error),
        }
    };
    connection.set_nonblocking(false)?;
    connection.set_read_timeout(Some(TIMEOUT))?;
    connection.set_write_timeout(Some(TIMEOUT))?;

    // The descendant does not publish its readiness marker until this
    // handshake completes. Cleanup therefore cannot discard unread greeting
    // bytes before this observer has identified the connection.
    let mut ready = [0; 6];
    connection.read_exact(&mut ready)?;
    if &ready != b"ready\n" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process probe sent an invalid readiness greeting",
        ));
    }
    connection.write_all(b"ack\n")?;

    // This accepted connection is an identity-stable lifetime token: only the
    // sleeping descendant owns its peer. Terminal closure therefore proves
    // that exact descendant exited without consulting a recyclable PID, PGID,
    // or process handle.
    let mut unexpected = [0];
    match connection.read(&mut unexpected) {
        Ok(0) => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process probe contained unexpected lifetime data",
        )),
        Err(error) if is_terminal_probe_close(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn is_terminal_probe_close(error: &io::Error) -> bool {
    #[cfg(windows)]
    {
        matches!(error.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted)
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}
