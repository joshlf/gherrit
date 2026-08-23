//! Bounded pull-request content and frozen stack-level body recipes.
//!
//! Keeping the recipe at stack scope makes order, identity, navigation, and
//! number assignment one fact instead of independently supplied per-change
//! fields.

use std::{collections::HashSet, fmt};

use color_eyre::eyre::{Result, bail};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use super::MAX_BODY_SIZE_BYTES;
use crate::pre_push::{
    destination::{PushDestination, RepositoryCoordinates},
    history::{Revision, ValidatedChangeHistory},
    local::{GherritPrId, LocalChange, LocalStack, PullRequestTitle},
    pull_request::PullRequestNumber,
    version::Version,
};

const MAX_PENDING_PULL_REQUEST_NUMBER: u64 = i32::MAX as u64;

/// The selected repository and optional raw public branch used by body links.
///
/// Repository and branch provenance remains a planner obligation. The branch
/// stays raw here because the label and URL projections belong at the output
/// boundary and must always derive from the same value.
#[derive(Debug, Eq, PartialEq)]
pub(in crate::pre_push) struct BodyLinkContext {
    repository: RepositoryCoordinates,
    public_branch: Option<String>,
}

impl BodyLinkContext {
    /// Derives repository links from the exact selected push destination.
    pub(in crate::pre_push) fn from_destination(
        destination: &PushDestination,
        public_branch: Option<String>,
    ) -> Result<Self> {
        let repository = destination.repository_coordinates();
        Self::new(repository, public_branch)
    }

    fn new(repository: RepositoryCoordinates, public_branch: Option<String>) -> Result<Self> {
        if let Some(branch) = &public_branch {
            validate_public_branch(branch)?;
        }
        Ok(Self { repository, public_branch })
    }

    pub(in crate::pre_push) fn agrees_with(&self, destination: &PushDestination) -> bool {
        self.repository == destination.repository_coordinates()
    }
}

fn validate_public_branch(value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("A body recipe requires a nonempty public branch");
    }
    // Git excludes ASCII control bytes and DEL from ref names, and those bytes
    // could create body lines outside the branch link. It accepts UTF-8 C1
    // control scalars, however. Those are safe data here: the Markdown label
    // writes them literally and the URL projection percent-encodes their UTF-8
    // bytes. A Unicode-wide `char::is_control` check would therefore reject
    // valid Git branches without protecting either output grammar.
    if value.bytes().any(|byte| byte.is_ascii_control()) {
        bail!("A body recipe public branch must not contain ASCII control bytes");
    }
    Ok(())
}

/// Bytes which must be percent-encoded inside GitHub's branch path.
///
/// Slash remains a path separator because GitHub's tree route represents a
/// slash-delimited branch that way. Every byte other than RFC 3986 unreserved
/// data or slash is encoded, including non-ASCII UTF-8 bytes.
const GITHUB_TREE_BRANCH_PATH: &AsciiSet =
    &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~').remove(b'/');

fn write_markdown_text(output: &mut impl fmt::Write, value: &str) -> fmt::Result {
    value.chars().try_for_each(|character| {
        if character.is_ascii_punctuation() {
            output.write_char('\\')?;
        }
        output.write_char(character)
    })
}

/// Writes two projections of one validated raw branch at the presentation edge.
///
/// CommonMark permits a backslash escape before every ASCII punctuation
/// character, so one mechanical rule keeps the label literal. The GitHub tree
/// route instead receives an RFC 3986 path projection which retains `/` as the
/// branch hierarchy separator. Both projections stream into the caller's
/// bounded writer.
fn write_public_branch_link(output: &mut impl fmt::Write, branch: &str) -> fmt::Result {
    output.write_str("This PR is on branch [")?;
    write_markdown_text(output, branch)?;
    writeln!(output, "](../tree/{}).\n", utf8_percent_encode(branch, GITHUB_TREE_BRANCH_PATH),)
}

/// A generated body proven to fit GHerrit's product body limit.
#[derive(Debug, Eq, PartialEq)]
pub(in crate::pre_push) struct GeneratedBody(Box<str>);

impl GeneratedBody {
    #[cfg(test)]
    pub(in crate::pre_push) fn for_test(value: &str) -> Self {
        assert!(value.len() <= MAX_BODY_SIZE_BYTES, "test body must satisfy the product limit");
        Self(value.into())
    }

    pub(in crate::pre_push) fn as_str(&self) -> &str {
        &self.0
    }

