use std::{
    ffi::OsStr,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    process::{Command, ExitStatus, Output, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use command_group::{CommandGroup, GroupChild};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub struct TestCommand {
    command: Command,
    timeout: Duration,
    input: Option<Vec<u8>>,
}

impl TestCommand {
    pub(crate) fn new(program: impl AsRef<OsStr>) -> Self {
        Self { command: Command::new(program), timeout: DEFAULT_TIMEOUT, input: None }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.command.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command.args(args);
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.command.env(key, value);
        self
    }

    pub(crate) fn envs<I, K, V>(&mut self, variables: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.command.envs(variables);
        self
    }

    pub(crate) fn env_clear(&mut self) -> &mut Self {
        self.command.env_clear();
        self
    }

    pub fn current_dir(&mut self, directory: impl AsRef<Path>) -> &mut Self {
        self.command.current_dir(directory);
        self
    }

    pub fn timeout(&mut self, timeout: Duration) -> &mut Self {
        self.timeout = timeout;
        self
    }

    pub(crate) fn input(&mut self, input: impl Into<Vec<u8>>) -> &mut Self {
        self.input = Some(input.into());
        self
    }

    pub fn output(&mut self) -> io::Result<Output> {
        // Back stdin with a temporary file rather than a pipe. A child which
        // stops reading can fill a pipe and block the caller before the
        // command timeout is observed. The file makes input delivery
        // independent of child progress and needs no unsupervised writer
        // thread.
        let stdin = command_stdin(self.input.take())?;
        self.command.stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = spawn_command_group(&mut self.command)?;
        let deadline = Instant::now() + self.timeout;
        let stdout = child.inner().stdout.take().map(read_pipe);
        let stderr = child.inner().stderr.take().map(read_pipe);

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(
                        POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                Ok(None) => {
                    let _ = child.kill();
                    wait_for_group_exit(&mut child, Instant::now() + CLEANUP_TIMEOUT)?;
                    collect_output(stdout, stderr, CLEANUP_TIMEOUT)?;
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("command timed out after {:?}", self.timeout),
                    ));
                }
                Err(error) => {
                    let _ = child.kill();
                    return Err(error);
                }
            }
        };

        // A process leader can exit while a descendant still holds the output
        // pipes. Terminating the residual group makes command completion and
        // fixture teardown the same lifecycle boundary.
        let _ = child.kill();
        let (stdout, stderr) = collect_output(stdout, stderr, CLEANUP_TIMEOUT)?;
        Ok(Output { status, stdout, stderr })
    }

    #[must_use]
    pub fn assert(&mut self) -> assert_cmd::assert::Assert {
        let output = self
            .output()
            .unwrap_or_else(|error| panic!("Failed to run {:?}: {error}", self.command));
        assert_cmd::assert::Assert::new(output)
            .append_context("command", format!("{:?}", self.command))
    }
}

fn command_stdin(input: Option<Vec<u8>>) -> io::Result<Stdio> {
    let Some(input) = input else { return Ok(Stdio::null()) };
    let mut file = tempfile::tempfile()?;
    file.write_all(&input)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(Stdio::from(file))
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

fn read_pipe(mut pipe: impl Read + Send + 'static) -> Receiver<io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut output = Vec::new();
        let result = pipe.read_to_end(&mut output).map(|_| output);
        let _ = sender.send(result);
    });
    receiver
}

fn wait_for_group_exit(child: &mut GroupChild, deadline: Instant) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "command group did not terminate after it was killed",
            ));
        }
        thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

fn collect_output(
    stdout: Option<Receiver<io::Result<Vec<u8>>>>,
    stderr: Option<Receiver<io::Result<Vec<u8>>>>,
    timeout: Duration,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let deadline = Instant::now() + timeout;
    let stdout = receive_pipe(stdout, deadline, "stdout")?;
    let stderr = receive_pipe(stderr, deadline, "stderr")?;
    Ok((stdout, stderr))
}

fn receive_pipe(
    pipe: Option<Receiver<io::Result<Vec<u8>>>>,
    deadline: Instant,
    name: &str,
) -> io::Result<Vec<u8>> {
    let Some(pipe) = pipe else { return Ok(Vec::new()) };
    match pipe.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out collecting command {name}"),
        )),
        Err(RecvTimeoutError::Disconnected) => {
            Err(io::Error::other(format!("command {name} reader exited without a result")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{env, process::Command, thread, time::Instant};

    use super::*;

    const DESCENDANT_TEST_MODE: &str = "GHERRIT_DESCENDANT_TIMEOUT_TEST";
    const UNREAD_INPUT_TEST_MODE: &str = "GHERRIT_UNREAD_INPUT_TIMEOUT_TEST";

    #[test]
    fn terminates_descendants_on_timeout() {
        match env::var(DESCENDANT_TEST_MODE).as_deref() {
            Ok("parent") => {
                let mut leaf = Command::new(env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "command::tests::terminates_descendants_on_timeout",
                        "--nocapture",
                    ])
                    .env(DESCENDANT_TEST_MODE, "leaf")
                    .spawn()
                    .unwrap();
                leaf.wait().unwrap();
                return;
            }
            Ok("leaf") => {
                thread::sleep(Duration::from_secs(2));
                return;
            }
            _ => {}
        }

        let mut command = TestCommand::new(env::current_exe().unwrap());
        command
            .args(["--exact", "command::tests::terminates_descendants_on_timeout", "--nocapture"])
            .env(DESCENDANT_TEST_MODE, "parent")
            .timeout(Duration::from_millis(100));

        let started = Instant::now();
        let error = command.output().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "descendant retained command output pipes for {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn input_does_not_depend_on_child_reading() {
        if env::var(UNREAD_INPUT_TEST_MODE).is_ok() {
            thread::sleep(Duration::from_secs(2));
            return;
        }

        let mut command = TestCommand::new(env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "command::tests::input_does_not_depend_on_child_reading",
                "--nocapture",
            ])
            .env(UNREAD_INPUT_TEST_MODE, "1")
            .input(vec![b'x'; 4 * 1024 * 1024])
            .timeout(Duration::from_millis(100));

        let started = Instant::now();
        let error = command.output().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "unread input retained command execution for {:?}",
            started.elapsed()
        );
    }
}
