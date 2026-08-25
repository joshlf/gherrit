//! Validated local input for one pre-push publication attempt.
//!
//! A local stack is the ordered first-parent path from the default branch to
//! `HEAD`. Its order is the source of parent, child, and root relationships.
//! Those relationships are deliberately not stored alongside each change.

use std::{
    collections::{HashMap, HashSet},
    str,
};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::ObjectId;

use super::{autosquash, body::gherrit_pr_id_re, destination::DefaultBranch};
use crate::util::{self, CommandExt as _};

const MAX_GHERRIT_PR_ID_BYTES: usize = 128;

/// An ASCII alphanumeric GHerrit pull request ID of 1 through 128 bytes.
///
/// Construction proves the shared trailer and ref-component grammar. The
/// enclosing stack or remote-ref validation establishes where the value came
/// from and whether it identifies managed state.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct GherritPrId(
    // INVARIANT: `.0` is nonempty, ASCII alphanumeric, and at most
    // `MAX_GHERRIT_PR_ID_BYTES` bytes long.
    String,
);

impl GherritPrId {
    /// Decodes the same identity grammar from a managed remote ref component.
    ///
    /// Unlike a trailer error, this has no local commit to identify. Callers
    /// add the remote-ref context appropriate to the namespace they parse.
    pub(super) fn from_ref_component(value: &[u8]) -> Result<Self> {
        if value.is_empty()
            || value.len() > MAX_GHERRIT_PR_ID_BYTES
            || !value.iter().all(u8::is_ascii_alphanumeric)
        {
            bail!("invalid GHerrit change ID");
        }
        Ok(Self(str::from_utf8(value)?.to_owned()))
    }

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

