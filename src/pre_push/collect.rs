//! Read local history and admit the commits that make up a publishable stack.
//!
//! Native Git recognizes complete-message trailers; this module checks their
//! cardinality and identity grammar after autosquash admission. It reads local
//! objects and runs trailer queries, without publishing Git or GitHub state.

use std::collections::HashSet;

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::ObjectId;

use super::{autosquash, identity::ChangeId};
use crate::{
    commit_message::strip_gherrit_id,
    util::{self, CommandExt as _},
};

pub(super) fn collect_commits(repo: &util::Repo) -> Result<Vec<Commit>> {
    // Stack membership is based on the selected local default-branch ref, not
    // a freshly fetched remote tip. The user must update/rebase local history
    // when that ref no longer describes the intended publication base.
    let head = repo.rev_parse_single("HEAD")?;
    let default_branch = repo.find_default_branch_on_default_remote()?;
    let default_ref = repo.rev_parse_single(format!("refs/heads/{}", default_branch).as_str())?;

    let commits = repo.commits_between(default_ref, head).map_err(|err| match err {
        util::CommitsBetweenError::NotAncestor => {
            let branch_name = repo.current_branch().name().unwrap_or("current branch");
            eyre!(
                "The branch '{branch_name}' is not based on '{default_branch}'.\n\
                 GHerrit only supports stacked branches that share history with the default branch.\n\
                 Maybe you want to 'git rebase' on '{default_branch}' before pushing?"
            )
        }
        util::CommitsBetweenError::Eyre(e) => e,
    })?;

    let commits = commits
        .into_iter()
        .map(|commit| -> Result<_> {
            let title = core::str::from_utf8(commit.message()?.title)?.to_owned();
            Ok((commit, title))
        })
        .collect::<Result<Vec<_>>>()?;

    autosquash::ensure_publishable(
        commits.iter().map(|(_, title)| title.as_str()),
        &repo.default_remote_name()?,
        &default_branch,
    )?;

    let trailers = read_commit_trailers(&commits)?;
    let commits = commits
        .into_iter()
        .zip(trailers)
        .map(|((commit, _), trailers)| Commit::from_git(commit, &trailers))
        .collect::<Result<Vec<_>>>()?;
    ensure_unique_gherrit_ids(commits.iter().map(|commit| commit.gherrit_id.as_str()))?;
    Ok(commits)
}