    pub(in crate::pre_push) fn into_string(self) -> String {
        self.0.into()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NumberSource {
    Existing(PullRequestNumber),
    Missing,
}

/// One existing or missing PR number coupled to complete validated history.
///
/// The key is intentionally stored beside the number instead of accepting a
/// parallel number vector. Construction joins this key to both the ordered
/// local change and the history's retained key before the number can escape.
#[derive(Debug)]
pub(in crate::pre_push) struct BodyRecipeInput {
    id: GherritPrId,
    history: ValidatedChangeHistory,
    number: NumberSource,
}

impl BodyRecipeInput {
    pub(in crate::pre_push) fn existing(
        id: GherritPrId,
        history: ValidatedChangeHistory,
        number: PullRequestNumber,
    ) -> Result<Self> {
        Self::new(id, history, NumberSource::Existing(number))
    }

    pub(in crate::pre_push) fn missing(
        id: GherritPrId,
        history: ValidatedChangeHistory,
    ) -> Result<Self> {
        Self::new(id, history, NumberSource::Missing)
    }

    fn new(id: GherritPrId, history: ValidatedChangeHistory, number: NumberSource) -> Result<Self> {
        if history.id() != &id {
            bail!(
                "Body history for '{}' cannot be joined to keyed input '{}'",
                history.id().as_str(),
                id.as_str()
            );
        }
        Ok(Self { id, history, number })
    }
}

#[derive(Debug)]
struct JoinedInput {
    change: LocalChange,
    history: ValidatedChangeHistory,
    number: NumberSource,
}

/// One concrete generated body coupled to its change identity.
#[derive(Debug, Eq, PartialEq)]
pub(in crate::pre_push) struct RenderedBody {
    id: GherritPrId,
    body: GeneratedBody,
}

impl RenderedBody {
    pub(in crate::pre_push) fn id(&self) -> &GherritPrId {
        &self.id
    }

    #[cfg(test)]
    pub(in crate::pre_push) fn body(&self) -> &GeneratedBody {
        &self.body
    }

    pub(in crate::pre_push) fn into_parts(self) -> (GherritPrId, GeneratedBody) {
        (self.id, self.body)
    }
}

/// Concrete provisional bodies plus the one frozen final stack recipe.
#[derive(Debug)]
pub(in crate::pre_push) struct StackBodyRecipes {
    provisional: Box<[RenderedBody]>,
    final_bodies: FinalBodyRecipes,
}

impl StackBodyRecipes {
    pub(in crate::pre_push) fn new(
        context: BodyLinkContext,
        stack: LocalStack,
        inputs: Vec<BodyRecipeInput>,
    ) -> Result<Self> {
        let BodyLinkContext { repository, public_branch } = context;
        let repository_url = repository.relative_url();
        let changes = stack.into_changes();
        if changes.len() != inputs.len() {
            bail!(
                "Body recipe received {} histories for a {}-change local stack",
                inputs.len(),
                changes.len()
            );
        }
        let inputs = changes
            .into_iter()
            .zip(inputs)
            .map(|(change, input)| {
                if &input.id != change.id() {
                    bail!(
                        "Body input for '{}' cannot be joined to local change '{}'",
                        input.id.as_str(),
                        change.id().as_str()
                    );
                }
                debug_assert_eq!(input.history.id(), &input.id);
                let proposal = input.history.proposed();
                if proposal.head() != change.head()
                    || proposal.first_parent() != change.first_parent()
                {
                    bail!(
                        "Body history for '{}' does not retain the local proposal and first parent",
                        change.id().as_str()
                    );
                }
                Ok(JoinedInput { change, history: input.history, number: input.number })
            })
            .collect::<Result<Vec<_>>>()?;

        let mut existing_numbers = HashSet::new();
        for input in &inputs {
            if let NumberSource::Existing(number) = input.number
                && !existing_numbers.insert(number)
            {
                bail!("Body recipe repeats observed pull request number {}", number.get());
            }
        }

        let representative_numbers = inputs
            .iter()
            .map(|input| match input.number {
                NumberSource::Existing(number) => number,
                NumberSource::Missing => PullRequestNumber::new(MAX_PENDING_PULL_REQUEST_NUMBER)
                    .expect("GitHub GraphQL Int::MAX is a valid pull request number"),
            })
            .collect::<Vec<_>>();

        let mut layouts = Vec::with_capacity(inputs.len());
        let mut representative = Vec::with_capacity(inputs.len());
        for (index, input) in inputs.iter().enumerate() {
            let full = render_body(
                &repository_url,
                public_branch.as_deref(),
                input.change.id(),
                index,
                input.change.body(),
                &input.history,
                Navigation::Numbered(&representative_numbers),
                HistoryLayout::Full,
            );
            let (layout, body) = match full {
                Ok(body) => (HistoryLayout::Full, body),
                Err(BodyTooLarge) => {
                    let sparse = render_body(
                        &repository_url,
                        public_branch.as_deref(),
                        input.change.id(),
                        index,
                        input.change.body(),
                        &input.history,
                        Navigation::Numbered(&representative_numbers),
                        HistoryLayout::Sparse,
                    );
                    let body = sparse.map_err(|BodyTooLarge| {
                        color_eyre::eyre::eyre!(
                            "Generated pull request body for '{}' exceeds the {MAX_BODY_SIZE_BYTES}-byte limit even with sparse history",
                            input.change.id().as_str()
                        )
                    })?;
                    (HistoryLayout::Sparse, body)
                }
            };
            layouts.push(layout);
            representative.push(body);
        }

        let mut provisional = Vec::new();
        for (index, (input, layout)) in inputs.iter().zip(&layouts).enumerate() {
            if input.number != NumberSource::Missing {
                continue;
            }
            let body = render_body(
                &repository_url,
                public_branch.as_deref(),
                input.change.id(),
                index,
                input.change.body(),
                &input.history,
                Navigation::Omitted,
                *layout,
            )
            .map_err(|BodyTooLarge| {
                color_eyre::eyre::eyre!(
                    "Provisional pull request body for '{}' unexpectedly exceeds its bounded widest final render",
                    input.change.id().as_str()
                )
            })?;
            provisional.push(RenderedBody { id: input.change.id().clone(), body });
        }

        let entries = inputs
            .into_iter()
            .zip(layouts)
            .zip(representative)
            .map(|((input, layout), representative)| {
                let (id, title, commit_body) = input.change.into_body_parts();
                FinalBodyRecipe {
                    id,
                    title,
                    commit_body,
                    history: input.history,
                    number: input.number,
                    layout,
                    representative,
                }
            })
            .collect();
        let final_bodies = FinalBodyRecipes { repository_url, public_branch, entries };
        Ok(Self { provisional: provisional.into_boxed_slice(), final_bodies })
    }

    #[cfg(test)]
    pub(in crate::pre_push) fn provisional_bodies(&self) -> &[RenderedBody] {
        &self.provisional
    }

    pub(in crate::pre_push) fn final_bodies(&self) -> &FinalBodyRecipes {
        &self.final_bodies
    }

    pub(in crate::pre_push) fn into_parts(self) -> (Box<[RenderedBody]>, FinalBodyRecipes) {
        (self.provisional, self.final_bodies)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryLayout {
    Full,
    Sparse,
}

#[derive(Debug)]
struct FinalBodyRecipe {
    id: GherritPrId,
    title: PullRequestTitle,
    commit_body: String,
    history: ValidatedChangeHistory,
    number: NumberSource,
    layout: HistoryLayout,
    representative: GeneratedBody,
}

/// One immutable ordered recipe whose only unresolved facts are missing PR
/// numbers.
#[derive(Debug)]
pub(in crate::pre_push) struct FinalBodyRecipes {
    repository_url: String,
    public_branch: Option<String>,
    entries: Box<[FinalBodyRecipe]>,
}

impl FinalBodyRecipes {
    /// Validated title text in exact local stack order.
    pub(in crate::pre_push) fn titles(
        &self,
    ) -> impl ExactSizeIterator<Item = (&GherritPrId, &PullRequestTitle)> {
        self.entries.iter().map(|entry| (&entry.id, &entry.title))
    }

    /// Widest concrete bodies used for conservative GraphQL preflight.
    pub(in crate::pre_push) fn representative_bodies(
        &self,
    ) -> impl ExactSizeIterator<Item = (&GherritPrId, &GeneratedBody)> {
        self.entries.iter().map(|entry| (&entry.id, &entry.representative))
    }

    /// Consumes the exact missing-number assignment in local stack order.
    pub(in crate::pre_push) fn complete(
        self,
        assignments: impl IntoIterator<Item = (GherritPrId, PullRequestNumber)>,
    ) -> Result<Box<[RenderedBody]>> {
        let mut assignments = assignments.into_iter();
        let mut numbers = Vec::with_capacity(self.entries.len());
        let mut seen_numbers = HashSet::with_capacity(self.entries.len());
        for (index, entry) in self.entries.iter().enumerate() {
            let number = match entry.number {
                NumberSource::Existing(number) => number,
                NumberSource::Missing => {
                    let Some((id, number)) = assignments.next() else {
                        bail!(
                            "Final body number assignment is missing change '{}' at stack position {index}",
                            entry.id.as_str()
                        );
                    };
                    if id != entry.id {
                        bail!(
                            "Final body number assignment has change '{}' at stack position {index}, expected '{}'",
                            id.as_str(),
                            entry.id.as_str()
                        );
                    }
                    number
                }
            };
            if !seen_numbers.insert(number) {
                bail!("Final body number assignment repeats pull request number {}", number.get());
            }
            numbers.push(number);
        }
        if let Some((extra, _)) = assignments.next() {
            bail!(
                "Final body number assignment has unexpected change '{}' after the local stack",
                extra.as_str()
            );
        }

        self.entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let body = render_body(
                    &self.repository_url,
                    self.public_branch.as_deref(),
                    &entry.id,
                    index,
                    &entry.commit_body,
                    &entry.history,
                    Navigation::Numbered(&numbers),
                    entry.layout,
                )
                .map_err(|BodyTooLarge| {
                    color_eyre::eyre::eyre!(
                        "Generated pull request body for '{}' exceeds the {MAX_BODY_SIZE_BYTES}-byte limit",
                        entry.id.as_str()
                    )
                })?;
                Ok(RenderedBody { id: entry.id.clone(), body })
            })
            .collect::<Result<Box<[_]>>>()
    }
}

#[derive(Clone, Copy)]
enum Navigation<'numbers> {
    Omitted,
    Numbered(&'numbers [PullRequestNumber]),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BodyTooLarge;

#[allow(clippy::too_many_arguments)]
fn render_body(
    repository_url: &str,
    public_branch: Option<&str>,
    id: &GherritPrId,
    current_index: usize,
    commit_body: &str,
    history: &ValidatedChangeHistory,
    navigation: Navigation<'_>,
    history_layout: HistoryLayout,
) -> std::result::Result<GeneratedBody, BodyTooLarge> {
    let mut output = BoundedWriter::new();
    write_body(
        &mut output,
        repository_url,
        public_branch,
        id,
        current_index,
        commit_body,
        history,
        navigation,
        history_layout,
    )
    .map_err(|_| BodyTooLarge)?;
    Ok(output.finish())
}

#[allow(clippy::too_many_arguments)]
fn write_body(
    output: &mut impl fmt::Write,
    repository_url: &str,
    public_branch: Option<&str>,
    id: &GherritPrId,
    current_index: usize,
    commit_body: &str,
    history: &ValidatedChangeHistory,
    navigation: Navigation<'_>,
    history_layout: HistoryLayout,
) -> fmt::Result {
    output.write_str(
        "<!-- WARNING: This PR description is automatically generated by GHerrit. Any manual edits will be overwritten on the next push. -->\n\n",
    )?;
    output.write_str(commit_body)?;
    output.write_str("\n\n---\n\n")?;
    write_navigation(output, public_branch, current_index, navigation)?;
    write_history(output, repository_url, id, history, history_layout)?;
    write_download(output, id)?;
    output.write_str("\n\n")?;
    output.write_str("*Stacked PRs enabled by [GHerrit](https://github.com/joshlf/gherrit).*")
}

fn write_navigation(
    output: &mut impl fmt::Write,
    public_branch: Option<&str>,
    current_index: usize,
    navigation: Navigation<'_>,
) -> fmt::Result {
    if let Some(branch) = public_branch {
        write_public_branch_link(output, branch)?;
    }

    let Navigation::Numbered(numbers) = navigation else {
        return Ok(());
    };
    numbers.iter().enumerate().rev().try_for_each(|(index, number)| {
        let prefix = if index == current_index { "👉" } else { "\u{3000}\u{2009}" };
        writeln!(output, "- {prefix} #{}", number.get())
    })
}

fn write_history(
    output: &mut impl fmt::Write,
    repository_url: &str,
    id: &GherritPrId,
    history: &ValidatedChangeHistory,
    layout: HistoryLayout,
) -> fmt::Result {
    let latest = history.projected_current().number().get();
    if latest == 1 {
        return Ok(());
    }

    write_history_table(
        output,
        repository_url,
        id,
        latest,
        history.projected_versions().rev(),
        layout,
    )
}

fn write_history_table(
    output: &mut impl fmt::Write,
    repository_url: &str,
    id: &GherritPrId,
    latest: u64,
    rows: impl Iterator<Item = (Version, Revision)>,
    layout: HistoryLayout,
) -> fmt::Result {
    write!(
        output,
        "\n\n**Latest Update:** v{latest} — [Compare vs v{}]({repository_url}/compare/gherrit/{}/v{}..gherrit/{}/v{latest})\n\n",
        latest - 1,
        id.as_str(),
        latest - 1,
        id.as_str(),
    )?;
    output.write_str("<details>\n<summary><strong>📚 Full Patch History</strong></summary>\n\n")?;
    output
        .write_str("*Links show the diff between the row version and the column version.*\n\n")?;

    output.write_str("|Version|")?;
    for version in (1..latest).rev() {
        write!(output, " v{version} |")?;
    }
    output.write_str("Base|")?;

    output.write_str("\n|:---|")?;
    for _ in 1..latest {
        output.write_str(":---|")?;
    }
    output.write_str(":---|\n")?;

    let prefix = if latest <= 8 { "vs " } else { "" };
    for (version, revision) in rows {
        let row = version.get();
        write!(output, "|v{row}|")?;
        for column in (1..latest).rev() {
            if column >= row {
                output.write_str("|")?;
                continue;
            }
            let show = match layout {
                HistoryLayout::Full => true,
                HistoryLayout::Sparse => row == latest || row == column + 1,
            };
            if show {
                write!(
                    output,
                    "[{prefix}v{column}]({repository_url}/compare/gherrit/{}/v{column}..gherrit/{}/v{row})|",
                    id.as_str(),
                    id.as_str(),
                )?;
            } else {
                output.write_str("|")?;
            }
        }
        writeln!(
            output,
            "[{prefix}Base]({repository_url}/compare/{}...gherrit/{}/v{row})|",
            revision.first_parent(),
            id.as_str(),
        )?;
    }
    output.write_str("\n</details>")
}

fn write_download(output: &mut impl fmt::Write, id: &GherritPrId) -> fmt::Result {
    output.write_str("\n<details>\n<summary><strong>⬇️ Download this PR</strong></summary>\n\n")?;
    output.write_str("######\n\n")?;
    let id = id.as_str();
    writeln!(
        output,
        "**Branch**\n```bash\ngit fetch origin refs/heads/{id} && git checkout -b pr-{id} FETCH_HEAD\n```\n"
    )?;
    writeln!(
        output,
        "**Checkout**\n```bash\ngit fetch origin refs/heads/{id} && git checkout FETCH_HEAD\n```\n"
    )?;
    writeln!(
        output,
        "**Cherry Pick**\n```bash\ngit fetch origin refs/heads/{id} && git cherry-pick FETCH_HEAD\n```\n"
    )?;
    writeln!(output, "**Pull**\n```bash\ngit pull origin refs/heads/{id}\n```\n")?;
    output.write_str("</details>")
}

struct BoundedWriter {
    output: String,
}

impl BoundedWriter {
    fn new() -> Self {
        Self { output: String::new() }
    }

    fn finish(self) -> GeneratedBody {
        debug_assert!(self.output.len() <= MAX_BODY_SIZE_BYTES);
        GeneratedBody(self.output.into_boxed_str())
    }
}

impl fmt::Write for BoundedWriter {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let remaining = MAX_BODY_SIZE_BYTES - self.output.len();
        if value.len() > remaining {
            return Err(fmt::Error);
        }
        self.output.push_str(value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, fmt::Write as _};

    use gix::ObjectId;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        pre_push::{
            destination::DefaultBranch,
            history::{CommitGraphEvidence, NormalizedPublishedHistory},
            local::LocalStack,
            remote,
        },
        util,
    };

    fn change_id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).expect("valid test change ID")
    }

