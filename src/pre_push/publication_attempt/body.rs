//! Bounded pull-request content frozen from one ordered local stack.
//!
//! The publication planner owns pull-request existence and identities. This
//! module owns only the shared presentation context and ordered per-change
//! facts needed to render one internally consistent set of provisional or
//! final bodies.
use std::{collections::HashSet, fmt};

use color_eyre::eyre::{Result, bail};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use super::{
    PublicBranch,
    github::PullRequestNumber,
    history::{Revision, ValidatedChangeHistory},
    version::Version,
};
use crate::pre_push::{
    destination::PushDestination,
    local::{GherritPrId, LocalChange, LocalStack, PullRequestTitle},
};

// Per https://github.com/orgs/community/discussions/27190#discussioncomment-3254953,
// GitHub stores PR bodies in a `mediumblob` with a 262,144-byte limit. Use half
// of that limit as a safety factor.
const MAX_BODY_SIZE_BYTES: usize = 131_072;

/// Repository links derived from the selected push destination and an optional
/// checked public branch.
#[derive(Debug, Eq, PartialEq)]
struct BodyLinkContext {
    repository_url: String,
    public_branch: Option<PublicBranch>,
}

impl BodyLinkContext {
    fn from_destination(
        destination: &PushDestination,
        public_branch: Option<PublicBranch>,
    ) -> Self {
        Self { repository_url: destination.repo_url_relative(), public_branch }
    }

    #[cfg(test)]
    fn for_test(repository_url: &str, public_branch: Option<PublicBranch>) -> Self {
        Self { repository_url: repository_url.to_owned(), public_branch }
    }
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
fn write_public_branch_link(
    output: &mut impl fmt::Write,
    repository_url: &str,
    branch: &PublicBranch,
) -> fmt::Result {
    let branch = branch.as_str();
    output.write_str("This PR is on branch [")?;
    write_markdown_text(output, branch)?;
    writeln!(
        output,
        "]({repository_url}/tree/{}).\n",
        utf8_percent_encode(branch, GITHUB_TREE_BRANCH_PATH),
    )
}

/// A generated body proven to fit GHerrit's product body limit.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct GeneratedBody(Box<str>);

impl GeneratedBody {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn into_string(self) -> String {
        self.0.into()
    }
}

/// One concrete generated body coupled to its change identity.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct RenderedBody {
    id: GherritPrId,
    body: GeneratedBody,
}

impl RenderedBody {
    #[cfg(test)]
    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    #[cfg(test)]
    pub(super) fn body(&self) -> &GeneratedBody {
        &self.body
    }

    pub(super) fn into_parts(self) -> (GherritPrId, GeneratedBody) {
        (self.id, self.body)
    }
}

/// One ordered, bounded set of pull-request body recipes.
///
/// The set consumes a validated local stack and its correspondingly ordered
/// literal histories. It retains the presentation context only once and
/// derives every stack index, stack size, and navigation sequence from its one
/// entry collection. Pull-request existence remains solely planner state.
#[derive(Debug)]
pub(super) struct StackBodyRecipes {
    context: BodyLinkContext,
    entries: Box<[PullRequestRecipe]>,
}

impl StackBodyRecipes {
    pub(super) fn new(
        destination: &PushDestination,
        public_branch: Option<PublicBranch>,
        stack: LocalStack,
        histories: Vec<ValidatedChangeHistory>,
    ) -> Result<Self> {
        let context = BodyLinkContext::from_destination(destination, public_branch);
        Self::from_parts(context, stack.into_changes(), histories)
    }

    fn from_parts(
        context: BodyLinkContext,
        changes: Vec<LocalChange>,
        histories: Vec<ValidatedChangeHistory>,
    ) -> Result<Self> {
        if changes.is_empty() {
            bail!("Pull request body recipes require a nonempty stack");
        }
        if changes.len() != histories.len() {
            bail!("Local changes and body histories have different counts");
        }

        let entries = changes
            .into_iter()
            .zip(histories)
            .map(|(change, history)| PullRequestRecipe::new(change, history))
            .collect::<Result<Box<[_]>>>()?;
        let recipes = Self { context, entries };
        let number_count = recipes.entries.len();
        recipes.entries.iter().enumerate().try_for_each(|(current_index, recipe)| {
            recipe.prove_bounded(&recipes.context, current_index, number_count)
        })?;
        Ok(recipes)
    }

    pub(super) fn titles(
        &self,
    ) -> impl DoubleEndedIterator<Item = (&GherritPrId, &PullRequestTitle)> + ExactSizeIterator
    {
        self.entries.iter().map(|recipe| (recipe.id(), recipe.title()))
    }

    /// Renders a numberless body for every change.
    ///
    /// The planner selects bodies for absent pull requests. Rendering the
    /// complete set here keeps position and shared context derived from one
    /// ordered collection rather than accepting a parallel missing-state map.
    pub(super) fn provisional_bodies(&self) -> Box<[RenderedBody]> {
        self.entries
            .iter()
            .enumerate()
            .map(|(current_index, recipe)| recipe.provisional_body(&self.context, current_index))
            .collect()
    }

