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

use super::{autosquash, destination::DefaultBranch};
use crate::util;

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
        let title = PullRequestTitle::new(title)
            .wrap_err_with(|| format!("Commit {} has an invalid pull request title", commit.id))?;
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
        self.title.as_str()
    }

    pub(super) fn into_pull_request_content(self) -> (PullRequestTitle, String) {
        (self.title, self.body)
    }

    pub(super) fn body(&self) -> &str {
        &self.body
    }
}

/// An ordered, validated first-parent path from the default branch to `HEAD`.
#[derive(Debug)]
pub(super) struct LocalStack {
    default_branch: DefaultBranch,
    changes: Vec<LocalChange>,
}

impl LocalStack {
    /// Reads and validates the local managed stack without performing network
    /// writes.
    #[allow(dead_code)]
    pub(super) fn collect(repo: &util::Repo, default_branch: &DefaultBranch) -> Result<Self> {
        let head = repo.rev_parse_single("HEAD")?.detach();
        let branch_name = repo.current_branch().name().unwrap_or("current branch");
        Self::collect_captured(repo, branch_name, head, default_branch)
    }

    /// Reads and validates one captured local managed stack without re-reading
    /// its branch identity or `HEAD` target.
    pub(super) fn collect_captured(
        repo: &util::Repo,
        branch_name: &str,
        head: ObjectId,
        default_branch: &DefaultBranch,
    ) -> Result<Self> {
        let default_ref_name = default_branch.full_ref_name();
        let default_ref = repo.rev_parse_single(default_ref_name.as_str()).wrap_err_with(|| {
            format!("Local default branch '{}' is unavailable", default_branch.name())
        })?;
        if default_ref.detach() != default_branch.tip() {
            bail!(
                "Local default branch '{}' does not match the push repository",
                default_branch.name()
            );
        }
        if head == default_ref.detach() {
            return Self::new(default_branch.clone(), Vec::new());
        }

        repo.ensure_publishable_history()?;
        let captured_head_hex = head.to_string();
        let captured_head = repo.rev_parse_single(captured_head_hex.as_str())?;
        let commits = repo.first_parent_commits_between(default_ref, captured_head).map_err(|err| match err {
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
            default_branch,
        )?;

        let changes = commits
            .into_iter()
            .map(|(commit, title)| LocalChange::from_commit(commit, title))
            .collect::<Result<Vec<_>>>()?;

        let stack = Self::new(default_branch.clone(), changes)?;
        ensure_change_ids_unique_in_head_ancestry(repo, &stack, head)?;
        Ok(stack)
    }

    fn new(default_branch: DefaultBranch, changes: Vec<LocalChange>) -> Result<Self> {
        let ids = changes.iter().map(|change| change.id.as_str());
        ensure_unique_change_ids(ids)?;
        let default_ref = default_branch.full_ref_name();
        if let Some((change, managed_ref)) = changes.iter().find_map(|change| {
            let managed_ref = format!("refs/heads/{}", change.id.as_str());
            ref_is_equal_or_descendant(&default_ref, &managed_ref).then_some((change, managed_ref))
        }) {
            bail!(
                "Commit {} has gherrit-pr-id '{}', whose managed branch '{managed_ref}' conflicts with repository default branch '{default_ref}'",
                change.head,
                change.id.as_str(),
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

    pub(super) fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.changes.len()
    }

    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = &LocalChange> {
        self.changes.iter()
    }

    pub(super) fn into_changes(self) -> Vec<LocalChange> {
        self.changes
    }
}

/// Returns whether `ref_name` is `ancestor` or lies below it in the ref tree.
fn ref_is_equal_or_descendant(ref_name: &str, ancestor: &str) -> bool {
    ref_name == ancestor
        || ref_name.strip_prefix(ancestor).is_some_and(|suffix| suffix.starts_with('/'))
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
        let mut bytes = [byte; 20];
        if byte == 0 {
            bytes[19] = 1;
        }
        ObjectId::from_bytes_or_panic(&bytes)
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
            LocalStack::collect(&repository, &supplied_default)
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

        let head = repository.rev_parse_single("HEAD").unwrap().detach();
        let stack =
            LocalStack::collect_captured(&repository, "main", head, &supplied_default).unwrap();

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
    fn stack_ids_cannot_equal_or_be_ref_path_ancestors_of_the_default_branch() {
        [("main", "main"), ("release", "release/main"), ("release", "release/train/main")]
            .into_iter()
            .for_each(|(id, default)| {
                let error = LocalStack::new(
                    default_branch(default, 10),
                    vec![change(id, 1, 10)],
                )
                .unwrap_err();

                assert_eq!(
                    error.to_string(),
                    format!(
                        "Commit {} has gherrit-pr-id '{id}', whose managed branch 'refs/heads/{id}' conflicts with repository default branch 'refs/heads/{default}'",
                        object_id(1)
                    ),
                    "id={id:?}, default={default:?}",
                );
            });
    }

    #[test]
    fn stack_ids_only_conflict_at_a_ref_path_boundary() {
        [
            ("main", "mainline"),
            ("mainline", "main"),
            ("release", "releases/main"),
            ("main", "feature/main"),
            ("child", "parent/child"),
            ("Main", "main"),
        ]
        .into_iter()
        .for_each(|(id, default)| {
            LocalStack::new(default_branch(default, 10), vec![change(id, 1, 10)]).unwrap();
        });
    }

    #[test]
    fn captured_collection_uses_the_captured_branch_and_head_after_checkout_moves() {
        let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
        let repository = util::Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        let default_tip = repository.rev_parse_single("refs/heads/main").unwrap().detach();
        let default = DefaultBranch::new("main".to_owned(), default_tip).unwrap();

        context.run_git(&["checkout", "-b", "feature-a"]);
        let id_a = context.commit_with_gherrit_id("Feature A");
        let captured_head = repository.rev_parse_single("HEAD").unwrap().detach();
        context.run_git(&["checkout", "main"]);
        context.run_git(&["checkout", "-b", "feature-b"]);
        context.commit_with_gherrit_id("Feature B");

        let captured =
            LocalStack::collect_captured(&repository, "feature-a", captured_head, &default)
                .unwrap();
        assert_eq!(captured.iter().map(|change| change.id().as_str()).collect::<Vec<_>>(), [id_a]);
        assert_eq!(captured.iter().next().unwrap().head(), captured_head);
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

    #[test]
    fn former_metadata_prefix_is_ordinary_commit_text() {
        let body = "Keep <!-- gherrit-meta: arbitrary text --> exactly.\n\n\
                    gherrit-pr-id: Greal\n";
        let trailers = gherrit_id_trailers(body.as_bytes());
        let [GherritIdTrailer::Exact { line, .. }] = trailers.as_slice() else {
            panic!("expected one exact identity trailer")
        };

        assert_eq!(
            strip_gherrit_id(body, line.clone()),
            "Keep <!-- gherrit-meta: arbitrary text --> exactly.\n\n"
        );
    }
}
