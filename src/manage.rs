use crate::cmd;
use crate::util::{self, BranchError, CommandExt};

pub fn manage() {
    let (_, branch_name) = util::get_current_branch().unwrap();

    cmd!("git config", "branch.{branch_name}.gherritState", "managed").unwrap_status();
    log::info!("Branch '{}' is now managed by GHerrit.", branch_name);
}

pub fn unmanage() {
    let (_, branch_name) = util::get_current_branch().unwrap();

    cmd!("git config", "branch.{branch_name}.gherritState", "unmanaged").unwrap_status();
    eprintln!("Branch '{}' is now unmanaged by GHerrit.", branch_name);
}

pub fn post_checkout(_prev: &str, _new: &str, flag: &str) {
    // Only run on branch switches (flag=1)
    if flag != "1" {
        return;
    }

    let (_repo, branch_name) = match util::get_current_branch() {
        Ok(res) => res,
        Err(BranchError::DetachedHead) => return, // Detached HEAD (e.g. during rebase); do nothing.
        Err(e) => {
            log::error!("Failed to get current branch: {}", e);
            return;
        }
    };

    // Idempotency check: Bail if the branch management state is already set.
    let config_output = cmd!("git config", "branch.{branch_name}.gherritState").unwrap_output();
    if config_output.status.success() {
        log::debug!("Branch '{}' is already configured.", branch_name);
        return;
    }

    // Creation detection: Bail if we're just checking out an already-existing branch.
    let reflog_output = cmd!("git reflog show", branch_name, "-n1").unwrap_output();
    let reflog_stdout = String::from_utf8_lossy(&reflog_output.stdout);
    if !reflog_stdout.contains("branch: Created from") {
        log::debug!("Branch '{}' is not newly created.", branch_name);
        return;
    }

    let upstream_remote = cmd!("git config", "branch.{branch_name}.remote").unwrap_output();

    let upstream_merge = cmd!("git config", "branch.{branch_name}.merge").unwrap_output();

    let has_upstream = upstream_remote.status.success() && upstream_merge.status.success();
    let is_origin_main = if has_upstream {
        let remote = util::to_trimmed_string_lossy(&upstream_remote.stdout);
        let merge = util::to_trimmed_string_lossy(&upstream_merge.stdout);
        remote == "origin" && merge == "refs/heads/main"
    } else {
        false
    };

    if has_upstream && !is_origin_main {
        // Condition A: Shared Branch
        unmanage();
        log::info!("Branch initialized as UNMANAGED (Collaboration Mode).");
    } else {
        // Condition B: New Stack
        manage();
        log::info!("Branch initialized as MANAGED (Stack Mode).");
        log::info!("To opt-out, run: gherrit unmanage");
    }
}