        Self::from_ref_component(value)
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
    fn from_commit(commit: gix::Commit<'_>, title: String, trailers: &[u8]) -> Result<Self> {
        let message = commit.message()?;
        let body = message
            .body
            .map(|body| str::from_utf8(body.as_ref()))
            .transpose()
            .wrap_err_with(|| format!("Commit {} has a non-UTF-8 message body", commit.id))?
            .unwrap_or("");
        let mut ids = trailers.split(|byte| *byte == b'\n').filter_map(gherrit_id_trailer_value);
        let id = ids
            .next()
            .ok_or_else(|| eyre!("Commit {} missing gherrit-pr-id trailer", commit.id))?;
        if ids.next().is_some() {
            bail!("Commit {} has multiple gherrit-pr-id trailers", commit.id);
        }

        let id = GherritPrId::from_trailer(commit.id, id)?;
        let first_parent = commit
            .parent_ids()
            .next()
            .ok_or_else(|| eyre!("Commit {} has no first parent", commit.id))?
            .detach();
        let body = strip_gherrit_id(body, id.as_str());

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
    pub(super) fn collect(
        repo: &util::Repo,
        default_branch: &DefaultBranch,
        remote_name: &str,
    ) -> Result<Self> {
        let head = repo.rev_parse_single("HEAD")?;
        let default_ref =
            repo.rev_parse_single(default_branch.full_ref_name().as_str()).wrap_err_with(|| {
                format!("Local default branch '{}' is unavailable", default_branch.name())
            })?;
        if default_ref.detach() != default_branch.tip() {
            bail!(
                "Local default branch '{}' does not match the push repository",
                default_branch.name()
            );
        }
        if head == default_ref {
            return Self::new(default_ref.detach(), default_branch.name(), Vec::new());
        }

        repo.ensure_publishable_history()?;
        let commits = repo.first_parent_commits_between(default_ref, head).map_err(|err| match err {
            util::FirstParentCommitsBetweenError::NotOnFirstParentPath => {
                let branch_name = repo.current_branch().name().unwrap_or("current branch");
                let default_branch = default_branch.name();
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
            remote_name,
            default_branch.name(),
        )?;

        let trailers = read_commit_trailers(&commits)?;
        let changes = commits
            .into_iter()
            .zip(trailers)
            .map(|((commit, title), trailers)| LocalChange::from_commit(commit, title, &trailers))
            .collect::<Result<Vec<_>>>()?;

        let stack = Self::new(default_ref.detach(), default_branch.name(), changes)?;
        ensure_change_ids_unique_in_head_ancestry(&stack, head.detach())?;
        Ok(stack)
    }

    fn new(default_tip: ObjectId, default_branch: &str, changes: Vec<LocalChange>) -> Result<Self> {
        let ids = changes.iter().map(|change| change.id.as_str());
        ensure_unique_change_ids(ids)?;
        if let Some(change) = changes.iter().find(|change| change.id.as_str() == default_branch) {
            bail!(
                "Commit {} has gherrit-pr-id '{default_branch}', which conflicts with the repository default branch",
                change.head
            );
        }

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

fn read_commit_trailers(commits: &[(gix::Commit<'_>, String)]) -> Result<Vec<Vec<u8>>> {
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
            let mut fields = output.stdout.split(|byte| *byte == 0);

            chunk.iter().try_for_each(|(commit, _)| {
                let object_id = fields
                    .next()
                    .ok_or_else(|| eyre!("Git omitted trailer data for commit {}", commit.id))?;
                if object_id != commit.id.to_string().as_bytes() {
                    bail!("Git returned commit trailers out of order");
                }
                let trailers = fields
                    .next()
                    .ok_or_else(|| eyre!("Git omitted trailer data for commit {}", commit.id))?;
                parsed.push(trailers.to_vec());
                Ok(())
            })?;

            if fields.next() != Some(&[][..]) || fields.next().is_some() {
                bail!("Git returned malformed commit trailer data");
            }
            Ok(parsed)
        },
    )
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

fn gherrit_id_trailer_value(line: &[u8]) -> Option<&[u8]> {
    let colon = line.iter().position(|byte| *byte == b':')?;
    if !line[..colon].eq_ignore_ascii_case(b"gherrit-pr-id")
        || line.get(colon..colon + 2) != Some(b": ")
    {
        return None;
    }
    Some(&line[colon + 2..])
}

/// Requires each active change ID to identify exactly one reachable commit.
///
/// Stack order follows first parents, so commits reachable only through a
/// merge are not published as changes. They are nevertheless part of every
/// proposed head which contains the merge. Reusing an active ID anywhere in
/// that complete ancestry would make the ID describe two different commits.
fn ensure_change_ids_unique_in_head_ancestry(stack: &LocalStack, head: ObjectId) -> Result<()> {
    const FORMAT: &str = "--format=tformat:%H%x00%(trailers:only,unfold)";

    if stack.is_empty() {
        return Ok(());
    }

    let expected_heads = stack
        .iter()
        .map(|change| (change.id().as_str().as_bytes(), (change.id().as_str(), change.head())))
        .collect::<HashMap<_, _>>();
    let head = head.to_string();
    let output = util::cmd(
        "git",
        [
            "log",
            "--no-patch",
            "--no-show-signature",
            "--no-notes",
            "--no-decorate",
            "-z",
            FORMAT,
            &head,
        ],
    )
    .checked_output()
    .wrap_err("Failed to inspect commit identities in HEAD ancestry")?;
    let mut fields = output.stdout.split(|byte| *byte == 0);
    let mut observed = HashSet::with_capacity(stack.len());

    loop {
        let commit =
            fields.next().ok_or_else(|| eyre!("Git returned malformed commit ancestry data"))?;
        if commit.is_empty() {
            if fields.next().is_some() {
                bail!("Git returned malformed commit ancestry data");
            }
            break;
        }
        let commit = ObjectId::from_hex(commit)
            .wrap_err("Git returned an invalid commit ID while inspecting HEAD ancestry")?;
        let trailers =
            fields.next().ok_or_else(|| eyre!("Git omitted trailer data for commit {commit}"))?;

        for id in trailers.split(|byte| *byte == b'\n').filter_map(gherrit_id_trailer_value) {
            let Some((id, expected_head)) = expected_heads.get(id) else {
                continue;
            };
            if commit != *expected_head {
                bail!(
                    "HEAD ancestry contains multiple commits with gherrit-pr-id '{id}': \
                     {expected_head} and {commit}"
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

fn strip_gherrit_id(body: &str, id: &str) -> String {
    let trailer_start = body
        .rfind("\n\n")
        .map(|position| position + 2)
        .into_iter()
        .chain(body.rfind("\r\n\r\n").map(|position| position + 4))
        .max()
        .unwrap_or(0);
    let matching_trailer = gherrit_pr_id_re()
        .captures_iter(&body[trailer_start..])
        .filter(|captures| captures.get(1).is_some_and(|value| value.as_str() == id))
        .filter_map(|captures| captures.get(0))
        .last();
    let Some(trailer) = matching_trailer else {
        return body.to_string();
    };

    let mut body = body.to_string();
    let range = trailer.range();
    body.replace_range(trailer_start + range.start..trailer_start + range.end, "");
    body
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(GherritPrId::from_ref_component(maximum.as_bytes()).is_ok());

        let too_long = "G".repeat(MAX_GHERRIT_PR_ID_BYTES + 1);
        let error = GherritPrId::from_trailer(object_id(1), too_long.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("longer than the 128-byte limit"));
        assert!(GherritPrId::from_ref_component(too_long.as_bytes()).is_err());

        assert_eq!(gherrit_id_trailer_value(b"Gherrit-Pr-Id: Gone"), Some(b"Gone".as_slice()));
        assert_eq!(gherrit_id_trailer_value(b"gherrit-pr-id:Gone"), None);
    }

    #[test]
    fn stacks_require_unique_change_ids() {
        let error = LocalStack::new(
            object_id(0),
            "main",
            vec![change("Gsame", 1, 0), change("Gsame", 2, 1)],
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "Stack contains multiple commits with gherrit-pr-id 'Gsame'");
    }

    #[test]
    fn stacks_require_one_contiguous_first_parent_path() {
        let stack = LocalStack::new(
            object_id(0),
            "main",
            vec![change("Gone", 1, 0), change("Gtwo", 2, 1), change("Gthree", 3, 2)],
        )
        .unwrap();

        assert_eq!(
            stack.iter().map(|change| change.id().as_str()).collect::<Vec<_>>(),
            ["Gone", "Gtwo", "Gthree"]
        );

        let error =
            LocalStack::new(object_id(0), "main", vec![change("Gone", 1, 0), change("Gtwo", 2, 0)])
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
        let supplied_default = DefaultBranch::new("main".to_owned(), default_tip).unwrap();

        std::fs::create_dir_all(context.repo_path.join(".git/info")).unwrap();
        std::fs::write(context.repo_path.join(".git/info/grafts"), format!("{default_tip}\n"))
            .unwrap();
        std::fs::write(context.repo_path.join(".git/shallow"), format!("{default_tip}\n")).unwrap();
        context.run_git(&["config", "remote.origin.promisor", "true"]);

        let stack = LocalStack::collect(&repository, &supplied_default, "origin").unwrap();

        assert!(stack.is_empty());
    }

    #[test]
    fn stack_order_derives_root_parent_and_child_positions() {
        let stack = LocalStack::new(
            object_id(0),
            "main",
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
    fn stack_ids_cannot_name_the_default_branch() {
        let error = LocalStack::new(object_id(0), "main", vec![change("main", 1, 0)]).unwrap_err();

        assert_eq!(
            error.to_string(),
            format!(
                "Commit {} has gherrit-pr-id 'main', which conflicts with the repository default branch",
                object_id(1)
            )
        );
    }

    #[test]
    fn strips_only_the_matching_trailer_from_the_final_trailer_block() {
        let body = "Summary\n\ngherrit-pr-id: Gexample\n\nNotes\n\ngherrit-pr-id: Greal\n";

        assert_eq!(
            strip_gherrit_id(body, "Greal"),
            "Summary\n\ngherrit-pr-id: Gexample\n\nNotes\n\n\n"
        );
        assert_eq!(strip_gherrit_id(body, "Gmissing"), body);
    }
}
