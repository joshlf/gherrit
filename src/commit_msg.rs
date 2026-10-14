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

use std::{fs, io::Write as _, path::Path};

use eyre::{Result, WrapErr, bail};
use owo_colors::OwoColorize;

use crate::{
    cmd,
    identity::has_normalized_id as has_gherrit_id,
    manage::State,
    util::{self, CommandExt as _},
};

pub(crate) const ID_ENTROPY_BYTES: usize = 20;
pub(crate) type IdEntropy = [u8; ID_ENTROPY_BYTES];

fn is_temporary_squash(message: &str) -> bool {
    message.lines().next().is_some_and(|line| line.starts_with("squash! "))
}

/// Ask the running Git, including its supported aliases and config order.
/// Automatic commit-time selection remains ambiguous because Git does not
/// pass the character chosen before the editor to this hook.
fn native_comment_prefix() -> Result<Vec<u8>> {
    let mut input = crate::subprocess::RegularFileStdinBuilder::with_limit(2)?;
    input.write_all(b"x\n")?;
    let output = crate::subprocess::local_output(
        &mut cmd!("git stripspace --comment-lines"),
        Some(input.finish()?),
        std::time::Duration::from_secs(60),
        4 * 1024 * 1024,
    )?;
    if !output.status.success() {
        bail!("Git comment-prefix observation failed");
    }
    output
        .stdout
        .strip_suffix(b" x\n")
        .filter(|prefix| !prefix.is_empty())
        .map(<[u8]>::to_vec)
        .ok_or_else(|| eyre::eyre!("Git returned malformed comment-prefix output"))
}

/// A comment/scissors-cleaned view for hook decisions only. The hook does not
/// receive `--cleanup` or `--allow-empty-message` flags, so conservatively leave
/// empty messages alone. Git owns cancellation and explicit empty acceptance.
/// Stored commits are still parsed with the exact raw-message grammar.
fn message_view(prefix: &[u8], message: &[u8]) -> Result<Vec<u8>> {
    let mut scissors = prefix.to_vec();
    scissors.extend_from_slice(b" ------------------------ >8 ------------------------");
    let mut length = 0;
    for line in message.split_inclusive(|byte| *byte == b'\n') {
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        if content == scissors {
            break;
        }
        length += line.len();
    }
    // Prepared private file input avoids a pipe producer or shell quoting.
    let mut input = crate::subprocess::RegularFileStdinBuilder::with_limit(length as u64)
        .wrap_err("Failed to prepare message cleanup")?;
    input.write_all(&message[..length])?;
    let output = crate::subprocess::local_output(
        &mut cmd!("git stripspace --strip-comments"),
        Some(input.finish()?),
        std::time::Duration::from_secs(60),
        // stripspace only removes bytes, apart from one final newline.
        length.saturating_add(1),
    )?;
    if !output.status.success() {
        bail!("Git message cleanup failed");
    }
    Ok(output.stdout)
}

