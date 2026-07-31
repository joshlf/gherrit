use std::{collections::HashMap, env, path::PathBuf, process::Command};

use testutil::mock_server::{GitCompletion, GitRequest, GitResponse};

fn main() {
    let args: Vec<String> = env::args().collect();
    let prog_name = PathBuf::from(&args[0]).file_stem().unwrap().to_string_lossy().to_string();

    assert_eq!(prog_name, "git");
    handle_git(&args);
}

fn handle_git(args: &[String]) {
    let server_url = env::var("GHERRIT_MOCK_SERVER_URL").unwrap();

    let cwd = env::current_dir().unwrap().to_string_lossy().to_string();
    let env_vars: HashMap<String, String> =
        env::vars().filter(|(k, _)| k == "MOCK_BIN_FAIL_CMD").collect();

    let req = GitRequest { args: args.to_vec(), cwd, env: env_vars };

    let resp: GitResponse = ureq::post(&format!("{}/_internal/git", server_url))
        .send_json(req)
        .expect("Failed to communicate with mock server")
        .into_json()
        .expect("Failed to parse mock server response"); // ureq 2.x

    if !resp.stdout.is_empty() {
        print!("{}", resp.stdout);
    }
    if !resp.stderr.is_empty() {
        eprint!("{}", resp.stderr);
    }

    if !resp.passthrough {
        std::process::exit(resp.exit_code);
    }

    let exit_code = run_real_git(args);
    if resp.report_exit_status {
        report_exit_status(&server_url, args, exit_code);
    }
    std::process::exit(exit_code);
}

fn run_real_git(args: &[String]) -> i32 {
    // Pass through to real `git` command
    let real_git = env::var("SYSTEM_GIT_PATH").unwrap_or_else(|_| "git".to_string());

    let status = Command::new(real_git)
        .args(&args[1..])
        .status()
        .expect("Failed to run real git from mock shim");

    status.code().unwrap_or(1)
}

fn report_exit_status(server_url: &str, args: &[String], exit_code: i32) {
    let completion = GitCompletion { args: args.to_vec(), exit_code };
    ureq::post(&format!("{}/_internal/git/complete", server_url))
        .send_json(completion)
        .expect("Failed to report real git exit status to mock server");
}