    /// Renders every final body from one complete keyed navigation sequence.
    pub(super) fn final_bodies(
        &self,
        assignments: &[(GherritPrId, PullRequestNumber)],
    ) -> Result<Box<[RenderedBody]>> {
        if assignments.len() != self.entries.len() {
            bail!("Final pull request numbers and body recipes have different counts");
        }

        let mut numbers_seen = HashSet::with_capacity(assignments.len());
        self.entries.iter().zip(assignments).enumerate().try_for_each(
            |(index, (recipe, (id, number)))| {
                if recipe.id() != id {
                    bail!(
                        "Final pull request number at stack position {index} identifies '{}', expected '{}'",
                        id.as_str(),
                        recipe.id().as_str()
                    );
                }
                if !numbers_seen.insert(*number) {
                    bail!("Final navigation repeats pull request number {}", number.get());
                }
                Ok(())
            },
        )?;

        let numbers = assignments.iter().map(|(_, number)| *number).collect::<Box<[_]>>();
        Ok(self
            .entries
            .iter()
            .enumerate()
            .map(|(current_index, recipe)| {
                recipe.final_body(&self.context, current_index, &numbers)
            })
            .collect())
    }
}

#[derive(Debug)]
struct PullRequestRecipe {
    title: PullRequestTitle,
    commit_body: Box<str>,
    history: ValidatedChangeHistory,
}

impl PullRequestRecipe {
    fn new(change: LocalChange, history: ValidatedChangeHistory) -> Result<Self> {
        if history.id() != change.id() {
            bail!(
                "Body history for '{}' cannot be joined to local change '{}'",
                history.id().as_str(),
                change.id().as_str()
            );
        }
        let proposal = history.proposed();
        if proposal.head() != change.head() || proposal.first_parent() != change.first_parent() {
            bail!(
                "Body history for '{}' does not retain the local proposal and first parent",
                change.id().as_str()
            );
        }

        let (title, commit_body) = change.into_pull_request_content();
        Ok(Self { title, commit_body: commit_body.into_boxed_str(), history })
    }

    fn prove_bounded(
        &self,
        context: &BodyLinkContext,
        current_index: usize,
        number_count: usize,
    ) -> Result<()> {
        // This body is an existence witness, not a representative mutation
        // payload. A Full body rendered with exact short numbers can be wider
        // than this Sparse body. The planner renders exact final bodies after
        // create receipts arrive and preflights their mutations before the
        // following marker effect.
        render_body(
            &context.repository_url,
            context.public_branch.as_ref(),
            self.id(),
            current_index,
            &self.commit_body,
            &self.history,
            Navigation::Widest(number_count),
            HistoryLayout::Sparse,
        )
        .map_err(|BodyTooLarge| {
            color_eyre::eyre::eyre!(
                "Generated pull request body for '{}' exceeds the {MAX_BODY_SIZE_BYTES}-byte limit even with sparse history",
                self.id().as_str()
            )
        })?;
        Ok(())
    }

    fn id(&self) -> &GherritPrId {
        self.history.id()
    }

    fn title(&self) -> &PullRequestTitle {
        &self.title
    }

    /// Renders the numberless body used only while creating an absent pull
    /// request. The sparse fallback is no wider than the constructor's witness.
    fn provisional_body(&self, context: &BodyLinkContext, current_index: usize) -> RenderedBody {
        let body = render_preferred_body(
            &context.repository_url,
            context.public_branch.as_ref(),
            self.id(),
            current_index,
            &self.commit_body,
            &self.history,
            Navigation::Omitted,
        )
        .expect("a sparse provisional body is no wider than its proven witness");
        RenderedBody { id: self.id().clone(), body }
    }