/// Keep retained comment text before the raw trailer block. Git may leave it
/// after trailers for later cleanup, but verbatim commits retain those bytes.
/// The stored identity grammar must stay independent of later configuration.
fn relocate_trailing_comments(prefix: &[u8], message: &[u8]) -> Vec<u8> {
    fn content(line: &[u8]) -> &[u8] {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        line.strip_suffix(b"\r").unwrap_or(line)
    }
    fn line_before(message: &[u8], end: usize) -> (usize, &[u8]) {
        let content_end = if message[end - 1] == b'\n' { end - 1 } else { end };
        let start = message[..content_end]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        (start, content(&message[start..content_end]))
    }
    let mut scissors = prefix.to_vec();
    scissors.extend_from_slice(b" ------------------------ >8 ------------------------");
    if message.split(|byte| *byte == b'\n').any(|line| content(line) == scissors) {
        return message.to_vec();
    }
    let mut end = message.len();
    let mut found_comment = false;
    while end > 0 {
        let (start, line) = line_before(message, end);
        if line.starts_with(prefix) {
            found_comment = true;
        } else if !line.iter().all(u8::is_ascii_whitespace) {
            break;
        }
        end = start;
    }
    if !found_comment {
        return message.to_vec();
    }
    let mut start = end;
    while start > 0 {
        let (previous, line) = line_before(message, start);
        if line.iter().all(u8::is_ascii_whitespace) {
            break;
        }
        start = previous;
    }
    let block = &message[start..end];
    // Compatibility recognition does not authorize moving interleaved body
    // comments. Preserve the prior contiguous-block rewrite boundary.
    if block.split(|byte| *byte == b'\n').any(|line| line.starts_with(b"#")) {
        return message.to_vec();
    }
    match crate::identity::gherrit_id_trailers(block).as_slice() {
        [crate::identity::GherritIdTrailer::Exact { value, .. }]
            if crate::identity::GherritPrId::from_ref_component(value).is_ok() => {}
        _ => return message.to_vec(),
    }
    let separator_bytes: usize = message[end..]
        .split_inclusive(|byte| *byte == b'\n')
        .take_while(|line| content(line).iter().all(u8::is_ascii_whitespace))
        .map(|line| line.len())
        .sum();
    let mut output = message[..start].to_vec();
    output.extend_from_slice(&message[end + separator_bytes..]);
    if !output.ends_with(b"\n") {
        output.push(b'\n');
    }
    if !output.ends_with(b"\n\n") {
        output.push(b'\n');
    }
    output.extend_from_slice(block);
    output
}

/// Only the suffix can distinguish a retained message from scissors cancellation
/// when the cleaned prefix is empty. Editing there preserves both outcomes.
fn retained_scissors_suffix(prefix: &[u8], message: &[u8]) -> Result<Option<usize>> {
    let mut scissors = prefix.to_vec();
    scissors.extend_from_slice(b" ------------------------ >8 ------------------------");
    let mut offset = 0;
    for line in message.split_inclusive(|byte| *byte == b'\n') {
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        if content == scissors {
            let after = offset + line.len();
            return Ok((message_view(prefix, &message[..offset])?.is_empty()
                && !message_view(prefix, &message[after..])?.is_empty())
            .then_some(after));
        }
        offset += line.len();
    }
    Ok(None)
}

