use std::{env, fs, io::Write, path::PathBuf, process::Command};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
struct MockState {
    #[serde(default)]
    pushed_refs: Vec<String>,
    #[serde(default)]
    push_count: usize,
    #[serde(default = "default_owner")]
    repo_owner: String,
    #[serde(default = "default_repo")]
    repo_name: String,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            pushed_refs: vec![],
            push_count: 0,
            repo_owner: default_owner(),
            repo_name: default_repo(),
            extra: std::collections::HashMap::new(),
        }
    }
}

fn default_owner() -> String {
    "owner".to_string()
}

fn default_repo() -> String {
    "repo".to_string()
}

fn main() {
    let args: Vec<String> = env::args().collect();
    // Detect identity via filename (handles .exe on Windows)
    let prog_name = PathBuf::from(&args[0]).file_stem().unwrap().to_string_lossy().to_string();

    assert_eq!(prog_name, "git");
    handle_git(&args);
}

fn try_simulate_failure(command: &str, args: &[String]) {
    let Some(subcommand) = args.get(1) else {
        return;
    };
    if env::var("MOCK_BIN_FAIL_CMD").is_ok_and(|t| t == format!("{}:{}", command, subcommand)) {
        eprintln!("Simulated failure for {} {}", command, subcommand);
        std::process::exit(1);
    }
}

fn handle_git(args: &[String]) {
    try_simulate_failure("git", args);

    // Spy on "push" but pass through to real git
    if args.contains(&"push".to_string()) {
        // Parse refspecs (args that look like refs or have colons)
        let refspecs: Vec<String> = args
            .iter()
            .skip(1) // Skip "git"
            .filter(|arg| arg.starts_with("refs/") || arg.contains(":"))
            .cloned()
            .collect();

        let (repo_owner, repo_name) = update_state(|state| {
            state.pushed_refs.extend(refspecs);
            state.push_count += 1;
            (state.repo_owner.clone(), state.repo_name.clone())
        });

        // Simulate GitHub output which is filtered by `pre-push` hook.
        // This output must match the regex in pre_push.rs.
        eprintln!("remote: ");
        eprintln!("remote: Create a pull request for 'feature' on GitHub by visiting:");
        eprintln!("remote:      https://github.com/{repo_owner}/{repo_name}/pull/new/feature");
        eprintln!("remote: ");
    }

    // Pass through to real `git` command
    let real_git = env::var("SYSTEM_GIT_PATH").unwrap_or_else(|_| "git".to_string());
    let status = Command::new(real_git)
        .args(&args[1..])
        .status()
        .expect("Failed to run real git from mock shim");

    std::process::exit(status.code().unwrap_or(1));
}

fn update_state<O, F>(f: F) -> O
where
    F: FnOnce(&mut MockState) -> O,
{
    // Use gix::lock for robust file locking to ensure safe concurrent updates.
    // acquire_to_update_resource creates a lock file (e.g., mock_state.json.lock).
    // Defaults to failing immediately if locked, so we implement a retry loop.

    let state_path = PathBuf::from("mock_state.json");
    let mut lock = None;
    let mut retries = 0;

    while lock.is_none() {
        match gix::lock::File::acquire_to_update_resource(
            &state_path,
            gix::lock::acquire::Fail::Immediately,
            None,
        ) {
            Ok(l) => lock = Some(l),
            Err(gix::lock::acquire::Error::PermanentlyLocked { .. }) => {
                // Lock file exists, likely held by another process.
                std::thread::sleep(std::time::Duration::from_millis(10));
                retries += 1;
            }
            Err(e) => panic!("Failed to acquire lock: {e}"),
        }

        if retries > 500 {
            panic!("Timed out waiting for lock on mock_state.json");
        }
    }

    let mut lock = lock.unwrap();

    // Read current state (if file exists) while holding the lock.
    let mut state = if state_path.exists() {
        let content = fs::read_to_string(&state_path).unwrap();
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        MockState::default()
    };

    let result = f(&mut state);

    // Write the updated state and atomic commit (rename).
    let new_content = serde_json::to_string(&state).unwrap();
    lock.with_mut(|out| out.write_all(new_content.as_bytes())).unwrap();

    lock.commit().unwrap();

    result
}
