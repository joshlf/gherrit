use std::{collections::HashMap, env, path::PathBuf, process::Command};

use serde::{Deserialize, Serialize};
use testutil::mock_server::{GitRequest, GitResponse};

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

    if resp.passthrough {
        run_real_git(args);
    } else {
        std::process::exit(resp.exit_code);
    }
}

fn run_real_git(args: &[String]) {
    // Pass through to real `git` command
    let real_git = env::var("SYSTEM_GIT_PATH").unwrap_or_else(|_| "git".to_string());

    let status = Command::new(real_git)
        .args(&args[1..])
        .status()
        .expect("Failed to run real git from mock shim");

    std::process::exit(status.code().unwrap_or(1));
}