fn derive_gherrit_id(mut entropy: IdEntropy, object_hash: &[u8]) -> String {
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

pub fn run(repo: &util::Repo, msg_file: &Path, acquire_entropy: fn() -> IdEntropy) -> Result<()> {
    let msg_path = msg_file;
    if !msg_path.try_exists().wrap_err("Failed to check file existence")? {
        bail!("File does not exist: {}", msg_path.display().red().bold());
    }

    // Get current branch (supporting rebase)
    let Some(branch_name) = repo.current_branch().name() else {
        log::debug!("Could not determine branch name (detached head?). Skipping.");
        return Ok(());
    };

    if State::read_required_from(repo, branch_name)? == State::Unmanaged {
        log::warn!("Branch {} is not managed. Skipping.", branch_name.yellow());
        return Ok(());
    }

    // Skip temporary squash commits (e.g. from `git commit --squash`) so
    // changes intended to be folded into another commit do not receive a
    // persistent review identity or enter the managed stack.
    let msg_content = fs::read_to_string(msg_path).wrap_err("Failed to read msg file")?;
    if is_temporary_squash(&msg_content) {
        return Ok(());
    }

    let prefix = native_comment_prefix()?;
    if let Some(offset) = retained_scissors_suffix(&prefix, msg_content.as_bytes())? {
        let mut suffix = tempfile::NamedTempFile::new()?;
        suffix.write_all(&msg_content.as_bytes()[offset..])?;
        edit_identity(repo, &prefix, suffix.path(), &msg_content[offset..], acquire_entropy)?;
        let mut written = msg_content.as_bytes()[..offset].to_vec();
        written.extend_from_slice(&fs::read(suffix.path())?);
        fs::write(msg_path, written).wrap_err("Failed to write the retained scissors suffix")?;
        return Ok(());
    }
    edit_identity(repo, &prefix, msg_file, &msg_content, acquire_entropy)
}

fn edit_identity(
    repo: &util::Repo,
    prefix: &[u8],
    msg_file: &Path,
    msg_content: &str,
    acquire_entropy: fn() -> IdEntropy,
) -> Result<()> {
    let msg_path = msg_file;
    // Recognition retains configured legacy separators. Put ':' first so
    // Git formats every recognized key consistently, while insertion below
    // still writes only GHerrit's canonical separator.
    let configured = cmd!("git config --null --get trailer.separators").bounded_output()?;
    let mut separators = "trailer.separators=:".to_owned();
    match configured.status.code() {
        Some(0) => {
            let value = configured
                .stdout
                .strip_suffix(b"\0")
                .ok_or_else(|| eyre::eyre!("Malformed Git trailer separator configuration"))?;
            separators.push_str(std::str::from_utf8(value)?);
        }
        Some(1) if configured.stdout.is_empty() => {}
        _ => bail!("Failed to read Git trailer separator configuration"),
    }
    let output = cmd!("git")
        .args([
            "-c",
            &separators,
            "-c",
            "trailer.gherrit-pr-id.key=gherrit-pr-id",
            "interpret-trailers",
            "--no-divider",
            "--parse",
        ])
        .arg(msg_file)
        .checked_output_with_limits(
            std::time::Duration::from_secs(60),
            // Normalization can expand keys. Keep the ordinary allowance
            // while scaling it for large accepted messages.
            msg_content.len().saturating_mul(2).saturating_add(1).max(4 * 1024 * 1024),
        )?;
    let trailers = String::from_utf8_lossy(&output.stdout);

    if has_gherrit_id(&trailers) {
        let normalized = relocate_trailing_comments(prefix, msg_content.as_bytes());
        if normalized != msg_content.as_bytes() {
            fs::write(msg_path, normalized).wrap_err("Failed to retain the raw trailer block")?;
        }
        return Ok(());
    }

    if message_view(prefix, msg_content.as_bytes())?.is_empty() {
        let mut scissors = prefix.to_vec();
        scissors.extend_from_slice(b" ------------------------ >8 ------------------------");
        if msg_content
            .as_bytes()
            .split(|byte| *byte == b'\n')
            .any(|line| line.strip_suffix(b"\r").unwrap_or(line) == scissors)
        {
            log::warn!(
                "Git's cleanup mode is unavailable to commit-msg hooks. Preserving possible scissors cancellation without generating an identity; remove the scissors boundary before committing text below it."
            );
        }
        let config = repo.config_snapshot();
        if !msg_content.trim().is_empty()
            && config.string("core.commentString").is_none()
            && config.string("core.commentChar").is_some_and(|value| value.as_ref() == b"auto")
        {
            log::warn!(
                "Git's automatic comment character is unavailable to commit-msg hooks. Preserving this message without generating an identity; configure a fixed comment character if it should receive an ID."
            );
        }
        return Ok(());
    }

    // Construct the stable GHerrit ID input:
    // "Ident\nRefHash\nMsgContent".
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
    let gherrit_id = derive_gherrit_id(acquire_entropy(), object_id.as_bytes());

    // Insert trailer
    // Stored identities use the final trailer paragraph, even after patch text.
    // --no-divider: keep parsing and writing on that same grammar
    // --where start: puts it at the top of the trailer block
    // --if-exists doNothing: prevents duplicates
    cmd!(
        "git -c trailer.separators=: -c trailer.gherrit-pr-id.key=gherrit-pr-id interpret-trailers --no-divider --in-place --where start --if-exists doNothing --trailer",
        "gherrit-pr-id: {gherrit_id}"
    )
    .arg(msg_file)
    .success()?;
    let written = fs::read(msg_path).wrap_err("Failed to validate the written identity")?;
    let normalized = relocate_trailing_comments(prefix, &written);
    if normalized != written {
        fs::write(msg_path, &normalized).wrap_err("Failed to retain the raw trailer block")?;
    }
    let written = normalized;
    // COMMIT_EDITMSG can still contain Git status comments below the trailer.
    match crate::identity::gherrit_id_trailers(&message_view(prefix, &written)?).as_slice() {
        [crate::identity::GherritIdTrailer::Exact { value, .. }]
            if crate::identity::GherritPrId::from_ref_component(value)?.as_str() == gherrit_id => {}
        _ => bail!("Git trailer configuration did not produce one canonical GHerrit identity"),
    }
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
