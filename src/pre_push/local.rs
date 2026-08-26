//! Validated local input for one pre-push publication attempt.
//!
//! A local stack is the ordered first-parent path from the default branch to
//! the exact `HEAD` captured when publication began. Its order is the source
//! of parent, child, and root relationships. Those relationships are
//! deliberately not stored alongside each change.

use std::{
    collections::{HashMap, HashSet},
    str,
};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::ObjectId;

use super::{autosquash, destination::DefaultBranch};
use crate::{
    re,
    util::{self, CommandExt as _},
};

const MAX_TITLE_SCALARS: usize = 256;

/// A nonempty title of at most 256 Unicode scalar values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PullRequestTitle(String);

impl PullRequestTitle {
    fn new(value: String) -> Result<Self> {
        if value.is_empty() {
            bail!("A pull request title must not be empty");
        }
        if value.chars().nth(MAX_TITLE_SCALARS).is_some() {
            bail!(
                "A pull request title must contain at most {MAX_TITLE_SCALARS} Unicode scalar values"
            );
        }
        Ok(Self(value))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

const MAX_GHERRIT_PR_ID_BYTES: usize = 128;
pub(super) const GHERRIT_ID_TRAILER_FORMAT: &str =
    "--format=tformat:%H%x00%(trailers:key=gherrit-pr-id,only,unfold)";

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
    title: PullRequestTitle,
    body: String,
}

impl LocalChange {
    #[cfg(test)]
    /// Constructs trusted synthetic content for body-renderer unit tests.
    pub(super) fn for_body_test(
        id: GherritPrId,
        head: ObjectId,
        first_parent: ObjectId,
        title: String,
        body: String,
    ) -> Result<Self> {
        Ok(Self { id, head, first_parent, title: PullRequestTitle::new(title)?, body })
    }

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
        let title = PullRequestTitle::new(title)
            .wrap_err_with(|| format!("Commit {} has an invalid pull request title", commit.id))?;
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

    #[cfg(test)]
    pub(super) fn title(&self) -> &str {
        self.title.as_str()
    }

