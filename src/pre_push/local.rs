//! Validated local input for one pre-push publication attempt.
//!
//! A local stack is the ordered first-parent path from the default branch to
//! `HEAD`. Its order is the source of parent, child, and root relationships.
//! Those relationships are deliberately not stored alongside each change.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    str,
};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::ObjectId;

use super::autosquash;
use crate::util;

const MAX_GHERRIT_PR_ID_BYTES: usize = 128;

/// An ASCII alphanumeric `gherrit-pr-id` of 1 through 128 bytes.
///
/// Constructing a `GherritPrId` is validation. Code which has one can therefore
/// use it as a managed ref-name component without repeating trailer checks.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct GherritPrId(
    // INVARIANT: `.0` is nonempty, ASCII alphanumeric, and at most
    // `MAX_GHERRIT_PR_ID_BYTES` bytes long.
    String,
);

impl GherritPrId {
    fn from_trailer(commit: ObjectId, value: &[u8]) -> Result<Self> {
        if value.is_empty() {
            bail!("Commit {commit} missing gherrit-pr-id trailer");
        }
        if value.len() > MAX_GHERRIT_PR_ID_BYTES {
            bail!(
                "Commit {commit} has a gherrit-pr-id trailer longer than the {MAX_GHERRIT_PR_ID_BYTES}-byte limit"
            );
        }
        if !value.iter().all(u8::is_ascii_alphanumeric) {
            bail!("Commit {commit} has invalid gherrit-pr-id trailer");
        }

        Ok(Self(str::from_utf8(value)?.to_owned()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// One local commit which GHerrit can publish.
///
/// The commit's identity, first parent, and user-visible metadata are read
/// together. `LocalChange` instances exist only inside a validated
/// [`LocalStack`].
#[derive(Debug)]
pub(super) struct LocalChange {
    id: GherritPrId,
    head: ObjectId,
    first_parent: ObjectId,
    title: String,
    body: String,
}

impl LocalChange {
    fn from_commit(commit: gix::Commit<'_>, title: String) -> Result<Self> {
        let message = commit.message()?;
        let raw_body = message.body.map(AsRef::as_ref);
        let body = raw_body
            .map(str::from_utf8)
            .transpose()
            .wrap_err_with(|| format!("Commit {} has a non-UTF-8 message body", commit.id))?
            .unwrap_or("");
        let identity_source = raw_body.unwrap_or(commit.message_raw()?.as_ref());
        let mut ids = gherrit_id_trailers(identity_source);
        let id = match ids.len() {
            0 => bail!("Commit {} missing gherrit-pr-id trailer", commit.id),
            1 => ids.pop().expect("length checked above"),
            _ => bail!("Commit {} has multiple gherrit-pr-id trailers", commit.id),
        };
        let (value, line) = match id {
            GherritIdTrailer::Exact { value, line } => (value, line),
            GherritIdTrailer::Malformed => {
                bail!("Commit {} has invalid gherrit-pr-id trailer syntax", commit.id)
            }
        };

        let id = GherritPrId::from_trailer(commit.id, &value)?;
        let first_parent = commit
            .parent_ids()
            .next()
            .ok_or_else(|| eyre!("Commit {} has no first parent", commit.id))?
            .detach();
        let body = raw_body.map_or_else(String::new, |_| strip_gherrit_id(body, line));

        Ok(Self { id, head: commit.id, first_parent, title, body })
    }

    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn head(&self) -> ObjectId {
        self.head
    }

    pub(super) fn first_parent(&self) -> ObjectId {
        self.first_parent
    }

    pub(super) fn title(&self) -> &str {
        &self.title
    }

    pub(super) fn body(&self) -> &str {
        &self.body
    }
}

/// An ordered, validated first-parent path from the default branch to `HEAD`.
#[derive(Debug)]
pub(super) struct LocalStack {
    changes: Vec<LocalChange>,
}

impl LocalStack {
    /// Reads and validates the local managed stack without performing network
    /// writes.
    pub(super) fn collect(repo: &util::Repo) -> Result<Self> {
        let head = repo.rev_parse_single("HEAD")?;
        let default_branch = repo.find_default_branch_on_default_remote();
        let default_ref = repo.rev_parse_single(format!("refs/heads/{default_branch}").as_str())?;
        if head == default_ref {
            return Self::new(default_ref.detach(), Vec::new());
        }

        repo.ensure_publishable_history()?;
        let commits = repo.first_parent_commits_between(default_ref, head).map_err(|err| match err {
            util::FirstParentCommitsBetweenError::NotOnFirstParentPath => {
                let branch_name = repo.current_branch().name().unwrap_or("current branch");
                eyre!(
                    "The branch '{branch_name}' does not descend from '{default_branch}' on its first-parent path.\n\
                     GHerrit defines stack order using first-parent ancestry.\n\
                     Maybe you want to 'git rebase' on '{default_branch}' before pushing?"
                )
            }
            util::FirstParentCommitsBetweenError::Eyre(error) => error,
        })?;

        let commits = commits
            .into_iter()
            .map(|commit| -> Result<_> {
                let title = str::from_utf8(commit.message()?.title)
                    .wrap_err_with(|| format!("Commit {} has a non-UTF-8 title", commit.id))?
                    .to_owned();
                Ok((commit, title))
            })
            .collect::<Result<Vec<_>>>()?;

        autosquash::ensure_publishable(
            commits.iter().map(|(_, title)| title.as_str()),
            &repo.default_remote_name(),
            &default_branch,
        )?;

        let changes = commits
            .into_iter()
            .map(|(commit, title)| LocalChange::from_commit(commit, title))
            .collect::<Result<Vec<_>>>()?;

        let stack = Self::new(default_ref.detach(), changes)?;
        ensure_change_ids_unique_in_head_ancestry(repo, &stack, head.detach())?;
        Ok(stack)
    }

    fn new(default_tip: ObjectId, changes: Vec<LocalChange>) -> Result<Self> {
        let ids = changes.iter().map(|change| change.id.as_str());
        ensure_unique_change_ids(ids)?;

        let mut expected_parent = default_tip;
        for change in &changes {
            if change.first_parent() != expected_parent {
                bail!(
                    "Commit {} is not on the first-parent path from the default branch",
                    change.head
                );
            }
            expected_parent = change.head;
        }

        Ok(Self { changes })
    }

    pub(super) fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.changes.len()
    }

    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = &LocalChange> {
        self.changes.iter()
    }

    pub(super) fn as_slice(&self) -> &[LocalChange] {
        &self.changes
    }
}

fn ensure_unique_change_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Result<()> {
    ids.into_iter().try_fold(HashSet::new(), |mut seen, id| {
        if !seen.insert(id) {
            bail!("Stack contains multiple commits with gherrit-pr-id '{id}'");
        }
        Ok(seen)
    })?;
    Ok(())
}

/// One occurrence of the identity key in a raw trailer block.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum GherritIdTrailer {
    /// An occurrence with GHerrit's exact `: ` separator.
    Exact { value: Vec<u8>, line: Range<usize> },
    /// An occurrence whose separator is not exactly `: `.
    Malformed,
}

/// Reads identity-key occurrences from the final raw commit-message paragraph.
///
/// Git's trailer formatter normalizes inputs such as `key:value` into
/// `key: value`. The raw bytes are therefore the only authority for GHerrit's
/// exact `: ` separator. Same-key occurrences with other separators remain
/// visible as malformed rather than disappearing. Other keys matter only for
/// recognizing the final block. Continuations are unfolded so validation
/// rejects a continued value rather than silently accepting its first line.
pub(super) fn gherrit_id_trailers(message: &[u8]) -> Vec<GherritIdTrailer> {
    let mut start = 0;
    let mut lines = message
        .split_inclusive(|byte| *byte == b'\n')
        .map(|terminated| {
            let end = start + terminated.len();
            let line = terminated.strip_suffix(b"\n").unwrap_or(terminated);
            let raw_line = start..end;
            start = end;
            (raw_line, line.strip_suffix(b"\r").unwrap_or(line))
        })
        .collect::<Vec<_>>();
    while lines.last().is_some_and(|(_, line)| line.is_empty()) {
        lines.pop();
    }
    let block_start =
        lines.iter().rposition(|(_, line)| line.is_empty()).map_or(0, |index| index + 1);
    let mut identities = Vec::<GherritIdTrailer>::new();
    let mut has_entry = false;
    let mut continued_identity: Option<usize> = None;

    for (raw_line, line) in &lines[block_start..] {
        if line.first().is_some_and(|byte| matches!(byte, b' ' | b'\t')) {
            if !has_entry {
                return Vec::new();
            }
            if let Some(index) = continued_identity {
                let GherritIdTrailer::Exact { value, .. } = &mut identities[index] else {
                    unreachable!("only exact identity trailers have continuation indices")
                };
                value.push(b' ');
                let value_start = line
                    .iter()
                    .position(|byte| !matches!(byte, b' ' | b'\t'))
                    .unwrap_or(line.len());
                value.extend_from_slice(&line[value_start..]);
            }
            continue;
        }

        let Some(separator) = line.iter().position(|byte| matches!(byte, b':' | b'=')) else {
            return Vec::new();
        };
        let key = &line[..separator];
        if key.is_empty()
            || key.iter().any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Vec::new();
        }

        has_entry = true;
        continued_identity = None;
        if key.eq_ignore_ascii_case(b"gherrit-pr-id") {
            if line.get(separator..separator + 2) == Some(b": ") {
                identities.push(GherritIdTrailer::Exact {
                    value: line[separator + 2..].to_vec(),
                    line: raw_line.clone(),
                });
                continued_identity = Some(identities.len() - 1);
            } else {
                identities.push(GherritIdTrailer::Malformed);
            }
        }
    }

    identities
}

/// Requires each active change ID to identify exactly one reachable commit.
///
/// Stack order follows first parents, so commits reachable only through a
/// merge are not published as changes. They are nevertheless part of every
/// proposed head which contains the merge. Reusing an active ID anywhere in
/// that complete ancestry would make the ID describe two different commits.
fn ensure_change_ids_unique_in_head_ancestry(
    repo: &util::Repo,
    stack: &LocalStack,
    head: ObjectId,
) -> Result<()> {
    if stack.is_empty() {
        return Ok(());
    }

    let expected_heads = stack
        .iter()
        .map(|change| (change.id().as_str().as_bytes(), (change.id().as_str(), change.head())))
        .collect::<HashMap<_, _>>();
    let mut observed = HashSet::with_capacity(stack.len());

    for commit in
        repo.rev_walk([head]).all().wrap_err("Failed to begin inspecting HEAD ancestry")?
    {
        let commit = commit
            .wrap_err("Failed to inspect HEAD ancestry")?
            .object()
            .wrap_err("Failed to read a commit in HEAD ancestry")?;
        for id in gherrit_id_trailers(commit.message_raw()?) {
            let GherritIdTrailer::Exact { value: id, .. } = id else {
                continue;
            };
            let Some((id, expected_head)) = expected_heads.get(id.as_slice()) else {
                continue;
            };
            if commit.id != *expected_head {
                bail!(
                    "HEAD ancestry contains multiple commits with gherrit-pr-id '{id}': \
                     {expected_head} and {}",
                    commit.id,
                );
            }
            observed.insert(*id);
        }
    }

    if let Some(change) = stack.iter().find(|change| !observed.contains(change.id().as_str())) {
        bail!(
            "Git omitted gherrit-pr-id '{}' while inspecting HEAD ancestry",
            change.id().as_str()
        );
    }

    Ok(())
}

fn strip_gherrit_id(body: &str, line: Range<usize>) -> String {
    let mut body = body.to_string();
    body.replace_range(line, "");
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_values(message: &[u8]) -> Vec<Option<Vec<u8>>> {
        gherrit_id_trailers(message)
            .into_iter()
            .map(|trailer| match trailer {
                GherritIdTrailer::Exact { value, .. } => Some(value),
                GherritIdTrailer::Malformed => None,
            })
            .collect()
    }

    fn object_id(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    fn change(id: &str, head: u8, first_parent: u8) -> LocalChange {
        LocalChange {
            id: GherritPrId::from_trailer(object_id(head), id.as_bytes()).unwrap(),
            head: object_id(head),
            first_parent: object_id(first_parent),
            title: String::new(),
            body: String::new(),
        }
    }

    #[test]
    fn gherrit_pr_ids_require_bounded_nonempty_ascii_alphanumeric_values() {
        for id in [b"A".as_slice(), b"G123", b"abcDEF012"] {
            assert_eq!(
                GherritPrId::from_trailer(object_id(1), id).unwrap().as_str(),
                str::from_utf8(id).unwrap()
            );
        }

        for id in [b"".as_slice(), b"with-dash", b"with space", "snowman-☃".as_bytes()] {
            assert!(GherritPrId::from_trailer(object_id(1), id).is_err(), "id={id:?}");
        }

        let maximum = "G".repeat(MAX_GHERRIT_PR_ID_BYTES);
        assert_eq!(
            GherritPrId::from_trailer(object_id(1), maximum.as_bytes()).unwrap().as_str(),
            maximum
        );

        let too_long = "G".repeat(MAX_GHERRIT_PR_ID_BYTES + 1);
        let error = GherritPrId::from_trailer(object_id(1), too_long.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("longer than the 128-byte limit"));
    }

    #[test]
    fn raw_messages_require_an_exact_identity_trailer() {
        for message in [
            b"Work\n\nGherrit-Pr-Id: Gone".as_slice(),
            b"Work\r\n\r\ngherrit-pr-id: Gone\r\n",
            b"gherrit-pr-id: Gone",
        ] {
            assert_eq!(identity_values(message), [Some(b"Gone".to_vec())]);
        }

        for message in [
            b"Work\n\ngherrit-pr-id:Gone".as_slice(),
            b"Work\n\ngherrit-pr-id=Gone",
            b"Work\n\ngherrit-pr-id:\tGone",
            b"Work\n\ngherrit-pr-id:: Gone",
        ] {
            assert_eq!(identity_values(message), [None]);
        }

        assert_eq!(
            identity_values(b"Non-UTF-8 body: \xff\n\ngherrit-pr-id: Gone"),
            [Some(b"Gone".to_vec())]
        );
        assert_eq!(identity_values(b"Work\n\ngherrit-pr-id: G\xff"), [Some(b"G\xff".to_vec())]);
    }

    #[test]
    fn raw_messages_use_only_the_final_trailer_block() {
        assert!(
            identity_values(b"Work\n\ngherrit-pr-id: Gbody\n\nThis final paragraph is prose.")
                .is_empty()
        );
        assert_eq!(
            identity_values(
                b"Work\n\ngherrit-pr-id: Gbody\n\nReviewed-by: A\ngherrit-pr-id: Greal\n"
            ),
            [Some(b"Greal".to_vec())]
        );
        assert!(identity_values(b"Work\n\nThis is prose.\ngherrit-pr-id: Gembedded").is_empty());
    }

    #[test]
    fn raw_messages_preserve_empty_multiple_and_continued_values() {
        assert_eq!(
            identity_values(b"Work\n\ngherrit-pr-id: \ngherrit-pr-id: Gvalid"),
            [Some(Vec::new()), Some(b"Gvalid".to_vec())]
        );
        assert_eq!(
            identity_values(b"Work\n\ngherrit-pr-id: Gone\n continuation"),
            [Some(b"Gone continuation".to_vec())]
        );
        assert_eq!(identity_values(b"Work\n\ngherrit-pr-id: Gone\n \t"), [Some(b"Gone ".to_vec())]);
        assert_eq!(
            identity_values(b"Work\n\nReviewed-by: A\n continued\ngherrit-pr-id: Gone"),
            [Some(b"Gone".to_vec())]
        );
    }

    #[test]
    fn raw_messages_retain_malformed_occurrences_beside_exact_ones() {
        assert_eq!(
            identity_values(
                b"Work\n\ngherrit-pr-id: Gvalid\ngherrit-pr-id:Gother\nGHERRIT-PR-ID=Gthird"
            ),
            [Some(b"Gvalid".to_vec()), None, None]
        );
    }

    #[test]
    fn stacks_require_unique_change_ids() {
        let error =
            LocalStack::new(object_id(0), vec![change("Gsame", 1, 0), change("Gsame", 2, 1)])
                .unwrap_err();

        assert_eq!(error.to_string(), "Stack contains multiple commits with gherrit-pr-id 'Gsame'");
    }

    #[test]
    fn stacks_require_one_contiguous_first_parent_path() {
        let stack = LocalStack::new(
            object_id(0),
            vec![change("Gone", 1, 0), change("Gtwo", 2, 1), change("Gthree", 3, 2)],
        )
        .unwrap();

        assert_eq!(
            stack.iter().map(|change| change.id().as_str()).collect::<Vec<_>>(),
            ["Gone", "Gtwo", "Gthree"]
        );

        let error = LocalStack::new(object_id(0), vec![change("Gone", 1, 0), change("Gtwo", 2, 0)])
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "Commit {} is not on the first-parent path from the default branch",
                object_id(2)
            )
        );
    }