    fn final_body(
        &self,
        context: &BodyLinkContext,
        current_index: usize,
        numbers: &[PullRequestNumber],
    ) -> RenderedBody {
        let body = render_preferred_body(
            &context.repository_url,
            context.public_branch.as_ref(),
            self.id(),
            current_index,
            &self.commit_body,
            &self.history,
            Navigation::Numbered(numbers),
        )
        .expect("typed pull request numbers fit the proven widest sparse body");
        RenderedBody { id: self.id().clone(), body }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryLayout {
    Full,
    Sparse,
}

#[derive(Clone, Copy)]
enum Navigation<'numbers> {
    Omitted,
    Numbered(&'numbers [PullRequestNumber]),
    Widest(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BodyTooLarge;

#[allow(clippy::too_many_arguments)]
fn render_preferred_body(
    repository_url: &str,
    public_branch: Option<&PublicBranch>,
    id: &GherritPrId,
    current_index: usize,
    commit_body: &str,
    history: &ValidatedChangeHistory,
    navigation: Navigation<'_>,
) -> std::result::Result<GeneratedBody, BodyTooLarge> {
    render_body(
        repository_url,
        public_branch,
        id,
        current_index,
        commit_body,
        history,
        navigation,
        HistoryLayout::Full,
    )
    .or_else(|BodyTooLarge| {
        render_body(
            repository_url,
            public_branch,
            id,
            current_index,
            commit_body,
            history,
            navigation,
            HistoryLayout::Sparse,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn render_body(
    repository_url: &str,
    public_branch: Option<&PublicBranch>,
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
    public_branch: Option<&PublicBranch>,
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
    write_navigation(output, repository_url, public_branch, current_index, navigation)?;
    write_history(output, repository_url, id, history, history_layout)?;
    write_download(output, id)?;
    output.write_str("\n\n")?;
    output.write_str("*Stacked PRs enabled by [GHerrit](https://github.com/joshlf/gherrit).*")
}

fn write_navigation(
    output: &mut impl fmt::Write,
    repository_url: &str,
    public_branch: Option<&PublicBranch>,
    current_index: usize,
    navigation: Navigation<'_>,
) -> fmt::Result {
    if let Some(branch) = public_branch {
        write_public_branch_link(output, repository_url, branch)?;
    }

    match navigation {
        Navigation::Omitted => Ok(()),
        Navigation::Numbered(numbers) => {
            numbers.iter().copied().enumerate().rev().try_for_each(|(index, number)| {
                write_navigation_row(output, current_index, index, number)
            })
        }
        Navigation::Widest(count) => (0..count).rev().try_for_each(|index| {
            write_navigation_row(output, current_index, index, PullRequestNumber::MAX)
        }),
    }
}

fn write_navigation_row(
    output: &mut impl fmt::Write,
    current_index: usize,
    index: usize,
    number: PullRequestNumber,
) -> fmt::Result {
    let prefix = if index == current_index { "👉" } else { "\u{3000}\u{2009}" };
    writeln!(output, "- {prefix} #{}", number.get())
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

    use super::*;
    use crate::{
        manage::PublicBranchName,
        pre_push::destination::{DefaultBranch, PushDestination},
    };

    fn object_id(value: u16) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[18..].copy_from_slice(&value.to_be_bytes());
        if value == 0 {
            bytes[17] = 1;
        }
        ObjectId::from_bytes_or_panic(&bytes)
    }

    fn change_id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).expect("valid test change ID")
    }

    fn number(value: u32) -> PullRequestNumber {
        PullRequestNumber::for_test(value)
    }

    fn public_branch(value: &str) -> Result<PublicBranch> {
        PublicBranch::new(
            PublicBranchName::new(value.to_owned())?,
            &DefaultBranch::new("main".to_owned(), object_id(1))?,
        )
    }

    struct StackFixture {
        changes: Vec<LocalChange>,
        histories: Vec<ValidatedChangeHistory>,
    }

    fn stack_history_fixture(entries: &[(&str, &str, &str, usize)]) -> StackFixture {
        assert!(entries.iter().all(|(_, _, _, versions)| *versions > 0));
        let default_tip = object_id(60_000);
        let heads = entries
            .iter()
            .enumerate()
            .map(|(index, _)| object_id(50_000 + u16::try_from(index).unwrap()))
            .collect::<Vec<_>>();
        let mut first_parent = default_tip;
        let changes = entries
            .iter()
            .zip(&heads)
            .map(|((id, title, body, _), head)| {
                let change = LocalChange::for_body_test(
                    change_id(id),
                    *head,
                    first_parent,
                    (*title).to_owned(),
                    (*body).to_owned(),
                )
                .expect("valid body-test change");
                first_parent = *head;
                change
            })
            .collect::<Vec<_>>();
        let histories = changes
            .iter()
            .zip(entries)
            .enumerate()
            .map(|(entry_index, (change, (_, _, _, versions)))| {
                let entry_index = u16::try_from(entry_index).unwrap();
                let published = (0..versions.saturating_sub(1))
                    .map(|version| {
                        let version = u16::try_from(version).unwrap();
                        (
                            object_id(1_000 + entry_index * 1_000 + version),
                            object_id(30_000 + entry_index * 1_000 + version),
                        )
                    })
                    .collect::<Vec<_>>();
                ValidatedChangeHistory::for_body_test(
                    change.id().clone(),
                    &published,
                    (change.head(), change.first_parent()),
                )
            })
            .collect();
        StackFixture { changes, histories }
    }

    fn stack_fixture(entries: &[(&str, &str, &str)]) -> StackFixture {
        let entries =
            entries.iter().map(|(id, title, body)| (*id, *title, *body, 1)).collect::<Vec<_>>();
        stack_history_fixture(&entries)
    }

    fn recipes(
        repository_url: &str,
        public_branch_name: Option<&str>,
        fixture: StackFixture,
    ) -> Result<StackBodyRecipes> {
        let public_branch = public_branch_name.map(public_branch).transpose()?;
        let context = BodyLinkContext::for_test(repository_url, public_branch);
        StackBodyRecipes::from_parts(context, fixture.changes, fixture.histories)
    }

    fn link_context(repository_url: &str, public_branch_name: Option<&str>) -> BodyLinkContext {
        let public_branch = public_branch_name.map(public_branch).transpose().unwrap();
        BodyLinkContext::for_test(repository_url, public_branch)
    }

    fn rendered_report<'body>(
        bodies: impl IntoIterator<Item = (&'body GherritPrId, &'body GeneratedBody)>,
    ) -> String {
        bodies.into_iter().fold(String::new(), |mut report, (id, body)| {
            writeln!(report, "===== {} =====", id.as_str()).unwrap();
            writeln!(report, "{}", body.as_str()).unwrap();
            report
        })
    }

    fn provisional_report(recipes: &StackBodyRecipes) -> String {
        let bodies = recipes.provisional_bodies();
        rendered_final_report(&bodies)
    }

    fn rendered_final_report(bodies: &[RenderedBody]) -> String {
        rendered_report(bodies.iter().map(|body| (body.id(), body.body())))
    }

    fn single_recipes(id: &str, body: &str, versions: usize) -> Result<StackBodyRecipes> {
        single_recipes_with_branch(id, body, versions, None)
    }

    fn single_recipes_with_branch(
        id: &str,
        body: &str,
        versions: usize,
        public_branch: Option<&str>,
    ) -> Result<StackBodyRecipes> {
        recipes(
            "/octo/widgets",
            public_branch,
            stack_history_fixture(&[(id, "Validated title", body, versions)]),
        )
    }

    fn final_single(recipes: &StackBodyRecipes, number: PullRequestNumber) -> RenderedBody {
        let id = recipes.entries[0].id().clone();
        recipes
            .final_bodies(&[(id, number)])
            .unwrap()
            .into_vec()
            .pop()
            .expect("one body-test recipe")
    }

    fn render_single(
        id: &str,
        body: &str,
        versions: usize,
        number: PullRequestNumber,
        layout: HistoryLayout,
    ) -> GeneratedBody {
        let fixture = stack_history_fixture(&[(id, "Validated title", body, versions)]);
        let history = &fixture.histories[0];
        render_body(
            "/octo/widgets",
            None,
            history.id(),
            0,
            body,
            history,
            Navigation::Numbered(&[number]),
            layout,
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
    fn provisional_private_stack_preserves_ordinary_metadata_text() {
        let fixture = stack_fixture(&[
            (
                "Groot",
                "Root title",
                "Introduce the root.\n\n<!-- gherrit-meta: ordinary commit text -->\n\nExplain it.",
            ),
            ("Gmiddle", "Middle title", "Build on the root."),
            ("Gtip", "Tip title", "Finish the stack."),
        ]);
        let recipes = recipes("/octo/widgets", None, fixture).unwrap();
        let report = provisional_report(&recipes);

        assert!(report.contains("<!-- gherrit-meta: ordinary commit text -->"));
        assert!(!report.contains("GHerrit relies on the following metadata"));
        assert!(!report.contains("\n- "));
        insta::assert_snapshot!("bounded_provisional_private_stack", report);
    }

    #[test]
    fn provisional_public_stack_has_safe_links_and_no_numbers() {
        let fixture = stack_fixture(&[
            ("Groot", "Root title", "Introduce the root."),
            ("Gmiddle", "Middle title", "Build on the root."),
            ("Gtip", "Tip title", "Finish the stack."),
        ]);
        let recipes = recipes("/octo/widgets", Some("feature-/public-stack"), fixture).unwrap();
        let report = provisional_report(&recipes);

        assert!(report.contains("feature\\-\\/public\\-stack"));
        assert!(!report.contains("\n- "));
        insta::assert_snapshot!("bounded_provisional_public_stack", report);
    }

    #[test]
    fn final_stack_uses_the_planners_complete_ordered_numbers() {
        let fixture = stack_fixture(&[
            ("Groot", "Root title", "Introduce the root."),
            ("Gmiddle", "Middle title", "Build on the root."),
            ("Gtip", "Tip title", "Finish the stack."),
        ]);
        let recipes = recipes("/octo/widgets", None, fixture).unwrap();
        assert_eq!(
            recipes.entries.iter().map(|recipe| recipe.id().as_str()).collect::<Vec<_>>(),
            ["Groot", "Gmiddle", "Gtip"]
        );
        assert_eq!(
            recipes.titles().map(|(_, title)| title.as_str()).collect::<Vec<_>>(),
            ["Root title", "Middle title", "Tip title"]
        );

        let assignments = [
            (change_id("Groot"), number(11)),
            (change_id("Gmiddle"), number(22)),
            (change_id("Gtip"), number(33)),
        ];
        let bodies = recipes.final_bodies(&assignments).unwrap();
        for (index, body) in bodies.iter().enumerate() {
            for number in [11, 22, 33] {
                assert!(body.body().as_str().contains(&format!("#{number}")));
            }
            assert_eq!(body.body().as_str().matches("👉").count(), 1);
            assert!(body.body().as_str().contains(&format!("👉 #{}", [11, 22, 33][index])));
        }
        insta::assert_snapshot!("bounded_complete_final_stack", rendered_final_report(&bodies));
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
        assert_eq!(accepted.finish().as_str().len(), MAX_BODY_SIZE_BYTES);

        for remaining in [1, 2] {
            let prefix = "x".repeat(MAX_BODY_SIZE_BYTES - remaining);
            let mut writer = BoundedWriter::new();
            writer.write_str(&prefix).unwrap();
            assert!(writer.write_str("雪").is_err());
            assert_eq!(writer.output, prefix);
        }
    }

    #[test]
    fn generated_body_accepts_exactly_the_limit_and_rejects_the_next_byte() {
        let empty = render_single("Gexact", "", 1, PullRequestNumber::MAX, HistoryLayout::Full);
        let exact_padding = "x".repeat(MAX_BODY_SIZE_BYTES - empty.as_str().len());
        let exact =
            render_single("Gexact", &exact_padding, 1, PullRequestNumber::MAX, HistoryLayout::Full);
        assert_eq!(exact.as_str().len(), MAX_BODY_SIZE_BYTES);

        let over_padding = format!("{exact_padding}x");
        let fixture = stack_history_fixture(&[("Gexact", "Exact", &over_padding, 1)]);
        let history = &fixture.histories[0];
        assert!(
            render_body(
                "/octo/widgets",
                None,
                history.id(),
                0,
                &over_padding,
                history,
                Navigation::Numbered(&[PullRequestNumber::MAX]),
                HistoryLayout::Full,
            )
            .is_err()
        );
    }

    #[test]
    fn oversized_user_body_and_large_history_fail_inside_the_bound() {
        let huge_body = "x".repeat(MAX_BODY_SIZE_BYTES * 4);
        let error = recipes(
            "/octo/widgets",
            None,
            stack_fixture(&[("Ghugebody", "Huge body", &huge_body)]),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exceeds the 131072-byte limit"));

        let fixture = stack_history_fixture(&[("Ghugehistory", "Huge history", "", 512)]);
        assert_eq!(fixture.histories[0].projected_versions().len(), 512);
        let error = recipes("/octo/widgets", None, fixture).unwrap_err();
        assert!(error.to_string().contains("even with sparse history"));
    }

    #[test]
    fn reverse_history_is_zero_copy_and_bounded_writing_stops_before_rows() {
        fn require_reverse_exact(
            iterator: impl DoubleEndedIterator<Item = (Version, Revision)> + ExactSizeIterator,
        ) -> usize {
            iterator.len()
        }

        let fixture = stack_history_fixture(&[("Gstream", "Stream", "", 3)]);
        let history = &fixture.histories[0];
        assert_eq!(require_reverse_exact(history.projected_versions()), 3);
        let revision = history.proposed();
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
        assert_eq!(visited.get(), 0);
        assert!(output.output.len() <= MAX_BODY_SIZE_BYTES);
    }

    #[test]
    fn construction_rejects_empty_misaligned_or_mismatched_input() {
        let error = StackBodyRecipes::from_parts(
            link_context("/octo/widgets", None),
            Vec::new(),
            Vec::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("nonempty stack"));

        let mut fixture = stack_fixture(&[("Gone", "One", "One.")]);
        fixture.histories.clear();
        let error = StackBodyRecipes::from_parts(
            link_context("/octo/widgets", None),
            fixture.changes,
            fixture.histories,
        )
        .unwrap_err();
        assert!(error.to_string().contains("different counts"));

        let mut fixture = stack_fixture(&[("Gone", "One", "One.")]);
        let change = fixture.changes.pop().unwrap();
        let proposal = fixture.histories.pop().unwrap().proposed();
        let wrong_id = ValidatedChangeHistory::for_body_test(
            change_id("Gother"),
            &[],
            (proposal.head(), proposal.first_parent()),
        );
        let error = StackBodyRecipes::from_parts(
            link_context("/octo/widgets", None),
            vec![change],
            vec![wrong_id],
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot be joined to local change 'Gone'"));

        let mut fixture = stack_fixture(&[("Gsame", "Same", "Body")]);
        let change = fixture.changes.pop().unwrap();
        let wrong_proposal = ValidatedChangeHistory::for_body_test(
            change.id().clone(),
            &[],
            (object_id(42), change.first_parent()),
        );
        let error = StackBodyRecipes::from_parts(
            link_context("/octo/widgets", None),
            vec![change],
            vec![wrong_proposal],
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not retain the local proposal and first parent"));

        let mut fixture = stack_fixture(&[("Gsame", "Same", "Body")]);
        let change = fixture.changes.pop().unwrap();
        let wrong_proposal = ValidatedChangeHistory::for_body_test(
            change.id().clone(),
            &[],
            (change.head(), object_id(43)),
        );
        let error = StackBodyRecipes::from_parts(
            link_context("/octo/widgets", None),
            vec![change],
            vec![wrong_proposal],
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not retain the local proposal and first parent"));
    }

    #[test]
    fn body_links_derive_from_the_destination_and_escape_branch_data() {
        let destination = PushDestination::for_test();
        let context = BodyLinkContext::from_destination(
            &destination,
            Some(public_branch("feature-/public-stack").unwrap()),
        );
        assert_eq!(context.repository_url, "/owner/repo");
        assert_eq!(
            context.public_branch.as_ref().map(PublicBranch::as_str),
            Some("feature-/public-stack")
        );

        let mut rendered = String::new();
        for branch in [
            "feature-/public-stack",
            "release_(candidate)",
            "fix]docs",
            "hash#fragment",
            "percent%2Fslash",
            "amp&copy;",
            "tick`name",
            "angle<tag>",
            "paren)tail",
            "café/東京",
            "feature-/🚀",
        ] {
            writeln!(rendered, "===== {branch:?} =====").unwrap();
            let branch = public_branch(branch).unwrap();
            write_public_branch_link(&mut rendered, "/owner/repo", &branch).unwrap();
        }
        rendered.push_str("===== END =====\n");
        insta::assert_snapshot!("public_branch_links_are_data", rendered);
    }

    #[test]
    fn public_branch_encoders_cover_exact_ascii_classes() {
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
    fn public_branch_validation_enforces_git_ref_grammar() {
        for byte in (0..=0x1f).chain([0x7f]) {
            let branch = format!("feature-/{}tail", char::from(byte));
            assert!(public_branch(&branch).is_err());
        }

        for scalar in 0x80..=0x9f {
            let control = char::from_u32(scalar).unwrap();
            let branch = format!("feature-/{control}tail");
            public_branch(&branch).expect("valid UTF-8 Git branch data remains linkable");
            let mut rendered = String::new();
            let branch = public_branch(&branch).unwrap();
            write_public_branch_link(&mut rendered, "/octo/widgets", &branch).unwrap();
            assert_eq!(
                rendered,
                format!(
                    "This PR is on branch [feature\\-\\/{control}tail](/octo/widgets/tree/feature-/%C2%{scalar:02X}tail).\n\n"
                )
            );
        }
        assert!(public_branch("").is_err());
        for branch in [
            ".",
            "..",
            ".hidden",
            "../issues",
            "/leading",
            "trailing/",
            "double//slash",
            "feature/.",
            "feature/..",
            "feature..one",
            "feature.",
            "feature.lock",
            "feature@{value}",
            "feature with space",
            "feature~one",
            "feature^one",
            "feature:one",
            "feature?one",
            "feature*one",
            "feature[one",
            "feature\\one",
        ] {
            assert!(public_branch(branch).is_err(), "branch={branch:?}");
        }
        public_branch("feature-/<!--gherrit-meta-ordinary-->")
            .expect("metadata-looking text is ordinary valid branch data");
    }

    #[test]
    fn final_bodies_require_one_exact_unique_keyed_sequence() {
        let recipes = recipes(
            "/octo/widgets",
            None,
            stack_fixture(&[("Groot", "Root", ""), ("Gtip", "Tip", "")]),
        )
        .unwrap();

        let error = recipes.final_bodies(&[(change_id("Groot"), number(11))]).unwrap_err();
        assert!(error.to_string().contains("different counts"));

        let error = recipes
            .final_bodies(&[(change_id("Gtip"), number(22)), (change_id("Groot"), number(11))])
            .unwrap_err();
        assert!(error.to_string().contains("stack position 0 identifies 'Gtip', expected 'Groot'"));

        let error = recipes
            .final_bodies(&[(change_id("Groot"), number(11)), (change_id("Gother"), number(22))])
            .unwrap_err();
        assert!(
            error.to_string().contains("stack position 1 identifies 'Gother', expected 'Gtip'")
        );

        let error = recipes
            .final_bodies(&[(change_id("Groot"), number(11)), (change_id("Gtip"), number(11))])
            .unwrap_err();
        assert!(error.to_string().contains("repeats pull request number 11"));

        let error = recipes
            .final_bodies(&[
                (change_id("Groot"), number(11)),
                (change_id("Gtip"), number(22)),
                (change_id("Gextra"), number(33)),
            ])
            .unwrap_err();
        assert!(error.to_string().contains("different counts"));
    }

    #[test]
    fn every_provisional_body_omits_navigation_numbers() {
        let recipes = recipes(
            "/octo/widgets",
            None,
            stack_fixture(&[("Groot", "Root", ""), ("Gmiddle", "Middle", ""), ("Gtip", "Tip", "")]),
        )
        .unwrap();
        let provisional = recipes.provisional_bodies();
        assert_eq!(provisional.len(), 3);
        for body in provisional {
            assert!(!body.body().as_str().contains("\n- "));
            assert!(!body.body().as_str().contains("#11"));
            assert!(!body.body().as_str().contains(&format!("#{}", PullRequestNumber::MAX.get())));
        }
    }

    #[test]
    fn exact_actual_numbers_choose_full_layout_on_the_first_attempt() {
        let full_short =
            render_single("Gactual", "", 10, number(7), HistoryLayout::Full).as_str().len();
        let full_max =
            render_single("Gactual", "", 10, PullRequestNumber::MAX, HistoryLayout::Full)
                .as_str()
                .len();
        let sparse_max =
            render_single("Gactual", "", 10, PullRequestNumber::MAX, HistoryLayout::Sparse)
                .as_str()
                .len();
        assert!(full_short < full_max);
        assert!(sparse_max < full_short);

        let padding = "x".repeat(MAX_BODY_SIZE_BYTES - full_short);
        let recipes = single_recipes("Gactual", &padding, 10).unwrap();
        let body = final_single(&recipes, number(7));
        assert_eq!(body.body().as_str().len(), MAX_BODY_SIZE_BYTES);
        assert!(body.body().as_str().contains("/compare/gherrit/Gactual/v1..gherrit/Gactual/v3"));
    }

    #[test]
    fn exact_actual_numbers_fall_back_to_sparse_only_when_needed() {
        let full = render_single("Gsparse", "", 10, number(7), HistoryLayout::Full).as_str().len();
        let sparse =
            render_single("Gsparse", "", 10, number(7), HistoryLayout::Sparse).as_str().len();
        assert!(sparse < full);
        let padding = "x".repeat(MAX_BODY_SIZE_BYTES - full + 1);
        assert!(sparse + padding.len() <= MAX_BODY_SIZE_BYTES);

        let recipes = single_recipes("Gsparse", &padding, 10).unwrap();
        let body = final_single(&recipes, number(7));
        assert!(!body.body().as_str().contains("/compare/gherrit/Gsparse/v1..gherrit/Gsparse/v3"));
    }

    #[test]
    fn provisional_body_falls_back_to_sparse_only_when_needed() {
        let fixture = stack_history_fixture(&[("Gprovisional", "Provisional", "", 10)]);
        let history = &fixture.histories[0];
        let render = |layout| {
            render_body(
                "/octo/widgets",
                None,
                history.id(),
                0,
                "",
                history,
                Navigation::Omitted,
                layout,
            )
            .unwrap()
            .as_str()
            .len()
        };
        let full = render(HistoryLayout::Full);
        let sparse = render(HistoryLayout::Sparse);
        let witness =
            render_single("Gprovisional", "", 10, PullRequestNumber::MAX, HistoryLayout::Sparse)
                .as_str()
                .len();
        assert!(sparse <= witness && witness < full);

        let padding = "x".repeat(MAX_BODY_SIZE_BYTES - full + 1);
        let body = single_recipes("Gprovisional", &padding, 10)
            .unwrap()
            .provisional_bodies()
            .into_vec()
            .pop()
            .unwrap();
        assert!(body.body().as_str().len() <= MAX_BODY_SIZE_BYTES);
        assert!(
            !body
                .body()
                .as_str()
                .contains("/compare/gherrit/Gprovisional/v1..gherrit/Gprovisional/v3")
        );
    }

    #[test]
    fn construction_accepts_the_exact_sparse_max_witness_and_rejects_one_more_byte() {
        let sparse =
            render_single("Gsparseover", "", 10, PullRequestNumber::MAX, HistoryLayout::Sparse)
                .as_str()
                .len();
        let exact_padding = "x".repeat(MAX_BODY_SIZE_BYTES - sparse);
        let exact = single_recipes("Gsparseover", &exact_padding, 10).unwrap();
        assert_eq!(
            final_single(&exact, PullRequestNumber::MAX).body().as_str().len(),
            MAX_BODY_SIZE_BYTES
        );

        let error = single_recipes("Gsparseover", &format!("{exact_padding}x"), 10).unwrap_err();
        assert!(error.to_string().contains("even with sparse history"));
    }

    #[test]
    fn widest_navigation_streams_and_stops_when_it_alone_is_too_large() {
        let fixture = stack_fixture(&[("Gnavigation", "Navigation", "")]);
        let history = &fixture.histories[0];
        let error = render_body(
            "/octo/widgets",
            None,
            history.id(),
            0,
            "",
            history,
            Navigation::Widest(usize::MAX),
            HistoryLayout::Sparse,
        )
        .unwrap_err();

        assert_eq!(error, BodyTooLarge);
    }

    #[test]
    fn sparse_max_witness_bounds_every_valid_number_width() {
        let fixture =
            stack_fixture(&[("Groot", "Root", ""), ("Gmiddle", "Middle", ""), ("Gtip", "Tip", "")]);
        let history = &fixture.histories[1];
        let widest = render_body(
            "/octo/widgets",
            None,
            history.id(),
            1,
            "",
            history,
            Navigation::Widest(3),
            HistoryLayout::Sparse,
        )
        .unwrap();
        for value in [
            1,
            9,
            10,
            99,
            100,
            999,
            1_000,
            9_999,
            10_000,
            99_999,
            100_000,
            999_999,
            1_000_000,
            9_999_999,
            10_000_000,
            99_999_999,
            100_000_000,
            999_999_999,
            1_000_000_000,
            i32::MAX as u32,
        ] {
            let number = number(value);
            let rendered = render_body(
                "/octo/widgets",
                None,
                history.id(),
                1,
                "",
                history,
                Navigation::Numbered(&[number; 3]),
                HistoryLayout::Sparse,
            )
            .unwrap();
            assert!(rendered.as_str().len() <= widest.as_str().len(), "number={value}");
        }
    }

    #[test]
    fn body_limit_counts_utf8_bytes_without_splitting_scalars() {
        let fixed = render_single("Gutf8", "", 1, PullRequestNumber::MAX, HistoryLayout::Sparse)
            .as_str()
            .len();
        let available = MAX_BODY_SIZE_BYTES - fixed;
        let mut exact_padding = "雪".repeat(available / "雪".len());
        exact_padding.push_str(&"x".repeat(available - exact_padding.len()));

        let exact = single_recipes("Gutf8", &exact_padding, 1).unwrap();
        assert_eq!(
            final_single(&exact, PullRequestNumber::MAX).body().as_str().len(),
            MAX_BODY_SIZE_BYTES
        );
        let error = single_recipes("Gutf8", &format!("{exact_padding}雪"), 1).unwrap_err();
        assert!(error.to_string().contains("even with sparse history"));
    }

    #[test]
    fn body_limit_includes_escaped_public_branch_expansion() {
        let branch = "feature-/(escaped)!café";
        let empty = single_recipes_with_branch("Gbranch", "", 1, Some(branch)).unwrap();
        let fixed = final_single(&empty, PullRequestNumber::MAX).body().as_str().len();
        let exact_padding = "x".repeat(MAX_BODY_SIZE_BYTES - fixed);
        let exact = single_recipes_with_branch("Gbranch", &exact_padding, 1, Some(branch)).unwrap();
        assert_eq!(
            final_single(&exact, PullRequestNumber::MAX).body().as_str().len(),
            MAX_BODY_SIZE_BYTES
        );
        let error =
            single_recipes_with_branch("Gbranch", &format!("{exact_padding}x"), 1, Some(branch))
                .unwrap_err();
        assert!(error.to_string().contains("even with sparse history"));
    }

    #[test]
    fn widest_navigation_selects_the_arrow_by_stack_index() {
        let fixture = stack_fixture(&[("Groot", "Root", ""), ("Gtip", "Tip", "")]);
        let history = &fixture.histories[0];
        let rendered = render_body(
            "/octo/widgets",
            None,
            history.id(),
            0,
            "",
            history,
            Navigation::Widest(2),
            HistoryLayout::Full,
        )
        .unwrap();

        assert_eq!(rendered.as_str().matches("👉").count(), 1);
        assert!(rendered.as_str().contains(&format!("👉 #{}", PullRequestNumber::MAX.get())));
    }

    #[test]
    fn amend_and_rebase_history_keeps_each_literal_base() {
        let id = change_id("Ghistory");
        let proposal = (object_id(50_000), object_id(40_004));
        let published = [
            (object_id(10_001), object_id(40_001)),
            (object_id(10_002), object_id(40_002)),
            (object_id(10_003), object_id(40_003)),
        ];
        let change = LocalChange::for_body_test(
            id.clone(),
            proposal.0,
            proposal.1,
            "History".to_owned(),
            "Explain the history.".to_owned(),
        )
        .unwrap();
        let history = ValidatedChangeHistory::for_body_test(id, &published, proposal);
        let recipes = StackBodyRecipes::from_parts(
            link_context("/octo/widgets", None),
            vec![change],
            vec![history],
        )
        .unwrap();
        let body = final_single(&recipes, number(7));
        for (_, parent) in published.into_iter().chain([proposal]) {
            assert!(body.body().as_str().contains(&parent.to_string()));
        }
        let labels = published
            .into_iter()
            .chain([proposal])
            .enumerate()
            .map(|(index, (_, parent))| (parent, format!("<literal-base-v{}>", index + 1)))
            .collect::<Vec<_>>();
        let normalized = normalize_object_ids(
            body.body().as_str().to_owned(),
            &labels.iter().map(|(oid, label)| (*oid, label.as_str())).collect::<Vec<_>>(),
        );
        insta::assert_snapshot!("bounded_amend_rebase_history", normalized);
    }

    #[test]
    fn repeated_revision_history_retains_every_position() {
        let id = change_id("Grepeat");
        let base = object_id(40_001);
        let a = object_id(10_001);
        let b = object_id(10_002);
        let published = [(a, base), (a, base), (b, base), (a, base)];
        let change = LocalChange::for_body_test(
            id.clone(),
            a,
            base,
            "Repeated revision".to_owned(),
            "Repeat without collapsing versions.".to_owned(),
        )
        .unwrap();
        let history = ValidatedChangeHistory::for_body_test(id, &published, (a, base));
        assert_eq!(
            history.projected_versions().map(|(_, revision)| revision.head()).collect::<Vec<_>>(),
            [a, a, b, a]
        );
        let recipes = StackBodyRecipes::from_parts(
            link_context("/octo/widgets", None),
            vec![change],
            vec![history],
        )
        .unwrap();
        let body = final_single(&recipes, number(7));
        assert!(body.body().as_str().contains("**Latest Update:** v4"));
        let normalized =
            normalize_object_ids(body.body().as_str().to_owned(), &[(base, "<shared-base>")]);
        insta::assert_snapshot!("bounded_repeated_a_a_b_a_history", normalized);
    }
}