    pub(super) fn into_pull_request_content(self) -> (PullRequestTitle, String) {
        (self.title, self.body)
    }
}

/// An ordered, validated first-parent path to one captured `HEAD`.
#[derive(Debug)]
pub(super) struct LocalStack {
    default_branch: DefaultBranch,
    changes: Vec<LocalChange>,
}

impl LocalStack {
    /// Reads and validates the captured local managed stack without performing
    /// network writes.
    ///
    /// `branch_name` and `head` belong to the same pre-observation snapshot.
    /// Neither is re-read from the worktree while collection is in progress.
    pub(super) fn collect(
        repo: &util::Repo,
        branch_name: &str,
        head: ObjectId,
        default_branch: &DefaultBranch,
        remote_name: &str,
    ) -> Result<Self> {
        let default_ref =
            repo.rev_parse_single(default_branch.full_ref_name().as_str()).wrap_err_with(|| {
                format!("Local default branch '{}' is unavailable", default_branch.name())
            })?;
        let default_ref = default_ref.detach();
        if default_ref != default_branch.tip() {
            bail!(
                "Local default branch '{}' does not match the push repository",
                default_branch.name()
            );
        }
        if head == default_ref {
            let stack = Self::new(default_branch.clone(), Vec::new())?;
            debug_assert_eq!(stack.default_branch(), default_branch);
            return Ok(stack);
        }

        repo.ensure_publishable_history()?;
        let commits = repo.first_parent_commits_between(default_ref, head).map_err(|err| match err {
            util::FirstParentCommitsBetweenError::NotOnFirstParentPath => {
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

        let trailers = read_commit_trailers(repo, &commits)?;
        let changes = commits
            .into_iter()
            .zip(trailers)
            .map(|((commit, title), trailers)| LocalChange::from_commit(commit, title, &trailers))
            .collect::<Result<Vec<_>>>()?;

        let stack = Self::new(default_branch.clone(), changes)?;
        debug_assert_eq!(stack.default_branch(), default_branch);
        ensure_change_ids_unique_in_head_ancestry(repo, &stack, head)?;
        Ok(stack)
    }

    fn new(default_branch: DefaultBranch, changes: Vec<LocalChange>) -> Result<Self> {
        let ids = changes.iter().map(|change| change.id.as_str());
        ensure_unique_change_ids(ids)?;
        if let Some(change) = changes.iter().find(|change| {
            let id = change.id.as_str();
            default_branch.name() == id
                || default_branch
                    .name()
                    .strip_prefix(id)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            bail!(
                "Commit {} has gherrit-pr-id '{}', which conflicts with the repository default branch",
                change.head,
                change.id.as_str()
            );
        }

        let mut expected_parent = default_branch.tip();
        for change in &changes {
            if change.first_parent() != expected_parent {
                bail!(
                    "Commit {} is not on the first-parent path from the default branch",
                    change.head
                );
            }
            expected_parent = change.head;
        }

        Ok(Self { default_branch, changes })
    }

    #[cfg(test)]
    pub(super) fn for_history_test(
        default_branch: DefaultBranch,
        changes: impl IntoIterator<Item = (GherritPrId, ObjectId, ObjectId)>,
    ) -> Self {
        Self::for_plan_test(
            default_branch,
            changes.into_iter().map(|(id, head, first_parent)| {
                (id, head, first_parent, "Test change".to_owned(), String::new())
            }),
        )
    }

    /// Constructs a synthetic stack with distinct presentation content after
    /// local collection and validation have been tested at their own boundary.
    #[cfg(test)]
    pub(super) fn for_plan_test(
        default_branch: DefaultBranch,
        changes: impl IntoIterator<Item = (GherritPrId, ObjectId, ObjectId, String, String)>,
    ) -> Self {
        let changes = changes
            .into_iter()
            .map(|(id, head, first_parent, title, body)| LocalChange {
                id,
                head,
                first_parent,
                title: PullRequestTitle::new(title).expect("plan-test title is valid"),
                body,
            })
            .collect();
        Self::new(default_branch, changes).expect("valid plan-test local stack")
    }

    pub(super) fn default_branch(&self) -> &DefaultBranch {
        &self.default_branch
    }

    /// The exact commit named by the checked-out stack branch. An empty stack
    /// shares the observed default tip; otherwise its final change is HEAD.
    pub(super) fn tip(&self) -> ObjectId {
        self.changes.last().map_or(self.default_branch.tip(), LocalChange::head)
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

    pub(super) fn into_changes(self) -> Vec<LocalChange> {
        self.changes
    }
}

fn read_commit_trailers(
    repo: &util::Repo,
    commits: &[(gix::Commit<'_>, String)],
) -> Result<Vec<Vec<u8>>> {
    const QUERY_BATCH_LEN: usize = 120;

    commits.chunks(QUERY_BATCH_LEN).try_fold(
        Vec::with_capacity(commits.len()),
        |mut parsed, chunk| {
            let arguments = ["log", "--no-walk=unsorted", "-z", GHERRIT_ID_TRAILER_FORMAT]
                .into_iter()
                .map(ToString::to_string)
                .chain(chunk.iter().map(|(commit, _)| commit.id.to_string()));
            let mut command = util::cmd("git", arguments);
            command.current_dir(repo.workdir().unwrap_or(repo.path()));
            let output = command.checked_output().wrap_err("Failed to parse commit trailers")?;
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

pub(super) fn gherrit_id_trailer_value(line: &[u8]) -> Option<&[u8]> {
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
    let head = head.to_string();
    let mut command = util::cmd(
        "git",
        [
            "log",
            "--no-patch",
            "--no-show-signature",
            "--no-notes",
            "--no-decorate",
            "-z",
            GHERRIT_ID_TRAILER_FORMAT,
            &head,
        ],
    );
    command.current_dir(repo.workdir().unwrap_or(repo.path()));
    let output = command
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

fn gherrit_pr_id_re() -> &'static regex::Regex {
    re!(r"(?mi)^gherrit-pr-id[=:][ \t]*([a-zA-Z0-9]+)[ \t]*\r?$")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object_id(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    fn default_branch(name: &str, tip: u8) -> DefaultBranch {
        DefaultBranch::new(name.to_owned(), object_id(tip)).unwrap()
    }

    fn change(id: &str, head: u8, first_parent: u8) -> LocalChange {
        LocalChange {
            id: GherritPrId::from_trailer(object_id(head), id.as_bytes()).unwrap(),
            head: object_id(head),
            first_parent: object_id(first_parent),
            title: PullRequestTitle::new("Test change".to_owned()).unwrap(),
            body: String::new(),
        }
    }

    #[test]
    fn gherrit_id_trailers_require_a_nonempty_identifier() {
        assert!(gherrit_pr_id_re().is_match("gherrit-pr-id: Gone"));
        assert!(gherrit_pr_id_re().is_match("Gherrit-Pr-Id: Gone"));
        assert!(!gherrit_pr_id_re().is_match("gherrit-pr-id: "));
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
    fn pull_request_titles_use_unicode_scalar_limits() {
        assert!(PullRequestTitle::new(String::new()).is_err());
        assert_eq!(PullRequestTitle::new("雪".to_owned()).unwrap().as_str(), "雪");

        let ascii_256 = "x".repeat(256);
        assert_eq!(PullRequestTitle::new(ascii_256.clone()).unwrap().as_str(), ascii_256);
        assert!(PullRequestTitle::new("x".repeat(257)).is_err());

        let multibyte_256 = "雪".repeat(256);
        assert!(multibyte_256.len() > 256);
        assert!(PullRequestTitle::new(multibyte_256).is_ok());

        let combining_256 = "e\u{301}".repeat(128);
        assert_eq!(combining_256.chars().count(), 256);
        assert!(PullRequestTitle::new(combining_256).is_ok());
        assert!(PullRequestTitle::new(format!("{}x", "e\u{301}".repeat(128))).is_err());
    }

    #[test]
    fn collected_titles_enforce_the_unicode_scalar_limit() {
        let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
        let repository = util::Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        let default_tip = repository.rev_parse_single("refs/heads/main").unwrap().detach();
        let supplied_default = DefaultBranch::new("main".to_owned(), default_tip).unwrap();
        context.run_git(&["checkout", "-b", "feature"]);

        let collect = || {
            let repository = util::Repo::open(context.repo_path.to_str().unwrap()).unwrap();
            let head = repository.head_snapshot().unwrap().target().unwrap();
            LocalStack::collect(&repository, "feature", head, &supplied_default, "origin")
        };

        let accepted = "雪".repeat(MAX_TITLE_SCALARS);
        context.commit_with_gherrit_id(&accepted);
        let stack = collect().unwrap();
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.iter().next().unwrap().title(), accepted);

        let rejected = format!("{accepted}雪");
        context.commit_with_gherrit_id(&rejected);
        let rejected_head = util::Repo::open(context.repo_path.to_str().unwrap())
            .unwrap()
            .rev_parse_single("HEAD")
            .unwrap()
            .detach();
        let error = collect().unwrap_err();
        assert_eq!(
            error.chain().map(ToString::to_string).collect::<Vec<_>>(),
            [
                format!("Commit {rejected_head} has an invalid pull request title"),
                format!(
                    "A pull request title must contain at most {MAX_TITLE_SCALARS} Unicode scalar values"
                ),
            ]
        );
    }

    #[test]
    fn collection_uses_the_captured_head_after_the_worktree_moves() {
        let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
        let initial = util::Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        let default_tip = initial.rev_parse_single("refs/heads/main").unwrap().detach();
        let supplied_default = DefaultBranch::new("main".to_owned(), default_tip).unwrap();

        context.checkout_new("captured-stack");
        let captured_id = context.commit_with_gherrit_id("Captured change");
        let captured_head = ObjectId::from_hex(context.head_oid().as_bytes())
            .expect("fixture HEAD is an object ID");

        context.run_git(&["checkout", "main"]);
        context.checkout_new("later-stack");
        context.commit_with_gherrit_id("Later change");
        let repository = util::Repo::open(context.repo_path.to_str().unwrap()).unwrap();

        let stack = LocalStack::collect(
            &repository,
            "captured-stack",
            captured_head,
            &supplied_default,
            "origin",
        )
        .unwrap();

        assert_eq!(stack.tip(), captured_head);
        assert_eq!(
            stack.iter().map(|change| change.id().as_str()).collect::<Vec<_>>(),
            [captured_id.as_str()]
        );
    }

    #[test]
    fn ancestry_diagnostic_names_the_captured_branch_not_live_head() {
        let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
        let initial = util::Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        let default_tip = initial.rev_parse_single("refs/heads/main").unwrap().detach();
        let supplied_default = DefaultBranch::new("main".to_owned(), default_tip).unwrap();

        context.run_git(&["checkout", "--orphan", "captured-unrelated"]);
        context.commit_with_gherrit_id("Unrelated captured change");
        let captured_head = ObjectId::from_hex(context.head_oid().as_bytes())
            .expect("fixture HEAD is an object ID");

        context.run_git(&["checkout", "main"]);
        context.checkout_new("later-stack");
        let repository = util::Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        let error = LocalStack::collect(
            &repository,
            "captured-unrelated",
            captured_head,
            &supplied_default,
            "origin",
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("The branch 'captured-unrelated' does not descend from 'main'"));
        assert!(!error.contains("later-stack"));
    }

    #[test]
    fn stacks_require_unique_change_ids() {
        let error = LocalStack::new(
            default_branch("main", 10),
            vec![change("Gsame", 1, 10), change("Gsame", 2, 1)],
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "Stack contains multiple commits with gherrit-pr-id 'Gsame'");
    }

    #[test]
    fn stacks_require_one_contiguous_first_parent_path() {
        let stack = LocalStack::new(
            default_branch("main", 10),
            vec![change("Gone", 1, 10), change("Gtwo", 2, 1), change("Gthree", 3, 2)],
        )
        .unwrap();

        assert_eq!(
            stack.iter().map(|change| change.id().as_str()).collect::<Vec<_>>(),
            ["Gone", "Gtwo", "Gthree"]
        );

        let error = LocalStack::new(
            default_branch("main", 10),
            vec![change("Gone", 1, 10), change("Gtwo", 2, 10)],
        )
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

        let head = repository.head_snapshot().unwrap().target().unwrap();
        let stack =
            LocalStack::collect(&repository, "main", head, &supplied_default, "origin").unwrap();

        assert!(stack.is_empty());
    }

    #[test]
    fn stack_order_derives_root_parent_and_child_positions() {
        let stack = LocalStack::new(
            default_branch("main", 10),
            vec![change("Gone", 1, 10), change("Gtwo", 2, 1), change("Gthree", 3, 2)],
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
        let error =
            LocalStack::new(default_branch("main", 10), vec![change("main", 1, 10)]).unwrap_err();

        assert_eq!(
            error.to_string(),
            format!(
                "Commit {} has gherrit-pr-id 'main', which conflicts with the repository default branch",
                object_id(1)
            )
        );
    }

    #[test]
    fn stacks_retain_the_exact_default_path_origin() {
        let default = default_branch("trunk", 10);
        let stack = LocalStack::new(default.clone(), vec![change("Gone", 1, 10)]).unwrap();

        assert_eq!(stack.default_branch(), &default);
        assert_eq!(stack.iter().next().unwrap().head(), object_id(1));
        assert_eq!(stack.iter().next().unwrap().first_parent(), default.tip());
    }

    #[test]
    fn strips_only_the_matching_trailer_from_the_final_trailer_block() {
        let body = "Summary\n\ngherrit-pr-id: Gexample\n\nNotes\n\ngherrit-pr-id: Greal\n";

        assert_eq!(
            strip_gherrit_id(body, "Greal"),
            "Summary\n\ngherrit-pr-id: Gexample\n\nNotes\n\n\n"
        );
        assert_eq!(strip_gherrit_id(body, "Gmissing"), body);
        assert_eq!(strip_gherrit_id("Gherrit-Pr-Id: Greal\n", "Greal"), "\n");
    }

    #[test]
    fn former_metadata_prefix_is_ordinary_commit_text() {
        let body = "Keep <!-- gherrit-meta: arbitrary text --> exactly.\n\n\
                    gherrit-pr-id: Greal\n";

        assert_eq!(
            strip_gherrit_id(body, "Greal"),
            "Keep <!-- gherrit-meta: arbitrary text --> exactly.\n\n\n"
        );
    }
}
