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
use crate::{cmd, util};
use eyre::{Result, WrapErr, bail};
use owo_colors::OwoColorize;

pub fn run(repo: &util::Repo, msg_file: &str) -> Result<()> {
    let msg_path = Path::new(msg_file);
    if !msg_path
        .try_exists()
        .wrap_err("Failed to check file existence")?
    {
        bail!("File does not exist: {}", msg_path.display().red().bold());
    }

    // Get current branch (Supporting Rebase)
    let Some(branch_name) = repo.current_branch().name() else {
        log::debug!("Could not determine branch name (detached head?). Skipping.");
        return Ok(());
    };

    // Check if managed – bail if unmanaged or if management state is unset.
    if manage::get_state(repo, branch_name).wrap_err("Failed to get config")?
        != Some(manage::State::Managed)
    {
        return Ok(());
    }

    // Skip temporary squash commits (e.g. from `git commit --squash`) to
    // prevent creating "phantom" PRs for changes destined to be merged away.
    // These commits are transient and shouldn't be part of the persistent
    // managed stack.
    let msg_content = fs::read_to_string(msg_path).wrap_err("Failed to read msg file")?;
    if msg_content
        .lines()
        .next()
        .is_some_and(|l| l.starts_with("squash! "))
    {
        return Ok(());
    }

    // Calculate Change-ID
    // Construct the input: "Ident\nRefHash\nMsgContent"
    let input_data = {
        let committer_ident = cmd!("git var GIT_COMMITTER_IDENT").output()?;
        let committer_ident = String::from_utf8_lossy(committer_ident.stdout.as_slice())
            .trim()
            .to_string();

        // Use HEAD or the empty tree hash if this is the first commit
        let refhash = repo
            .head_id()
            .map(|h| h.to_string())
            .unwrap_or_else(|_| gix::ObjectId::empty_tree(repo.object_hash()).to_string());

        format!("{}\n{}\n{}", committer_ident, refhash, msg_content)
    };

    // Compute a hash of the object data and a random salt. Combined,
    // this minimizes the likelihood of collisions.
    let hash = {
        let object_id = gix::diff::object::compute_hash(
            repo.object_hash(),
            gix::object::Kind::Blob,
            input_data.as_bytes(),
        )
        .wrap_err("Failed to compute hash")?;

        // Initialize the hash with the salt.
        let mut hash: [u8; 20] = rand::random();

        // Mix in the object hash using a simple XOR. This isn't
        // cryptographically secure, but it's good enough for our purposes –
        // namely, it ensures that the resulting `hash` has entropy from both
        // the salt and the object hash.
        for (r, &d) in hash.iter_mut().zip(object_id.as_bytes().iter().cycle()) {
            *r ^= d;
        }
        hash
    };

    // Poor man's hex encoding
    let mut hash_str = String::with_capacity(hash.len() * 2);
    for b in hash {
        use std::fmt::Write as _;
        write!(&mut hash_str, "{:02x}", b).unwrap();
    }

    // Check if trailer exists
    let output = cmd!("git interpret-trailers --parse", msg_file).output()?;
    let trailers = String::from_utf8_lossy(&output.stdout);

    let re = crate::re!(r"^gherrit-pr-id: .*");
    if trailers.lines().any(|line| re.is_match(line)) {
        return Ok(());
    }

    // Insert trailer
    // --where start: puts it at the top of the trailer block
    // --if-exists doNothing: prevents duplicates
    cmd!(
        "git interpret-trailers --in-place --where start --if-exists doNothing --trailer",
        "gherrit-pr-id: G{hash_str}",
        msg_file
    )
    .status()?;
    Ok(())
}