fn read_commit_trailers(commits: &[(gix::Commit<'_>, String)]) -> Result<Vec<Vec<u8>>> {
    // Let native Git recognize trailers in each complete committed message.
    // Query several objects per child process, but retain their requested
    // order so a trailer can never silently migrate to a different commit.
    const QUERY_BATCH_LEN: usize = 120;
    const FORMAT: &str = "--format=tformat:%H%x00%(trailers:only,unfold)";

    commits.chunks(QUERY_BATCH_LEN).try_fold(
        Vec::with_capacity(commits.len()),
        |mut parsed, chunk| {
            let arguments = ["log", "--no-walk=unsorted", "-z", FORMAT]
                .into_iter()
                .map(ToString::to_string)
                .chain(chunk.iter().map(|(commit, _)| commit.id.to_string()));
            let output = util::cmd("git", arguments)
                .checked_output()
                .wrap_err("Failed to parse commit trailers")?;
            let object_ids = chunk.iter().map(|(commit, _)| commit.id).collect::<Vec<_>>();
            parsed.extend(decode_commit_trailers(&output.stdout, &object_ids)?);
            Ok(parsed)
        },
    )
}

fn decode_commit_trailers(output: &[u8], object_ids: &[ObjectId]) -> Result<Vec<Vec<u8>>> {
    // Each record is OID NUL trailers NUL, with one final empty split field.
    // Newlines belong to trailer content; NUL framing separates the records.
    // Validate counts and OIDs before exposing any values to ID validation.
    let mut parsed = Vec::with_capacity(object_ids.len());
    let mut fields = output.split(|byte| *byte == 0);
    for object_id in object_ids {
        let returned_id = fields
            .next()
            .ok_or_else(|| eyre!("Git omitted trailer data for commit {object_id}"))?;
        if returned_id != object_id.to_string().as_bytes() {
            bail!("Git returned commit trailers out of order");
        }
        let trailers = fields
            .next()
            .ok_or_else(|| eyre!("Git omitted trailer data for commit {object_id}"))?;
        parsed.push(trailers.to_vec());
    }
    if fields.next() != Some(&[][..]) || fields.next().is_some() {
        bail!("Git returned malformed commit trailer data");
    }
    Ok(parsed)
}

fn ensure_unique_gherrit_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Result<()> {
    // Two objects with one logical ID would both target the same branch and
    // PR. Reject the complete stack before any publication effect is issued.
    ids.into_iter().try_fold(HashSet::new(), |mut seen, id| {
        if !seen.insert(id) {
            bail!("Stack contains multiple commits with gherrit-pr-id '{id}'");
        }
        Ok(seen)
    })?;
    Ok(())
}

pub(super) struct Commit {
    // These identities have different lifetimes: id changes on a rewrite;
    // gherrit_id is retained so the rewritten change updates the same PR.
    id: ObjectId,
    gherrit_id: ChangeId,
    message_title: String,
    message_body: String,
}

impl Commit {
    // Only collection constructs these facts from a native commit. Consumers
    // can read them without replacing a validated ID or mixing message data
    // from a different revision into the value.
    pub(super) fn id(&self) -> ObjectId {
        self.id
    }

    pub(super) fn gherrit_id(&self) -> &ChangeId {
        &self.gherrit_id
    }

    pub(super) fn message_title(&self) -> &str {
        &self.message_title
    }

    pub(super) fn message_body(&self) -> &str {
        &self.message_body
    }

    fn from_git(c: gix::Commit<'_>, trailers: &[u8]) -> Result<Self> {
        let message = c.message()?;
        let message_title = core::str::from_utf8(message.title)?.to_string();
        let message_body = message
            .body
            .map(|body| core::str::from_utf8(body))
            .transpose()
            .wrap_err_with(|| format!("Commit {} body is not UTF-8", c.id))?
            .unwrap_or("")
            .to_string();
        let gherrit_id = committed_gherrit_id(trailers, c.id)?;
        let message_body = strip_gherrit_id(&message_body, gherrit_id.as_str());

        Ok(Commit { id: c.id, gherrit_id, message_title, message_body })
    }
}

fn committed_gherrit_id(trailers: &[u8], object_id: ObjectId) -> Result<ChangeId> {
    // The commit hook only preserves/inserts an identity. Publication is the
    // admission boundary: ambiguous or unsafe IDs must not become ref names.
    let mut gherrit_ids = crate::commit_message::gherrit_id_values(trailers);
    let gherrit_id = gherrit_ids
        .next()
        .ok_or_else(|| eyre!("Commit {} missing gherrit-pr-id trailer", object_id))?;
    if gherrit_ids.next().is_some() {
        bail!("Commit {} has multiple gherrit-pr-id trailers", object_id);
    }
    if gherrit_id.is_empty() {
        bail!("Commit {} missing gherrit-pr-id trailer", object_id);
    }
    ChangeId::parse(gherrit_id)
        .ok_or_else(|| eyre!("Commit {} has invalid gherrit-pr-id trailer", object_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_identity_corpus_preserves_admission_and_error_order() {
        let object_id = ObjectId::from_bytes_or_panic(&[0x11; 20]);
        for case in crate::commit_message::trailer_cases() {
            let actual = committed_gherrit_id(case.trailers.as_bytes(), object_id);
            if let Some(expected_id) = case.publication.strip_prefix("ok:") {
                assert_eq!(actual.unwrap().as_str(), expected_id, "{}", case.name);
            } else {
                let expected = case.publication.strip_prefix("error:").unwrap();
                assert_eq!(
                    actual.unwrap_err().to_string(),
                    format!("Commit {object_id} {expected}"),
                    "{}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn native_trailer_records_require_exact_ids_order_and_terminator() {
        let a = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        let b = ObjectId::from_hex(b"2222222222222222222222222222222222222222").unwrap();
        let valid = format!("{a}\0gherrit-pr-id: GA\n\0{b}\0\0");
        assert_eq!(
            decode_commit_trailers(valid.as_bytes(), &[a, b]).unwrap(),
            vec![b"gherrit-pr-id: GA\n".to_vec(), vec![]]
        );
        for malformed in [
            format!("{a}\0"),
            format!("{a}\0value"),
            format!("{b}\0value\0"),
            format!("{a}\0value\0extra\0"),
        ] {
            assert!(decode_commit_trailers(malformed.as_bytes(), &[a]).is_err(), "{malformed:?}");
        }
        assert!(decode_commit_trailers(valid.as_bytes(), &[b, a]).is_err());
    }
}
