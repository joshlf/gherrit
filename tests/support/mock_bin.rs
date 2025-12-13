use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

#[derive(Serialize, Deserialize, Debug)]
struct MockState {
    #[serde(default)]
    prs: Vec<PrEntry>,
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
            prs: vec![],
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

#[derive(Serialize, Deserialize, Debug, Clone)]
struct PrEntry {
    number: usize,
    title: Option<String>,
    body: Option<String>,
    url: String,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    // Detect identity via filename (handles .exe on Windows)
    let prog_name = PathBuf::from(&args[0])
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();

    if prog_name == "git" {
        handle_git(&args);
    } else {
        // Default to gh behavior if named gh or mock-gh
        handle_gh(&args);
    }
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

fn handle_gh(args: &[String]) {
    try_simulate_failure("gh", &args[1..]);

    // args[0] is program name, args[1] is subcommand (e.g., "pr")
    let subcmd = args.get(1).map(|s| s.as_str());
    if let Some("pr") = subcmd {
        match args.get(2).map(|s| s.as_str()) {
            Some("list") => {
                let state = read_state();
                println!("{}", serde_json::to_string(&state.prs).unwrap());
            }
            Some("create") => {
                // Parse arguments.
                let title = extract_arg(args, "--title").unwrap_or("No Title".into());
                let body = extract_arg(args, "--body").unwrap_or("".into());
                let _head = extract_arg(args, "--head").unwrap_or("?".into());
                let _base = extract_arg(args, "--base").unwrap_or("main".into());

                let url = update_state(|state| {
                    let max_id = state.prs.iter().map(|p| p.number).max().unwrap_or(100);
                    let num = max_id + 1;
                    let u = format!(
                        "https://github.com/{}/{}/pull/{num}",
                        state.repo_owner, state.repo_name
                    );

                    let entry = PrEntry {
                        number: num,
                        title: Some(title),
                        body: Some(body),
                        url: u.clone(),
                        extra: std::collections::HashMap::new(),
                    };
                    state.prs.push(entry);
                    u
                });

                println!("{}", url);
            }
            Some("edit") => {
                // usage: gh pr edit <number> ...
                if let Some(id_or_url) = args.get(3) {
                    let target_number = if let Ok(num) = id_or_url.parse::<usize>() {
                        Some(num)
                    } else {
                        let s = read_state();
                        s.prs.iter().find(|p| p.url == *id_or_url).map(|p| p.number)
                    };

                    if let Some(num) = target_number {
                        let title = extract_arg(args, "--title");
                        let body = extract_arg(args, "--body");

                        update_state(|state| {
                            if let Some(pr) = state.prs.iter_mut().find(|p| p.number == num) {
                                if let Some(t) = title {
                                    pr.title = Some(t);
                                }
                                if let Some(b) = body {
                                    pr.body = Some(b);
                                }
                            }
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

fn extract_arg(args: &[String], key: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == key {
            return iter.next().cloned();
        }
    }
    None
}

fn read_state() -> MockState {
    if let Ok(content) = fs::read_to_string("mock_state.json") {
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        MockState::default()
    }
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
    lock.with_mut(|out| out.write_all(new_content.as_bytes()))
        .unwrap();

    lock.commit().unwrap();

    result
}
