// Adapted from a bash script with the following copyright comment:
//
// From Gerrit Code Review 3.8.1-939-g8bc73efb23
//
// Part of Gerrit Code Review (https://www.gerritcodereview.com/)
//
// Copyright (C) 2009 The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fs;
use std::path::Path;

use crate::manage;
use crate::{
    cmd,
    util::{self, CommandExt as _, ResultExt as _},
};

const EMPTY_TREE_HASH: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

pub fn run(msg_file: &str) {
    let msg_path = Path::new(msg_file);
    if !msg_path.exists() {
        log::error!("File does not exist: {}", msg_path.display());
        std::process::exit(1);
    }

    let repo = gix::open(".").unwrap_or_exit("Failed to open repo");

    // Get current branch (Supporting Rebase)
    let branch_name = match get_active_branch_name(&repo) {
        Some(name) => name,
        None => {
            log::debug!("Could not determine branch name (detached head?). Skipping.");
            return;
        }
    };

    // Check if managed – bail if unmanaged or if management state is unset.
    if manage::get_state(&repo, &branch_name).unwrap_or_exit("Failed to get config")
        != Some(manage::State::Managed)
    {
        return;
    }

    // Squash check
    let msg_content = fs::read_to_string(msg_path).unwrap_or_exit("Failed to read msg file");
    if msg_content
        .lines()
        .next()
        .is_some_and(|l| l.starts_with("squash! "))
    {
        return;
    }

    // Calculate Change-ID
    // Construct the input: "Ident\nRefHash\nMsgContent"
    let input_data = {
        let committer_ident = cmd!("git var GIT_COMMITTER_IDENT").unwrap_output();
        let committer_ident = String::from_utf8_lossy(&committer_ident.stdout)
            .trim()
            .to_string();

        // Use HEAD or the empty tree hash if this is the first commit
        let refhash = repo
            .head_id()
            .map(|h| h.to_string())
            .unwrap_or_else(|_| EMPTY_TREE_HASH.to_string());

        format!("{}\n{}\n{}", committer_ident, refhash, msg_content)
    };

    let object_id = gix::diff::object::compute_hash(
        repo.object_hash(),
        gix::object::Kind::Blob,
        input_data.as_bytes(),
    )
    .unwrap_or_exit("Failed to compute hash");
    let random_hash = object_id.to_string();

    // Determine trailer token and value
    let review_url = util::get_config_string(&repo, "gerrit.reviewUrl").unwrap_or(None);
    let (token, value, regex_pattern) = if let Some(url) = review_url {
        let url = url.trim_end_matches('/');
        (
            "Link",
            format!("{}/id/I{}", url, random_hash),
            format!(r"^{}: .*/id/I[0-9a-f]{{40}}$", "Link"),
        )
    } else {
        (
            "gherrit-pr-id",
            format!("G{}", random_hash),
            format!(r"^{}: .*", "gherrit-pr-id"),
        )
    };

    // Check if trailer exists
    let output = cmd!("git interpret-trailers --parse", msg_file).unwrap_output();
    let trailers = String::from_utf8_lossy(&output.stdout);

    let re = regex::Regex::new(&regex_pattern).expect("Invalid regex");
    if trailers.lines().any(|line| re.is_match(line)) {
        return;
    }

    // Insert trailer
    // --where start: puts it at the top of the trailer block
    // --if-exists doNothing: prevents duplicates
    cmd!(
        "git interpret-trailers --in-place --where start --if-exists doNothing --trailer",
        "{token}: {value}",
        msg_file
    )
    .unwrap_status();
}

fn get_active_branch_name(repo: &gix::Repository) -> Option<String> {
    // Try standard HEAD
    if let Ok(head) = repo.head() {
        if let Some(name) = head.referent_name() {
            return Some(name.shorten().to_string());
        }
    }

    // If detached, check for rebase state in .git directory
    let git_dir = repo.path();

    // Interactive rebase
    let rebase_merge_head = git_dir.join("rebase-merge").join("head-name");
    if let Ok(name) = fs::read_to_string(&rebase_merge_head) {
        return Some(clean_branch_ref(&name));
    }

    // Apply-based rebase
    let rebase_apply_head = git_dir.join("rebase-apply").join("head-name");
    if let Ok(name) = fs::read_to_string(&rebase_apply_head) {
        return Some(clean_branch_ref(&name));
    }

    None
}

fn clean_branch_ref(name: &str) -> String {
    name.trim()
        .strip_prefix("refs/heads/")
        .unwrap_or(name.trim())
        .to_string()
}