    #[test]
    fn empty_stack_does_not_require_publishable_history() {
        let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
        let repository = util::Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        let default_tip = repository.rev_parse_single("refs/heads/main").unwrap().detach();

        std::fs::create_dir_all(context.repo_path.join(".git/info")).unwrap();
        std::fs::write(context.repo_path.join(".git/info/grafts"), format!("{default_tip}\n"))
            .unwrap();
        std::fs::write(context.repo_path.join(".git/shallow"), format!("{default_tip}\n")).unwrap();
        context.run_git(&["config", "remote.origin.promisor", "true"]);

        let stack = LocalStack::collect(&repository).unwrap();

        assert!(stack.is_empty());
    }

    #[test]
    fn stack_order_derives_root_parent_and_child_positions() {
        let stack = LocalStack::new(
            object_id(0),
            vec![change("Gone", 1, 0), change("Gtwo", 2, 1), change("Gthree", 3, 2)],
        )
        .unwrap();
        let ids = stack.iter().map(|change| change.id().as_str()).collect::<Vec<_>>();

        assert_eq!(ids.first(), Some(&"Gone"));
        assert_eq!(ids.last(), Some(&"Gthree"));
        assert_eq!(
            ids.windows(2).map(|pair| (pair[0], pair[1])).collect::<Vec<_>>(),
            [("Gone", "Gtwo"), ("Gtwo", "Gthree")]
        );
    }

    #[test]
    fn strips_only_the_matching_trailer_from_the_final_trailer_block() {
        let body = "Summary\r\n\r\ngherrit-pr-id: Gexample\r\n\r\nNotes\r\n\r\nGherrit-Pr-Id: Greal\r\n\r\n";
        let trailers = gherrit_id_trailers(body.as_bytes());
        let [GherritIdTrailer::Exact { value, line }] = trailers.as_slice() else {
            panic!("expected one exact identity trailer")
        };
        assert_eq!(value, b"Greal");

        assert_eq!(
            strip_gherrit_id(body, line.clone()),
            "Summary\r\n\r\ngherrit-pr-id: Gexample\r\n\r\nNotes\r\n\r\n\r\n"
        );
    }
}
