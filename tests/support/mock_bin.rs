use std::{env, path::PathBuf, process::Command};

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
        if let Ok(server_url) = env::var("GHERRIT_TEST_SERVER_URL") {
            // Parse refspecs (args that look like refs or have colons)
            let refspecs: Vec<String> = args
                .iter()
                .skip(1) // Skip "git"
                .filter(|arg| arg.starts_with("refs/") || arg.contains(":"))
                .cloned()
                .collect();

            if !refspecs.is_empty() {
                let url = format!("{}/_test/git-push", server_url);
                if let Err(e) = ureq::post(&url)
                    .send_json(serde_json::json!({
                        "refspecs": refspecs
                    }))
                {
                    eprintln!("WARN: mock_bin failed to report push to server: {}", e);
                }

                // Simulate GitHub output which is filtered by `pre-push` hook.
                // This output must match the regex in pre_push.rs.
                // Ideally this should be dynamic based on server response, but hardcoded is fine for now
                // to match previous behavior (mock_state didn't dictate this output really).
                // But wait, the previous code fetched owner/repo from state to print the URL.
                // We should probably fetch it or just use 'owner/repo' defaults if we don't want to query state.
                // The integration tests might rely on matching the owner/repo names in the URL?
                // Integration tests typically use "owner" and "repo" defaults unless configured.
                // Let's assume defaults for now or query state if strictly needed.
                // Comparing with old code:
                // let (repo_owner, repo_name) = update_state(...)
                // It DID use the values from state.
                // I will use hardcoded defaults or env vars if provided to keep it simple,
                // or I could query `GET /_test/state`.
                // Let's query state to be safe.
                
                let state_url = format!("{}/_test/state", server_url);
                 let (repo_owner, repo_name) = match ureq::get(&state_url).call() {
                    Ok(resp) => {
                        let json: serde_json::Value = resp.into_json().unwrap_or(serde_json::json!({}));
                        (
                             json["repo_owner"].as_str().unwrap_or("owner").to_string(),
                             json["repo_name"].as_str().unwrap_or("repo").to_string()
                        )
                    },
                    Err(_) => ("owner".to_string(), "repo".to_string()),
                };

                eprintln!("remote: ");
                eprintln!("remote: Create a pull request for 'feature' on GitHub by visiting:");
                eprintln!("remote:      https://github.com/{}/{}/pull/new/feature", repo_owner, repo_name);
                eprintln!("remote: ");
            }
        }
    }

    // Pass through to real `git` command
    let real_git = env::var("SYSTEM_GIT_PATH").unwrap_or_else(|_| "git".to_string());
    let status = Command::new(real_git)
        .args(&args[1..])
        .status()
        .expect("Failed to run real git from mock shim");

    std::process::exit(status.code().unwrap_or(1));
}