    fn number(value: u64) -> PullRequestNumber {
        PullRequestNumber::new(value).expect("valid test pull request number")
    }

    fn missing_input(history: ValidatedChangeHistory) -> BodyRecipeInput {
        let id = history.id().clone();
        BodyRecipeInput::missing(id, history).unwrap()
    }

    fn existing_input(history: ValidatedChangeHistory, value: u64) -> BodyRecipeInput {
        let id = history.id().clone();
        BodyRecipeInput::existing(id, history, number(value)).unwrap()
    }

    struct TestRepository {
        directory: TempDir,
        writer: gix::Repository,
    }

    impl TestRepository {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("temporary repository directory");
            let writer = gix::init_bare(directory.path()).expect("initialize bare repository");
            Self { directory, writer }
        }

        fn commit(&self, subject: &str, parents: &[ObjectId], id: Option<&str>) -> ObjectId {
            let message = id.map_or_else(
                || subject.to_owned(),
                |id| format!("{subject}\n\ngherrit-pr-id: {id}\n"),
            );
            let signature = gix::actor::Signature {
                name: "GHerrit body test".into(),
                email: "body@example.com".into(),
                time: gix::actor::date::Time::new(0, 0),
            };
            self.writer
                .write_object(&gix::objs::Commit {
                    tree: ObjectId::empty_tree(self.writer.object_hash()),
                    parents: parents.iter().copied().collect(),
                    author: signature.clone(),
                    committer: signature,
                    encoding: None,
                    message: message.into(),
                    extra_headers: Vec::new(),
                })
                .expect("write test commit")
                .detach()
        }

