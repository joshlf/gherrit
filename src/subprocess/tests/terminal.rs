//! Bounded PTY transcripts, exact supervisor completion and cleanup.

use std::io::{Read as _, Write as _};

use super::*;

pub(super) fn wait_for_foreground_terminal() -> File {
    let terminal = File::options().read(true).write(true).open("/dev/tty").unwrap();
    // SAFETY: getpgrp has no preconditions and cannot fail.
    let process_group = unsafe { libc::getpgrp() };
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: `terminal` is a live controlling-terminal descriptor.
        let foreground = unsafe { libc::tcgetpgrp(terminal.as_raw_fd()) };
        assert_ne!(foreground, -1, "failed to inspect terminal foreground ownership");
        if foreground == process_group {
            return terminal;
        }
        assert!(Instant::now() < deadline, "fixture never received terminal foreground ownership");
        thread::yield_now();
    }
}

fn assert_terminal_restored() {
    let mut terminal = wait_for_foreground_terminal();
    terminal.write_all(b"TERMINAL RESTORED\n").unwrap();
    terminal.flush().unwrap();
}

pub(super) fn terminal_supervisor(case: &str) {
    match case {
        "success" | "nonzero" => {
            let expected = if case == "success" { 0 } else { 23 };
            let mut command = reexec("terminal-prompt");
            command.env(REEXEC_BYTES, expected.to_string());
            let runtime =
                tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
            let output = runtime.block_on(output(command, TEST_TIMEOUT)).unwrap();
            assert_eq!(
                output.status().code(),
                Some(expected),
                "terminal child stderr: {}",
                String::from_utf8_lossy(output.stderr())
            );
        }
        "timeout" => {
            let runtime =
                tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
            let error = runtime
                .block_on(output(reexec("terminal-wait"), Duration::from_millis(100)))
                .unwrap_err();
            assert_eq!(error, CommandError::TimedOut);
        }
        "fast" => {
            let runtime =
                tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
            for _ in 0..32 {
                let output = runtime.block_on(output(reexec("nonzero"), TEST_TIMEOUT)).unwrap();
                assert_eq!(output.status().code(), Some(23));
            }
        }
        "drop" => {
            let directory = tempfile::tempdir().unwrap();
            let marker = directory.path().join("terminal-owned");
            let mut command = reexec("terminal-marker-wait");
            command
                .env(REEXEC_MARKER, &marker)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let child = spawn_owned_fixture(&mut command);
            wait_for_marker_blocking(&marker);
            drop(child);
        }
        "cancel" => {
            let directory = tempfile::tempdir().unwrap();
            let marker = directory.path().join("terminal-owned");
            let mut command = reexec("terminal-marker-wait");
            command.env(REEXEC_MARKER, &marker);
            let runtime =
                tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
            let task = runtime.spawn(output(command, Duration::from_secs(30)));
            runtime.block_on(wait_for_marker(&marker));
            task.abort();
            assert!(runtime.block_on(task).unwrap_err().is_cancelled());
        }
        _ => unreachable!("the fixture supplies one known terminal case"),
    }
    assert_terminal_restored();
}

pub(super) fn run_terminal_session(mode: &str, reply: bool, ready: &[u8]) {
    // This supervisor establishes its own session and controlling terminal.
    // Poll the transcript and exact supervisor under fixed deadlines; master
    // EOF is not a completion signal while any slave descriptor remains open.
    let (mut master, slave) = open_pseudoterminal();
    let mut command = reexec(mode);
    command
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    // SAFETY: only async-signal-safe session and terminal ioctls run between
    // fork and exec. Standard input has already been installed from `slave`.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            #[allow(clippy::useless_conversion)]
            let request = libc::TIOCSCTTY.into();
            if libc::ioctl(libc::STDIN_FILENO, request, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = KillOnDrop(Some(command.spawn().unwrap()));
    // Command retains its configured Stdio files for another spawn.
    drop(command);
    // Retain a parent slave through completion to exercise the Linux case
    // where EOF remains unavailable after the supervisor exits.

    let mut transcript = Vec::new();
    receive_terminal_text(&mut master, &mut transcript, ready);
    if reply {
        master.write_all(b"ok\n").unwrap();
        master.flush().unwrap();
    }
    receive_terminal_text(&mut master, &mut transcript, b"TERMINAL RESTORED");

    let status = wait_terminal_supervisor(child.0.as_mut().unwrap()).unwrap();
    assert!(status.success(), "terminal fixture failed with {status}: {transcript:?}");
    child.0 = None;
    drop(slave);
}

fn open_pseudoterminal() -> (File, File) {
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: both integers are exclusive writable outputs. Null termios,
    // window-size, and name pointers request the platform defaults.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(result, 0, "failed to open a pseudoterminal: {}", io::Error::last_os_error());
    // SAFETY: successful openpty returned two new uniquely owned descriptors.
    let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    // SAFETY: master is a live descriptor. Preserve its existing status flags
    // while making transcript reads nonblocking for the deadline below.
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert_ne!(flags, -1, "failed to inspect pseudoterminal flags");
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0,
        "failed to make pseudoterminal reads nonblocking"
    );
    (master, slave)
}

fn receive_terminal_text(master: &mut File, transcript: &mut Vec<u8>, expected: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buffer = [0; 1024];
    while !transcript.windows(expected.len()).any(|window| window == expected) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "terminal fixture did not emit {expected:?}: {transcript:?}");
        match master.read(&mut buffer) {
            Ok(0) => panic!("terminal fixture closed before {expected:?}: {transcript:?}"),
            Ok(read) => transcript.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(POLL_INTERVAL.min(remaining));
            }
            Err(error) => panic!("terminal fixture failed before {expected:?}: {error}"),
        }
    }
}

fn wait_terminal_supervisor(child: &mut Child) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal supervisor did not exit",
            ));
        }
        thread::sleep(POLL_INTERVAL.min(remaining));
    }
}

struct KillOnDrop(Option<Child>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = wait_terminal_supervisor(child);
        }
    }
}
