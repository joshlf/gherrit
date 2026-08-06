//! Git command interceptor for the hermetic test harness.
//!
//! This module is compiled only when `GHERRIT_TEST_BUILD` is set. The test
//! harness places this binary on `PATH` as `git`. On Unix, a small wrapper
//! invokes the binary with [`INVOCATION`]; on Windows, the binary is copied to
//! `git.exe` and recognized by its executable name.

use std::{
    collections::HashMap,
    env,
    ffi::OsStr,
    io::{self, Read as _, Write as _},
    net::TcpStream,
    path::Path,
    process::{Command, ExitCode},
    time::Duration,
};

use serde::{Deserialize, Serialize};

const INVOCATION: &str = "__test-git";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize)]
struct GitRequest {
    args: Vec<String>,
    cwd: String,
    env: HashMap<String, String>,
}

#[derive(Deserialize)]
struct GitResponse {
    stdout: String,
    stderr: String,
    exit_code: i32,
    passthrough: bool,
    report_exit_status: bool,
    override_exit_code: Option<i32>,
}

#[derive(Serialize)]
struct GitCompletion<'a> {
    args: &'a [String],
    exit_code: i32,
}

pub fn is_invocation() -> bool {
    invocation_argument_offset().is_some()
}

pub fn run() -> ExitCode {
    let argument_offset =
        invocation_argument_offset().expect("test Git interceptor was not invoked");
    let args = std::iter::once("git".to_string())
        .chain(env::args().skip(argument_offset))
        .collect::<Vec<_>>();
    let server_url = env::var("GHERRIT_MOCK_SERVER_URL").expect("missing mock server URL");
    let request = GitRequest {
        args: args.clone(),
        cwd: env::current_dir().unwrap().to_string_lossy().into_owned(),
        env: env::vars()
            .filter(|(name, _)| {
                matches!(name.as_str(), "MOCK_BIN_FAIL_CMD" | "MOCK_BIN_FAIL_AFTER_CMD")
            })
            .collect(),
    };

    let response = post_json(&server_url, "/_internal/git", &request);
    let response: GitResponse =
        serde_json::from_slice(&response).expect("invalid mock server response");

    print!("{}", response.stdout);
    eprint!("{}", response.stderr);
    io::stdout().flush().unwrap();
    io::stderr().flush().unwrap();

    if !response.passthrough {
        return exit_code(response.exit_code);
    }

    let status = Command::new(env::var("SYSTEM_GIT_PATH").expect("missing system Git path"))
        .args(&args[1..])
        .status()
        .expect("failed to run system Git");
    let code = status.code().unwrap_or(1);

    if response.report_exit_status {
        let completion = GitCompletion { args: &args, exit_code: code };
        post_json(&server_url, "/_internal/git/complete", &completion);
    }

    exit_code(response.override_exit_code.unwrap_or(code))
}

fn invocation_argument_offset() -> Option<usize> {
    let mut arguments = env::args_os();
    let executable = arguments.next()?;
    invocation_argument_offset_for(&executable, arguments.next().as_deref())
}

fn invocation_argument_offset_for(
    executable: &OsStr,
    first_argument: Option<&OsStr>,
) -> Option<usize> {
    if first_argument == Some(OsStr::new(INVOCATION)) {
        return Some(2);
    }

    (cfg!(windows)
        && Path::new(executable)
            .file_stem()
            .is_some_and(|stem| stem.to_string_lossy().eq_ignore_ascii_case("git")))
    .then_some(1)
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn post_json(url: &str, path: &str, body: &impl Serialize) -> Vec<u8> {
    let authority = url.strip_prefix("http://").expect("mock server URL must use HTTP");
    assert!(!authority.contains('/'), "mock server URL must not contain a path");

    let body = serde_json::to_vec(body).expect("failed to serialize mock server request");
    let mut stream = TcpStream::connect(authority).expect("failed to connect to mock server");
    stream.set_read_timeout(Some(REQUEST_TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(REQUEST_TIMEOUT)).unwrap();
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(&body).unwrap();

    let mut response = Vec::new();
    let mut read_buffer = [0; 4096];
    let header_end = loop {
        let count = stream.read(&mut read_buffer).expect("failed to read mock server response");
        assert!(count != 0, "mock server returned an incomplete HTTP response");
        response.extend_from_slice(&read_buffer[..count]);
        if let Some(position) = response.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = std::str::from_utf8(&response[..header_end]).expect("non-UTF-8 HTTP headers");
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .expect("mock server returned an invalid HTTP status");
    assert!((200..300).contains(&status), "mock server returned HTTP {status}");

    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("invalid HTTP content length"))
        })
        .unwrap_or_else(|| {
            assert_eq!(status, 204, "mock response has no content length");
            0
        });
    while response.len() < header_end + content_length {
        let count =
            stream.read(&mut read_buffer).expect("failed to read mock server response body");
        assert!(count != 0, "mock server returned an incomplete HTTP response body");
        response.extend_from_slice(&read_buffer[..count]);
    }
    response[header_end..header_end + content_length].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_marker_invocation() {
        assert_eq!(
            invocation_argument_offset_for(OsStr::new("gherrit"), Some(OsStr::new(INVOCATION)),),
            Some(2)
        );
    }

    #[test]
    fn rejects_regular_gherrit_invocation() {
        assert_eq!(
            invocation_argument_offset_for(OsStr::new("gherrit"), Some(OsStr::new("manage")),),
            None
        );
    }

    #[test]
    #[cfg(windows)]
    fn recognizes_native_windows_interceptor() {
        assert_eq!(
            invocation_argument_offset_for(
                OsStr::new(r"C:\fixture\git.exe"),
                Some(OsStr::new("commit")),
            ),
            Some(1)
        );
        assert_eq!(
            invocation_argument_offset_for(
                OsStr::new(r"C:\fixture\GIT.EXE"),
                Some(OsStr::new("commit")),
            ),
            Some(1)
        );
    }
}