        fn open(&self) -> util::Repo {
            util::Repo::open(self.directory.path().to_str().expect("UTF-8 test path"))
                .expect("open test repository")
        }
    }

    fn graph(
        repository: &TestRepository,
        roots: impl IntoIterator<Item = ObjectId>,
    ) -> CommitGraphEvidence {
        CommitGraphEvidence::load(&repository.open(), roots).expect("complete literal test graph")
    }

    fn validated_history(
        graph: &CommitGraphEvidence,
        change: &LocalChange,
        published: &[(ObjectId, ObjectId)],
    ) -> ValidatedChangeHistory {
        let default = ObjectId::from_bytes_or_panic(&[0xf0; 20]);
        let mut local = format!("{default}\trefs/heads/main\n");
        if let Some((head, first_parent)) = published.last() {
            writeln!(local, "{head}\trefs/heads/{}", change.id().as_str()).unwrap();
            writeln!(local, "{first_parent}\trefs/heads/gherrit-bases/{}", change.id().as_str())
                .unwrap();
        }
        let versions = published
            .iter()
            .enumerate()
            .map(|(index, (head, _))| {
                format!("{head}\trefs/tags/gherrit/{}/v{}\n", change.id().as_str(), index + 1)
            })
            .collect::<String>();
        local.push_str(&versions);
        let observed = remote::parse_active_change_for_test(
            change.id().clone(),
            DefaultBranch::new("main".to_owned(), default).unwrap(),
            local.as_bytes(),
        )
        .expect("complete remote history observation");
        NormalizedPublishedHistory::from_observation(observed, graph)
            .expect("normalized literal history")
            .with_proposal(change, graph)
            .expect("history and local proposal agree")
            .validate(graph, None)
            .expect("safe complete change history")
    }

    struct StackFixture {
        stack: LocalStack,
        histories: Vec<ValidatedChangeHistory>,
    }

    fn stack_history_fixture(entries: &[(&str, &str, &str, usize)]) -> StackFixture {
        assert!(entries.iter().all(|(_, _, _, versions)| *versions > 0));
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], None);
        let published = entries
            .iter()
            .map(|(id, _, _, versions)| {
                (1..*versions)
                    .map(|version| {
                        let base = repository.commit(
                            &format!("historic base {id} v{version}"),
                            &[root],
                            None,
                        );
                        let head = repository.commit(
                            &format!("historic change {id} v{version}"),
                            &[base],
                            Some(id),
                        );
                        (head, base)
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let mut parent = root;
        let proposals = entries
            .iter()
            .map(|(id, _, _, _)| {
                let head = repository.commit(&format!("proposal {id}"), &[parent], Some(id));
                let revision = (head, parent);
                parent = head;
                revision
            })
            .collect::<Vec<_>>();
        let roots = std::iter::once(parent)
            .chain(published.iter().flat_map(|revisions| revisions.iter().map(|(head, _)| *head)));
        let graph = graph(&repository, roots);
        let stack = LocalStack::for_test_with_content(
            root,
            entries.iter().zip(&proposals).map(|((id, title, body, _), (head, _))| {
                (change_id(id), *head, (*title).to_owned(), (*body).to_owned())
            }),
        )
        .expect("valid literal test stack");
        let histories = stack
            .iter()
            .zip(&published)
            .map(|(change, revisions)| validated_history(&graph, change, revisions))
            .collect();
        StackFixture { stack, histories }
    }

    fn stack_fixture(entries: &[(&str, &str, &str)]) -> (LocalStack, Vec<ValidatedChangeHistory>) {
        let entries =
            entries.iter().map(|(id, title, body)| (*id, *title, *body, 1)).collect::<Vec<_>>();
        let fixture = stack_history_fixture(&entries);
        (fixture.stack, fixture.histories)
    }

    struct AmendFixture {
        stack: LocalStack,
        history: ValidatedChangeHistory,
        revisions: Vec<(ObjectId, ObjectId)>,
    }

    fn amend_fixture(id: &str, body: &str, projected_versions: usize) -> AmendFixture {
        assert!(projected_versions > 0);
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], None);
        let revisions = (1..=projected_versions)
            .map(|version| {
                let base = repository.commit(&format!("base {version}"), &[root], None);
                let head = repository.commit(&format!("revision {version}"), &[base], Some(id));
                (head, base)
            })
            .collect::<Vec<_>>();
        let graph = graph(&repository, revisions.iter().map(|(head, _)| *head));
        let (proposal, proposal_parent) = *revisions.last().unwrap();
        let stack = LocalStack::for_test_with_content(
            proposal_parent,
            [(change_id(id), proposal, "Validated title".to_owned(), body.to_owned())],
        )
        .expect("valid amended local change");
        let history = validated_history(
            &graph,
            stack.iter().next().unwrap(),
            &revisions[..revisions.len() - 1],
        );
        AmendFixture { stack, history, revisions }
    }

    fn repeated_history_fixture(body: &str) -> AmendFixture {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], None);
        let base = repository.commit("base", &[root], None);
        let a = repository.commit("revision A", &[base], Some("Grepeat"));
        let b = repository.commit("revision B", &[base], Some("Grepeat"));
        let graph = graph(&repository, [a, b]);
        let stack = LocalStack::for_test_with_content(
            base,
            [(change_id("Grepeat"), a, "Repeated revision".to_owned(), body.to_owned())],
        )
        .expect("valid repeated local revision");
        let revisions = vec![(a, base), (a, base), (b, base), (a, base)];
        let history = validated_history(&graph, stack.iter().next().unwrap(), revisions.as_slice());
        AmendFixture { stack, history, revisions }
    }

    fn missing_recipes(
        repository_url: &str,
        public_branch: Option<&str>,
        stack: LocalStack,
        histories: Vec<ValidatedChangeHistory>,
    ) -> Result<StackBodyRecipes> {
        debug_assert_eq!(repository_url, "/octo/widgets");
        let destination =
            PushDestination::for_test("origin", "https://github.com/octo/widgets.git", Vec::new())?;
        let context =
            BodyLinkContext::from_destination(&destination, public_branch.map(str::to_owned))?;
        StackBodyRecipes::new(context, stack, histories.into_iter().map(missing_input).collect())
    }

    fn link_context(repository_url: &str, public_branch: Option<&str>) -> BodyLinkContext {
        debug_assert_eq!(repository_url, "/octo/widgets");
        let destination =
            PushDestination::for_test("origin", "https://github.com/octo/widgets.git", Vec::new())
                .unwrap();
        BodyLinkContext::from_destination(&destination, public_branch.map(str::to_owned)).unwrap()
    }

    fn rendered_report(bodies: &[RenderedBody]) -> String {
        let mut report = String::new();
        for body in bodies {
            writeln!(report, "===== {} =====", body.id().as_str()).unwrap();
            writeln!(report, "{}", body.body().as_str()).unwrap();
        }
        report
    }

    fn complete_missing(
        recipes: StackBodyRecipes,
        assignments: &[(&str, u64)],
    ) -> Result<Box<[RenderedBody]>> {
        let (_, final_bodies) = recipes.into_parts();
        final_bodies
            .complete(assignments.iter().map(|(id, number)| (change_id(id), self::number(*number))))
    }

    fn single_missing_recipes(id: &str, body: &str, versions: usize) -> Result<StackBodyRecipes> {
        let fixture = stack_history_fixture(&[(id, "Validated title", body, versions)]);
        missing_recipes("/octo/widgets", None, fixture.stack, fixture.histories)
    }

    fn empty_single_render(id: &str, versions: usize, layout: HistoryLayout) -> GeneratedBody {
        let fixture = stack_history_fixture(&[(id, "Validated title", "", versions)]);
        let history = &fixture.histories[0];
        let numbers = [number(MAX_PENDING_PULL_REQUEST_NUMBER)];
        render_body(
            "/octo/widgets",
            None,
            history.id(),
            0,
            "",
            history,
            Navigation::Numbered(&numbers),
            layout,
        )
        .unwrap()
    }

    fn mixed_recipes() -> StackBodyRecipes {
        let (stack, histories) = stack_fixture(&[
            ("Gknown", "Known", "Known body."),
            ("Gnew1", "New one", "First new body."),
            ("Gnew2", "New two", "Second new body."),
        ]);
        let mut histories = histories.into_iter();
        StackBodyRecipes::new(
            link_context("/octo/widgets", None),
            stack,
            vec![
                existing_input(histories.next().unwrap(), 11),
                missing_input(histories.next().unwrap()),
                missing_input(histories.next().unwrap()),
            ],
        )
        .unwrap()
    }

    fn normalize_object_ids(mut body: String, labels: &[(ObjectId, &str)]) -> String {
        for (oid, label) in labels {
            body = body.replace(&oid.to_string(), label);
        }
        body
    }

    #[test]
    fn provisional_private_stack_snapshot_has_no_numbered_navigation() {
        let ids = ["Groot", "Gmiddle", "Gtip"];
        let (stack, histories) = stack_fixture(&[
            (
                ids[0],
                "Root title",
                "Introduce the root.\n\n<!-- gherrit-meta: ordinary commit text -->\n\nExplain it.",
            ),
            (ids[1], "Middle title", "Build on the root."),
            (ids[2], "Tip title", "Finish the stack."),
        ]);
        let recipes = missing_recipes("/octo/widgets", None, stack, histories).unwrap();

        for body in recipes.provisional_bodies() {
            assert!(!body.body().as_str().contains("\n- "));
        }
        insta::assert_snapshot!(
            "bounded_provisional_private_stack",
            rendered_report(recipes.provisional_bodies())
        );
    }

    #[test]
    fn provisional_public_stack_snapshot_has_no_numbered_navigation() {
        let ids = ["Groot", "Gmiddle", "Gtip"];
        let (stack, histories) = stack_fixture(&[
            (ids[0], "Root title", "Introduce the root."),
            (ids[1], "Middle title", "Build on the root."),
            (ids[2], "Tip title", "Finish the stack."),
        ]);
        let recipes =
            missing_recipes("/octo/widgets", Some("feature/public-stack"), stack, histories)
                .unwrap();

        for body in recipes.provisional_bodies() {
            assert!(!body.body().as_str().contains("\n- "));
            assert!(body.body().as_str().contains("feature/public-stack"));
        }
        insta::assert_snapshot!(
            "bounded_provisional_public_stack",
            rendered_report(recipes.provisional_bodies())
        );
    }

    #[test]
    fn complete_final_stack_snapshot_uses_existing_and_assigned_numbers() {
        let ids = ["Groot", "Gmiddle", "Gtip"];
        let (stack, histories) = stack_fixture(&[
            (ids[0], "Root title", "Introduce the root."),
            (ids[1], "Middle title", "Build on the root."),
            (ids[2], "Tip title", "Finish the stack."),
        ]);
        let mut histories = histories.into_iter();
        let inputs = vec![
            existing_input(histories.next().unwrap(), 11),
            missing_input(histories.next().unwrap()),
            missing_input(histories.next().unwrap()),
        ];
        let recipes =
            StackBodyRecipes::new(link_context("/octo/widgets", None), stack, inputs).unwrap();
        assert_eq!(recipes.provisional_bodies().len(), 2);
        assert_eq!(
            recipes
                .final_bodies()
                .titles()
                .map(|(id, title)| (id.as_str(), title.as_str()))
                .collect::<Vec<_>>(),
            [("Groot", "Root title"), ("Gmiddle", "Middle title"), ("Gtip", "Tip title")]
        );

        let (_, final_bodies) = recipes.into_parts();
        let bodies = final_bodies
            .complete([(change_id("Gmiddle"), number(22)), (change_id("Gtip"), number(33))])
            .unwrap();
        for (index, body) in bodies.iter().enumerate() {
            for number in [11, 22, 33] {
                assert!(body.body().as_str().contains(&format!("#{number}")));
            }
            assert_eq!(body.body().as_str().matches("👉").count(), 1);
            assert!(body.body().as_str().contains(&format!("👉 #{}", [11, 22, 33][index])));
        }
        insta::assert_snapshot!("bounded_complete_final_stack", rendered_report(&bodies));
    }

    #[test]
    fn bounded_writer_accepts_the_exact_limit_and_rejects_atomic_overflow() {
        let oversized = "x".repeat(MAX_BODY_SIZE_BYTES + 1);
        let mut rejected = BoundedWriter::new();
        assert!(rejected.write_str(&oversized).is_err());
        assert!(rejected.output.is_empty());
        assert_eq!(rejected.output.capacity(), 0);

        let exact = "x".repeat(MAX_BODY_SIZE_BYTES);
        let mut accepted = BoundedWriter::new();
        accepted.write_str(&exact).unwrap();
        let body = accepted.finish();
        assert_eq!(body.as_str().len(), MAX_BODY_SIZE_BYTES);
    }

    #[test]
    fn generated_body_accepts_exactly_the_limit_and_rejects_the_next_byte() {
        let fixture = stack_history_fixture(&[("Gexact", "Exact", "", 1)]);
        let history = &fixture.histories[0];
        let numbers = [number(MAX_PENDING_PULL_REQUEST_NUMBER)];
        let fixed = render_body(
            "/octo/widgets",
            None,
            history.id(),
            0,
            "",
            history,
            Navigation::Numbered(&numbers),
            HistoryLayout::Full,
        )
        .unwrap()
        .as_str()
        .len();
        let exact_padding = "x".repeat(MAX_BODY_SIZE_BYTES - fixed);
        let exact = render_body(
            "/octo/widgets",
            None,
            history.id(),
            0,
            &exact_padding,
            history,
            Navigation::Numbered(&numbers),
            HistoryLayout::Full,
        )
        .unwrap();
        assert_eq!(exact.as_str().len(), MAX_BODY_SIZE_BYTES);

        let over_padding = format!("{exact_padding}x");
        assert!(
            render_body(
                "/octo/widgets",
                None,
                history.id(),
                0,
                &over_padding,
                history,
                Navigation::Numbered(&numbers),
                HistoryLayout::Full,
            )
            .is_err()
        );
    }

    #[test]
    fn oversized_user_body_and_large_history_fail_inside_the_bound() {
        let huge_body = "x".repeat(MAX_BODY_SIZE_BYTES * 4);
        let (stack, histories) = stack_fixture(&[("Ghugebody", "Huge body", &huge_body)]);
        let error = missing_recipes("/octo/widgets", None, stack, histories).unwrap_err();
        assert!(error.to_string().contains("exceeds the 131072-byte limit"));

        let fixture = stack_history_fixture(&[("Ghugehistory", "Huge history", "", 512)]);
        assert_eq!(fixture.histories[0].projected_versions().len(), 512);
        let error =
            missing_recipes("/octo/widgets", None, fixture.stack, fixture.histories).unwrap_err();
        assert!(error.to_string().contains("even with sparse history"));
    }

    #[test]
    fn reverse_history_is_zero_copy_and_bounded_writing_stops_before_rows() {
        fn require_reverse_exact(
            iterator: impl DoubleEndedIterator<Item = (Version, Revision)> + ExactSizeIterator,
        ) -> usize {
            iterator.len()
        }

        let fixture = amend_fixture("Gstream", "", 3);
        assert_eq!(require_reverse_exact(fixture.history.projected_versions()), 3);
        let revision = fixture.history.proposed();
        let visited = Cell::new(0usize);
        let rows = (0..100_000).rev().map(|index| {
            visited.set(visited.get() + 1);
            (Version::from_history_index(index).unwrap(), revision)
        });
        let mut output = BoundedWriter::new();
        assert!(
            write_history_table(
                &mut output,
                "/octo/widgets",
                &change_id("Gstream"),
                100_000,
                rows,
                HistoryLayout::Sparse,
            )
            .is_err()
        );
        assert_eq!(visited.get(), 0, "history rows were traversed before the bound failed");
        assert!(output.output.len() <= MAX_BODY_SIZE_BYTES);
    }

    #[test]
    fn keyed_inputs_reject_wrong_identity_before_recipe_construction() {
        let fixture = amend_fixture("Gactual", "", 1);
        let error = BodyRecipeInput::missing(change_id("Gwrong"), fixture.history).unwrap_err();
        assert!(error.to_string().contains("cannot be joined to keyed input 'Gwrong'"));
    }

    #[test]
    fn construction_rejects_count_order_and_duplicate_known_numbers() {
        let (stack, mut histories) =
            stack_fixture(&[("Gone", "One", "One."), ("Gtwo", "Two", "Two.")]);
        histories.pop();
        let error = StackBodyRecipes::new(
            link_context("/octo/widgets", None),
            stack,
            histories.into_iter().map(missing_input).collect(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("1 histories for a 2-change local stack"));

        let (stack, histories) = stack_fixture(&[("Gone", "One", "One."), ("Gtwo", "Two", "Two.")]);
        let (_, mut extra) = stack_fixture(&[("Gextra", "Extra", "Extra.")]);
        let mut inputs = histories.into_iter().map(missing_input).collect::<Vec<_>>();
        inputs.push(missing_input(extra.pop().unwrap()));
        let error =
            StackBodyRecipes::new(link_context("/octo/widgets", None), stack, inputs).unwrap_err();
        assert!(error.to_string().contains("3 histories for a 2-change local stack"));

        let (stack, mut histories) =
            stack_fixture(&[("Gone", "One", "One."), ("Gtwo", "Two", "Two.")]);
        histories.reverse();
        let error = StackBodyRecipes::new(
            link_context("/octo/widgets", None),
            stack,
            histories.into_iter().map(missing_input).collect(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot be joined to local change 'Gone'"));

        let (stack, histories) = stack_fixture(&[("Gone", "One", "One."), ("Gtwo", "Two", "Two.")]);
        let inputs = histories.into_iter().map(|history| existing_input(history, 11)).collect();
        let error =
            StackBodyRecipes::new(link_context("/octo/widgets", None), stack, inputs).unwrap_err();
        assert!(error.to_string().contains("repeats observed pull request number 11"));
    }

    fn proposal_mismatch(rebased: bool) -> (LocalStack, ValidatedChangeHistory) {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], None);
        let alternate_base = repository.commit("alternate base", &[root], None);
        let original = repository.commit("original", &[root], Some("Gsame"));
        let alternate_parent = if rebased { alternate_base } else { root };
        let alternate = repository.commit("alternate", &[alternate_parent], Some("Gsame"));
        let graph = graph(&repository, [original, alternate]);
        let original_stack = LocalStack::for_test_with_content(
            root,
            [(change_id("Gsame"), original, "Original".to_owned(), String::new())],
        )
        .unwrap();
        let history = validated_history(&graph, original_stack.iter().next().unwrap(), &[]);
        let alternate_stack = LocalStack::for_test_with_content(
            alternate_parent,
            [(change_id("Gsame"), alternate, "Alternate".to_owned(), String::new())],
        )
        .unwrap();
        (alternate_stack, history)
    }

    #[test]
    fn construction_rejects_same_id_amend_and_rebase_history_laundering() {
        for rebased in [false, true] {
            let (stack, history) = proposal_mismatch(rebased);
            let change = stack.iter().next().unwrap();
            assert_ne!(history.proposed().head(), change.head());
            if rebased {
                assert_ne!(history.proposed().first_parent(), change.first_parent());
            } else {
                assert_eq!(history.proposed().first_parent(), change.first_parent());
            }
            let input = missing_input(history);
            let error =
                StackBodyRecipes::new(link_context("/octo/widgets", None), stack, vec![input])
                    .unwrap_err();
            assert!(
                error.to_string().contains("does not retain the local proposal and first parent")
            );
        }
    }

    #[test]
    fn construction_is_all_or_none_when_a_late_entry_is_oversized() {
        let huge = "x".repeat(MAX_BODY_SIZE_BYTES + 1);
        let (stack, histories) =
            stack_fixture(&[("Gfirst", "First", "This entry fits."), ("Glate", "Late", &huge)]);
        let error = missing_recipes("/octo/widgets", None, stack, histories).unwrap_err();
        assert!(error.to_string().contains("'Glate'"));
        assert!(error.to_string().contains("even with sparse history"));
    }

    #[test]
    fn public_branch_links_escape_markdown_and_encode_url_paths() {
        let mut rendered = String::new();
        for branch in [
            "main",
            "feature/public-stack",
            "release_(candidate)",
            "fix]docs",
            "hash#fragment",
            "percent%2Fslash",
            "amp&copy;",
            "tick`name",
            "angle<tag>",
            "paren)tail",
            "café/東京",
            "feature/🚀",
        ] {
            let full_name = format!("refs/heads/{branch}");
            gix::refs::FullName::try_from(full_name.as_str())
                .expect("the fixture must be a valid Git ref name");
            writeln!(rendered, "===== {branch:?} =====").unwrap();
            write_public_branch_link(&mut rendered, branch).unwrap();
        }
        rendered.push_str("===== END =====\n");
        insta::assert_snapshot!("public_branch_links_are_data", rendered);
    }

    #[test]
    fn public_branch_link_encoders_cover_their_exact_ascii_classes() {
        for byte in b' '..=b'~' {
            let character = char::from(byte);
            let mut actual = String::new();
            write_markdown_text(&mut actual, &character.to_string()).unwrap();
            let expected = if character.is_ascii_punctuation() {
                format!("\\{character}")
            } else {
                character.to_string()
            };
            assert_eq!(actual, expected, "Markdown byte {byte:#04x}");
        }

        for byte in 0..=0x7f {
            let value = char::from(byte).to_string();
            let actual = utf8_percent_encode(&value, GITHUB_TREE_BRANCH_PATH).to_string();
            let expected = if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
                value
            } else {
                format!("%{byte:02X}")
            };
            assert_eq!(actual, expected, "URL byte {byte:#04x}");
        }
        assert_eq!(utf8_percent_encode("é", GITHUB_TREE_BRANCH_PATH).to_string(), "%C3%A9");
    }

    #[test]
    fn public_branch_validation_matches_git_control_domain() {
        let destination =
            PushDestination::for_test("origin", "https://github.com/octo/widgets.git", Vec::new())
                .unwrap();
        let repository = destination.repository_coordinates();

        for byte in (0..=0x1f).chain([0x7f]) {
            let branch = format!("feature/{}tail", char::from(byte));
            let full_name = format!("refs/heads/{branch}");
            assert!(
                gix::refs::FullName::try_from(full_name.as_str()).is_err(),
                "ASCII control byte {byte:#04x} must be outside Git's branch domain"
            );
            assert!(
                BodyLinkContext::new(repository.clone(), Some(branch)).is_err(),
                "ASCII control byte {byte:#04x} must not reach a body line"
            );
        }
        assert!(BodyLinkContext::new(repository.clone(), Some(String::new())).is_err());

        for scalar in 0x80..=0x9f {
            let control = char::from_u32(scalar).unwrap();
            let branch = format!("feature/{control}tail");
            let full_name = format!("refs/heads/{branch}");
            gix::refs::FullName::try_from(full_name.as_str())
                .expect("a UTF-8 C1 control scalar is valid Git branch data");
            BodyLinkContext::new(repository.clone(), Some(branch.clone()))
                .expect("every valid UTF-8 Git branch must remain linkable");

            let mut rendered = String::new();
            write_public_branch_link(&mut rendered, &branch).unwrap();
            assert_eq!(
                rendered,
                format!(
                    "This PR is on branch [feature\\/{control}tail](../tree/feature/%C2%{scalar:02X}tail).\n\n"
                ),
                "UTF-8 C1 control scalar U+{scalar:04X}"
            );
        }
    }

    #[test]
    fn no_pending_stack_completes_with_an_empty_assignment() {
        let ids = ["Gone", "Gtwo"];
        let (stack, histories) = stack_fixture(&[(ids[0], "One", "One."), (ids[1], "Two", "Two.")]);
        let inputs = histories
            .into_iter()
            .zip([11, 22])
            .map(|(history, number)| existing_input(history, number))
            .collect();
        let recipes =
            StackBodyRecipes::new(link_context("/octo/widgets", None), stack, inputs).unwrap();
        assert!(recipes.provisional_bodies().is_empty());
        let (_, final_bodies) = recipes.into_parts();
        let bodies = final_bodies.complete([]).unwrap();
        assert_eq!(bodies.len(), 2);
        for body in &bodies {
            assert!(body.body().as_str().contains("#11"));
            assert!(body.body().as_str().contains("#22"));
        }
    }

    #[test]
    fn completion_validates_every_pending_assignment_before_rendering() {
        let error = mixed_recipes()
            .into_parts()
            .1
            .complete([(change_id("Gnew1"), number(22))])
            .unwrap_err();
        assert!(error.to_string().contains("missing change 'Gnew2'"));

        let error = mixed_recipes()
            .into_parts()
            .1
            .complete([(change_id("Gnew2"), number(33)), (change_id("Gnew1"), number(22))])
            .unwrap_err();
        assert!(error.to_string().contains("expected 'Gnew1'"));

        let error = mixed_recipes()
            .into_parts()
            .1
            .complete([(change_id("Gnew1"), number(22)), (change_id("Gwrong"), number(33))])
            .unwrap_err();
        assert!(error.to_string().contains("expected 'Gnew2'"));

        let error = mixed_recipes()
            .into_parts()
            .1
            .complete([(change_id("Gnew1"), number(22)), (change_id("Gnew2"), number(22))])
            .unwrap_err();
        assert!(error.to_string().contains("repeats pull request number 22"));

        let error = mixed_recipes()
            .into_parts()
            .1
            .complete([(change_id("Gnew1"), number(11)), (change_id("Gnew2"), number(33))])
            .unwrap_err();
        assert!(error.to_string().contains("repeats pull request number 11"));

        let error = mixed_recipes()
            .into_parts()
            .1
            .complete([
                (change_id("Gnew1"), number(22)),
                (change_id("Gnew2"), number(33)),
                (change_id("Gextra"), number(44)),
            ])
            .unwrap_err();
        assert!(error.to_string().contains("unexpected change 'Gextra'"));
    }

    #[test]
    fn mixed_provisional_body_omits_all_known_and_pending_navigation() {
        let recipes = mixed_recipes();
        assert_eq!(recipes.provisional_bodies().len(), 2);
        for body in recipes.provisional_bodies() {
            assert!(!body.body().as_str().contains("\n- "));
            assert!(!body.body().as_str().contains("#11"));
            assert!(!body.body().as_str().contains("#2147483647"));
        }
    }

    #[test]
    fn widest_full_layout_succeeds_at_the_exact_boundary() {
        let fixed = empty_single_render("Gfullexact", 10, HistoryLayout::Full).as_str().len();
        let padding = "x".repeat(MAX_BODY_SIZE_BYTES - fixed);
        let recipes = single_missing_recipes("Gfullexact", &padding, 10).unwrap();
        assert_eq!(recipes.final_bodies.entries[0].layout, HistoryLayout::Full);
        let representative = &recipes.final_bodies.entries[0].representative;
        assert_eq!(representative.as_str().len(), MAX_BODY_SIZE_BYTES);

        let bodies =
            complete_missing(recipes, &[("Gfullexact", MAX_PENDING_PULL_REQUEST_NUMBER)]).unwrap();
        assert_eq!(bodies[0].body().as_str().len(), MAX_BODY_SIZE_BYTES);
    }

    #[test]
    fn widest_sparse_fallback_is_frozen_for_short_actual_numbers() {
        let full = empty_single_render("Gsparse", 10, HistoryLayout::Full).as_str().len();
        let sparse = empty_single_render("Gsparse", 10, HistoryLayout::Sparse).as_str().len();
        assert!(sparse < full);
        let padding = "x".repeat(MAX_BODY_SIZE_BYTES - full + 1);
        assert!(sparse + padding.len() <= MAX_BODY_SIZE_BYTES);
        let recipes = single_missing_recipes("Gsparse", &padding, 10).unwrap();
        assert_eq!(recipes.final_bodies.entries[0].layout, HistoryLayout::Sparse);
        let full_only = "/compare/gherrit/Gsparse/v1..gherrit/Gsparse/v3";
        assert!(!recipes.final_bodies.entries[0].representative.as_str().contains(full_only));
        let bodies = complete_missing(recipes, &[("Gsparse", 7)]).unwrap();
        assert!(!bodies[0].body().as_str().contains(full_only));
    }

    #[test]
    fn construction_rejects_when_widest_sparse_layout_is_oversized() {
        let sparse = empty_single_render("Gsparseover", 10, HistoryLayout::Sparse).as_str().len();
        let padding = "x".repeat(MAX_BODY_SIZE_BYTES - sparse + 1);
        let error = single_missing_recipes("Gsparseover", &padding, 10).unwrap_err();
        assert!(error.to_string().contains("even with sparse history"));
    }

    #[test]
    fn layout_is_selected_and_frozen_per_entry() {
        let fixture = stack_history_fixture(&[
            ("Gfull", "Full", "Small full history.", 4),
            ("Gsparse", "Sparse", "Large sparse history.", 64),
        ]);
        let recipes =
            missing_recipes("/octo/widgets", None, fixture.stack, fixture.histories).unwrap();
        assert_eq!(recipes.final_bodies.entries[0].layout, HistoryLayout::Full);
        assert_eq!(recipes.final_bodies.entries[1].layout, HistoryLayout::Sparse);
        let full_only = |id: &str| format!("/compare/gherrit/{id}/v1..gherrit/{id}/v3");
        assert!(
            recipes.final_bodies.entries[0].representative.as_str().contains(&full_only("Gfull"))
        );
        assert!(
            !recipes.final_bodies.entries[1]
                .representative
                .as_str()
                .contains(&full_only("Gsparse"))
        );
        let bodies = complete_missing(recipes, &[("Gfull", 7), ("Gsparse", 8)]).unwrap();
        assert!(bodies[0].body().as_str().contains(&full_only("Gfull")));
        assert!(!bodies[1].body().as_str().contains(&full_only("Gsparse")));
    }

    #[test]
    fn repeated_pending_sentinel_uses_index_at_the_exact_boundary() {
        let ids = ["Groot", "Gtip"];
        let (stack, histories) = stack_fixture(&[(ids[0], "Root", ""), (ids[1], "Tip", "")]);
        let rendered = render_body(
            "/octo/widgets",
            None,
            &change_id(ids[0]),
            0,
            "",
            &histories[0],
            Navigation::Numbered(&[
                number(MAX_PENDING_PULL_REQUEST_NUMBER),
                number(MAX_PENDING_PULL_REQUEST_NUMBER),
            ]),
            HistoryLayout::Full,
        )
        .unwrap();
        let padding = "x".repeat(MAX_BODY_SIZE_BYTES - rendered.as_str().len());
        drop(stack);
        drop(histories);

        let (stack, histories) = stack_fixture(&[(ids[0], "Root", &padding), (ids[1], "Tip", "")]);
        let recipes = missing_recipes("/octo/widgets", None, stack, histories).unwrap();
        for (_, body) in recipes.final_bodies().representative_bodies() {
            assert_eq!(body.as_str().matches("👉").count(), 1);
        }
        assert_eq!(
            recipes.final_bodies.entries[0].representative.as_str().len(),
            MAX_BODY_SIZE_BYTES
        );

        let bodies =
            complete_missing(recipes, &[(ids[0], 2_147_483_646), (ids[1], 2_147_483_645)]).unwrap();
        assert_eq!(bodies[0].body().as_str().len(), MAX_BODY_SIZE_BYTES);
        for (index, body) in bodies.iter().enumerate() {
            assert_eq!(body.body().as_str().matches("👉").count(), 1);
            assert!(
                body.body()
                    .as_str()
                    .contains(&format!("👉 #{}", [2_147_483_646u64, 2_147_483_645][index]))
            );
        }
    }

    #[test]
    fn amend_and_rebase_history_snapshot_keeps_literal_base_oids() {
        let fixture = amend_fixture("Ghistory", "Explain the history.", 4);
        let expected_links = fixture
            .revisions
            .iter()
            .enumerate()
            .map(|(index, (_, parent))| {
                format!("/compare/{parent}...gherrit/Ghistory/v{}", index + 1)
            })
            .collect::<Vec<_>>();
        let labels = fixture
            .revisions
            .iter()
            .enumerate()
            .map(|(index, (_, parent))| (*parent, format!("<literal-base-v{}>", index + 1)))
            .collect::<Vec<_>>();
        let recipes =
            missing_recipes("/octo/widgets", None, fixture.stack, vec![fixture.history]).unwrap();
        let representative = &recipes.final_bodies.entries[0].representative;
        for link in expected_links {
            assert!(representative.as_str().contains(&link));
        }
        let normalized = normalize_object_ids(
            representative.as_str().to_owned(),
            &labels.iter().map(|(oid, label)| (*oid, label.as_str())).collect::<Vec<_>>(),
        );
        insta::assert_snapshot!("bounded_amend_rebase_history", normalized);
    }

    #[test]
    fn repeated_revision_snapshot_retains_every_position() {
        let fixture = repeated_history_fixture("Repeat without collapsing versions.");
        assert_eq!(
            fixture
                .history
                .projected_versions()
                .map(|(_, revision)| revision.head())
                .collect::<Vec<_>>(),
            fixture.revisions.iter().map(|(head, _)| *head).collect::<Vec<_>>()
        );
        let shared_base = fixture.revisions[0].1;
        assert!(fixture.revisions.iter().all(|(_, parent)| *parent == shared_base));
        let recipes =
            missing_recipes("/octo/widgets", None, fixture.stack, vec![fixture.history]).unwrap();
        let representative = &recipes.final_bodies.entries[0].representative;
        assert!(representative.as_str().contains("**Latest Update:** v4"));
        let normalized = normalize_object_ids(
            representative.as_str().to_owned(),
            &[(shared_base, "<shared-literal-base>")],
        );
        insta::assert_snapshot!("bounded_repeated_a_a_b_a_history", normalized);
    }
}
