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

use std::{fs, path::Path};

use eyre::{Result, WrapErr, bail};
use owo_colors::OwoColorize;

use crate::{
    cmd,
    util::{self, CommandExt as _},
};

const GHERRIT_ID_BYTES: usize = 20;

fn is_temporary_squash(message: &str) -> bool {
    message.lines().next().is_some_and(|line| line.starts_with("squash! "))
}

fn has_gherrit_id(trailers: &str) -> bool {
    trailers.lines().any(|line| line.starts_with("gherrit-pr-id: "))
}

fn acquire_id_entropy() -> [u8; GHERRIT_ID_BYTES] {
    if util::__TESTING { [0; GHERRIT_ID_BYTES] } else { rand::random() }
}

fn derive_gherrit_id(mut entropy: [u8; GHERRIT_ID_BYTES], object_hash: &[u8]) -> String {
    assert!(!object_hash.is_empty(), "object hash must not be empty");

    // IDs are collision identifiers, not secrets. Mixing with XOR keeps the
    // random input uniformly distributed while also incorporating the commit
    // data represented by `object_hash`.
    entropy
        .iter_mut()
        .zip(object_hash.iter().cycle())
        .for_each(|(entropy, object_hash)| *entropy ^= object_hash);

    format!("G{}", data_encoding::BASE32.encode(&entropy).to_ascii_lowercase())
}

pub fn run(repo: &util::Repo, msg_file: &str) -> Result<()> {
    let msg_path = Path::new(msg_file);
    if !msg_path.try_exists().wrap_err("Failed to check file existence")? {
        bail!("File does not exist: {}", msg_path.display().red().bold());
    }

    // Get current branch (supporting rebase)
    let Some(branch_name) = repo.current_branch().name() else {
        log::debug!("Could not determine branch name (detached head?). Skipping.");
        return Ok(());
    };

    if !repo.is_managed(branch_name)? {
        log::warn!("Branch {} is not managed. Skipping.", branch_name.yellow());
        return Ok(());
    }

    // Skip temporary squash commits (e.g. from `git commit --squash`) to
    // prevent creating "phantom" PRs for changes destined to be merged away.
    // These commits are transient and shouldn't be part of the persistent
    // managed stack.
    let msg_content = fs::read_to_string(msg_path).wrap_err("Failed to read msg file")?;
    if is_temporary_squash(&msg_content) {
        return Ok(());
    }

    // Calculate Change-ID
    // Construct the input: "Ident\nRefHash\nMsgContent"
    let input_data = {
        let committer_ident = cmd!("git var GIT_COMMITTER_IDENT").checked_output()?;
        let committer_ident =
            String::from_utf8_lossy(committer_ident.stdout.as_slice()).trim().to_string();

        // Use HEAD or the empty tree hash if this is the first commit
        let refhash = repo
            .head_id()
            .map(|h| h.to_string())
            .unwrap_or_else(|_| gix::ObjectId::empty_tree(repo.object_hash()).to_string());

        format!("{}\n{}\n{}", committer_ident, refhash, msg_content)
    };

    // Compute a hash of the object data and mix it with fresh entropy. This
    // minimizes the likelihood of collisions.
    let object_id = gix::diff::object::compute_hash(
        repo.object_hash(),
        gix::object::Kind::Blob,
        input_data.as_bytes(),
    )
    .wrap_err("Failed to compute hash")?;
    let gherrit_id = derive_gherrit_id(acquire_id_entropy(), object_id.as_bytes());

    // Check if trailer exists
    let output = cmd!("git interpret-trailers --parse", msg_file).checked_output()?;
    let trailers = String::from_utf8_lossy(&output.stdout);

    if has_gherrit_id(&trailers) {
        return Ok(());
    }

    // Insert trailer
    // --where start: puts it at the top of the trailer block
    // --if-exists doNothing: prevents duplicates
    cmd!(
        "git interpret-trailers --in-place --where start --if-exists doNothing --trailer",
        "gherrit-pr-id: {gherrit_id}",
        msg_file
    )
    .success()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_squash_classification() {
        [
            ("", false),
            ("ordinary subject", false),
            ("squash! subject", true),
            ("squash! ", true),
            ("squash! subject\n\nbody", true),
            ("ordinary subject\n\nsquash! body", false),
            ("squash!", false),
            ("squash!\tsubject", false),
            (" squash! subject", false),
            ("Squash! subject", false),
            ("fixup! subject", false),
            ("amend! subject", false),
        ]
        .into_iter()
        .for_each(|(message, expected)| {
            assert_eq!(is_temporary_squash(message), expected, "message: {message:?}");
        });
    }

    #[test]
    fn existing_id_classification() {
        [
            ("", false),
            ("other-trailer: value", false),
            ("gherrit-pr-id: Gabc", true),
            ("gherrit-pr-id: ", true),
            ("other-trailer: value\ngherrit-pr-id: Gabc", true),
            ("gherrit-pr-id:Gabc", false),
            ("Gherrit-pr-id: Gabc", false),
            (" gherrit-pr-id: Gabc", false),
            ("not-gherrit-pr-id: Gabc", false),
        ]
        .into_iter()
        .for_each(|(trailers, expected)| {
            assert_eq!(has_gherrit_id(trailers), expected, "trailers: {trailers:?}");
        });
    }

    #[test]
    fn derives_id_from_object_hash_and_entropy() {
        assert_eq!(derive_gherrit_id([0; 20], &[0; 20]), format!("G{}", "a".repeat(32)));
        assert_eq!(derive_gherrit_id([0; 20], &[u8::MAX; 20]), format!("G{}", "7".repeat(32)));
        assert_eq!(
            derive_gherrit_id([u8::MAX; 20], &[u8::MAX; 20]),
            format!("G{}", "a".repeat(32))
        );
    }

    #[test]
    fn mixes_every_entropy_byte() {
        let entropy = std::array::from_fn(|index| index as u8);
        let object_hash = std::array::from_fn::<_, 20, _>(|index| (index as u8) << 1);
        let mixed = std::array::from_fn::<_, 20, _>(|index| entropy[index] ^ object_hash[index]);

        assert_eq!(
            derive_gherrit_id(entropy, &object_hash),
            format!("G{}", data_encoding::BASE32.encode(&mixed).to_ascii_lowercase())
        );
    }

    #[test]
    fn preserves_object_hash_cycle_behavior() {
        let entropy = [0; 20];
        let object_hash = [0x12, 0x34, 0x56];
        let mixed = std::array::from_fn::<_, 20, _>(|index| object_hash[index % 3]);

        assert_eq!(
            derive_gherrit_id(entropy, &object_hash),
            format!("G{}", data_encoding::BASE32.encode(&mixed).to_ascii_lowercase())
        );
    }

    #[test]
    fn preserves_long_object_hash_behavior() {
        let entropy = [0; 20];
        let mut object_hash = [0; 32];
        object_hash[20..].fill(u8::MAX);

        assert_eq!(derive_gherrit_id(entropy, &object_hash), format!("G{}", "a".repeat(32)));
    }

    #[test]
    #[should_panic(expected = "object hash must not be empty")]
    fn rejects_an_empty_object_hash() {
        derive_gherrit_id([0; 20], &[]);
    }
}
