//! Pure owned-base publication planning.
//!
//! This module consumes complete correlated evidence and constructs every Git
//! and GitHub action before exposing the first one. Planning performs no
//! network or repository writes, so its state transitions can be validated as
//! one deterministic unit.

use std::borrow::Cow;

use color_eyre::eyre::{Result, bail};

use super::{
    body::{
        BodyLinkContext, BodyRecipeInput, FinalBodyRecipes, GeneratedBody, RenderedBody,
        StackBodyRecipes,
    },
    destination::DefaultBranch,
    github::{
        CompleteCreateReceipts, CorrelatedRepository, CreatePullRequest, PreparedCreates,
        PreparedUpdates, RepositoryTerminalHistories, UpdatePreflight, UpdatePullRequest,
        preflight_updates,
    },
    history::{
        CommitGraphEvidence, NormalizedPublishedHistory, ValidatedChangeHistory,
        ValidatedPublishedHistory,
    },
    local::{GherritPrId, LocalStack},
    publication::{PreparedPushes, plan_owned_base_pushes},
    pull_request::{
        BaseKind, LocalPullRequestObservation, ManagedOpenParts, ManagedOpenPullRequest,
        PullRequestIdentity,
    },
    remote::{ActiveRemoteChanges, ObservedChangeHistory},
};

/// One complete plan whose GitHub projection remains behind Git publication.
pub(super) struct PublicationPlan<'destination> {
    pushes: Option<PreparedPushes<'destination>>,
    projection: ReadyProjection,
}

impl<'destination> PublicationPlan<'destination> {
    fn new(pushes: Option<PreparedPushes<'destination>>, projection: ReadyProjection) -> Self {
        Self { pushes, projection }
    }

    #[cfg(test)]
    fn push_arguments_for_test(&self) -> Vec<(Vec<String>, Vec<String>)> {
        match &self.pushes {
            Some(pushes) => pushes.arguments_for_test(),
            None => Vec::new(),
        }
    }

    /// Consumes the plan and releases projection only after exact Git success.
    pub(super) async fn publish(self) -> Result<ReadyProjection> {
        if let Some(pushes) = self.pushes {
            pushes.publish().await?;
        }
        Ok(self.projection)
    }
}

/// The minimal action available only after the Git acknowledgement barrier.
#[derive(Debug)]
pub(super) enum ReadyProjection {
    NoAction,
    Updates(PreparedUpdates),
    Creates { creates: Box<PreparedCreates>, projection: ProjectionSeed },
}

/// Frozen facts whose only missing inputs are exact created PR identities.
#[derive(Debug)]
pub(super) struct ProjectionSeed {
    entries: Box<[ProjectionEntry]>,
    final_bodies: FinalBodyRecipes,
}

impl ProjectionSeed {
    /// Consumes one exact complete receipt set and prepares every final update.
    pub(super) fn complete(self, receipts: CompleteCreateReceipts) -> Result<PreparedUpdates> {
        let created_ids = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ProjectionEntry::Created { id, .. } => Some(id.clone()),
                ProjectionEntry::Existing(_) => None,
            })
            .collect::<Box<[_]>>();
        let exact = receipts.into_exact(&created_ids)?;
        let bodies = self
            .final_bodies
            .complete(exact.iter().map(|(id, identity)| (id.clone(), identity.number())))?;
        let mut created = exact.into_values().into_vec().into_iter();
        let mut operations = Vec::with_capacity(self.entries.len());

        for (entry, rendered) in self.entries.into_vec().into_iter().zip(bodies.into_vec()) {
            let (body_id, body) = rendered.into_parts();
            if body_id != *entry.id() {
                bail!("final body order does not match the projection seed");
            }
            match entry {
                ProjectionEntry::Existing(existing) => {
                    if let Some(update) = existing.into_update(body)? {
                        operations.push(update);
                    }
                }
                ProjectionEntry::Created { id, base_update } => {
                    let Some((receipt_id, identity)) = created.next() else {
                        bail!("createPullRequest receipts end before the projection seed");
                    };
                    if receipt_id != id {
                        bail!(
                            "createPullRequest receipt for '{}' cannot project change '{}'",
                            receipt_id.as_str(),
                            id.as_str()
                        );
                    }
                    operations.push(UpdatePullRequest::new(
                        identity,
                        None,
                        Some(body.into_string()),
                        base_update,
                    )?);
                }
            }
        }
        if created.next().is_some() {
            bail!("createPullRequest receipts extend beyond the projection seed");
        }
        PreparedUpdates::new(operations)
    }
}

#[derive(Debug)]
enum LocalReality {
    Existing { history: ValidatedChangeHistory, pull_request: ManagedOpenPullRequest },
    Create { history: ValidatedChangeHistory, id: GherritPrId },
}

impl LocalReality {
    fn history(&self) -> &ValidatedChangeHistory {
        match self {
            Self::Existing { history, .. } | Self::Create { history, .. } => history,
        }
    }

    fn into_body_and_projection(self) -> Result<(BodyRecipeInput, ProjectionDraft)> {
        match self {
            Self::Existing { history, pull_request } => {
                let id = history.id().clone();
                let number = pull_request.identity().number();
                Ok((
                    BodyRecipeInput::existing(id, history, number)?,
                    ProjectionDraft::Existing(pull_request.into_validated_parts()),
                ))
            }
            Self::Create { history, id } => {
                Ok((BodyRecipeInput::missing(id.clone(), history)?, ProjectionDraft::Create(id)))
            }
        }
    }
}

enum ProjectionDraft {
    Existing(ManagedOpenParts),
    Create(GherritPrId),
}

#[derive(Debug)]
enum ProjectionEntry {
    Existing(ExistingProjection),
    Created {
        id: GherritPrId,
        // Creation always uses the owned base. Only a root needs a final base
        // update after GitHub assigns its identity.
        base_update: Option<String>,
    },
}

impl ProjectionEntry {
    fn id(&self) -> &GherritPrId {
        match self {
            Self::Existing(existing) => &existing.id,
            Self::Created { id, .. } => id,
        }
    }
}

#[derive(Debug)]
struct ExistingProjection {
    id: GherritPrId,
    identity: PullRequestIdentity,
    observed_body: Box<str>,
    title_update: Option<String>,
    base_update: Option<String>,
}

impl ExistingProjection {
    fn into_update(self, body: GeneratedBody) -> Result<Option<UpdatePullRequest>> {
        let body = (!bodies_equal(&self.observed_body, body.as_str())).then(|| body.into_string());
        if self.title_update.is_none() && body.is_none() && self.base_update.is_none() {
            return Ok(None);
        }
        Ok(Some(UpdatePullRequest::new(self.identity, self.title_update, body, self.base_update)?))
    }
}

/// Consumes complete evidence and constructs one all-or-nothing publication.
pub(super) fn plan_publication<'destination>(
    body_context: BodyLinkContext,
    stack: LocalStack,
    correlated: CorrelatedRepository<'destination>,
    terminal_histories: RepositoryTerminalHistories,
    remote: ActiveRemoteChanges<'destination>,
    graph: &CommitGraphEvidence,
) -> Result<PublicationPlan<'destination>> {
    if stack.is_empty() {
        bail!("publication planning requires a nonempty local stack");
    }
    let (destination, default_branch, local, nonlocal) = remote.into_parts();
    if !body_context.agrees_with(destination) {
        bail!("pull request body context came from a different push repository");
    }
    let (repository, correlated, terminal_histories) =
        correlated.into_planning_parts_for(destination, terminal_histories)?;
    let (repository_id, github_default_branch) = repository.into_parts();
    let default_branch = DefaultBranch::agree(default_branch, github_default_branch)?;
    validate_stack_default(stack.default_branch(), &default_branch)?;
    let (local_pull_requests, nonlocal_pull_requests, initial_identities) = correlated.into_parts();

    if stack.len() != local.len() || stack.len() != local_pull_requests.len() {
        bail!("local stack, Git, and GitHub evidence have different change counts");
    }
    let missing_ids = stack
        .iter()
        .zip(&local_pull_requests)
        .enumerate()
        .map(|(index, (change, pull_request))| {
            if pull_request.id() != change.id() {
                bail!(
                    "GitHub local evidence at stack position {index} identifies '{}', expected '{}'",
                    pull_request.id().as_str(),
                    change.id().as_str()
                );
            }
            Ok(matches!(pull_request, LocalPullRequestObservation::NeedsTerminalProof(_))
                .then(|| change.id().clone()))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let mut exact_empty_ids =
        terminal_histories.into_exact_empty_ids(&missing_ids)?.into_vec().into_iter();

    let realities = stack
        .iter()
        .zip(local.into_vec())
        .zip(local_pull_requests.into_vec())
        .enumerate()
        .map(|(index, ((change, observed), pull_request))| {
            if observed.id() != change.id() || pull_request.id() != change.id() {
                bail!(
                    "local evidence at stack position {index} does not identify change '{}'",
                    change.id().as_str()
                );
            }
            let desired_base = desired_base(index);
            let observed_base = match &pull_request {
                LocalPullRequestObservation::Open(pull_request) => Some(pull_request.base().kind()),
                LocalPullRequestObservation::NeedsTerminalProof(_) => None,
            };
            let root_tip = (desired_base == BaseKind::Default
                || observed_base == Some(BaseKind::Default))
            .then(|| default_branch.tip());
            let history = NormalizedPublishedHistory::from_observation(observed, graph)?
                .with_proposal(change, graph)?
                .validate(graph, root_tip)?;
            match pull_request {
                LocalPullRequestObservation::Open(pull_request) => {
                    if history.published_len() == 0 {
                        bail!(
                            "local change '{}' has an OPEN pull request but no published history",
                            change.id().as_str()
                        );
                    }
                    validate_local_pull_request(
                        &history,
                        &pull_request,
                        desired_base,
                        &default_branch,
                    )?;
                    Ok(LocalReality::Existing { history, pull_request })
                }
                LocalPullRequestObservation::NeedsTerminalProof(_) => {
                    let terminal_id = exact_empty_ids.next().ok_or_else(|| {
                        color_eyre::eyre::eyre!(
                            "terminal-history order ended before local change '{}'",
                            change.id().as_str()
                        )
                    })?;
                    if &terminal_id != change.id() {
                        bail!(
                            "terminal history identifies '{}', expected '{}'",
                            terminal_id.as_str(),
                            change.id().as_str()
                        );
                    }
                    Ok(LocalReality::Create { history, id: terminal_id })
                }
            }
        })
        .collect::<Result<Vec<_>>>()?;
    if exact_empty_ids.next().is_some() {
        bail!("terminal-history order extends beyond the local stack");
    }

    validate_nonlocal(
        nonlocal.into_vec(),
        nonlocal_pull_requests.into_vec(),
        graph,
        &default_branch,
    )?;

    let history_refs = realities.iter().map(LocalReality::history).collect::<Vec<_>>();
    let pushes = plan_owned_base_pushes(destination, &history_refs)?;
    let (body_inputs, drafts): (Vec<_>, Vec<_>) = realities
        .into_iter()
        .map(LocalReality::into_body_and_projection)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .unzip();
    let recipes = StackBodyRecipes::new(body_context, stack, body_inputs)?;
    let projection =
        prepare_projection(repository_id, &default_branch, drafts, initial_identities, recipes)?;

    Ok(PublicationPlan::new(pushes, projection))
}

fn validate_stack_default(stack: &DefaultBranch, agreed: &DefaultBranch) -> Result<()> {
    if stack.name() != agreed.name() {
        bail!("local stack and publication evidence use different default branch names");
    }
    if stack.tip() != agreed.tip() {
        bail!("local stack and publication evidence use different default branch tips");
    }
    Ok(())
}

fn desired_base(index: usize) -> BaseKind {
    if index == 0 { BaseKind::Default } else { BaseKind::Owned }
}

fn validate_local_pull_request(
    history: &ValidatedChangeHistory,
    pull_request: &ManagedOpenPullRequest,
    desired_base: BaseKind,
    default_branch: &DefaultBranch,
) -> Result<()> {
    validate_pull_request(
        history.id(),
        pull_request,
        history.contains_published_head(pull_request.head_oid()),
        history.contains_published_first_parent(pull_request.base().oid()),
        Some(desired_base),
        default_branch,
    )
}

fn validate_nonlocal(
    histories: Vec<ObservedChangeHistory>,
    pull_requests: Vec<ManagedOpenPullRequest>,
    graph: &CommitGraphEvidence,
    default_branch: &DefaultBranch,
) -> Result<()> {
    if histories.len() != pull_requests.len() {
        bail!("nonlocal Git and GitHub evidence have different change counts");
    }
    histories.into_iter().zip(pull_requests).enumerate().try_for_each(
        |(index, (observed, pull_request))| {
            if observed.id() != pull_request.id() {
                bail!("nonlocal evidence at position {index} identifies different changes");
            }
            if observed.id().as_str() == default_branch.name() {
                bail!(
                    "Nonlocal GHerrit change '{}' conflicts with the repository default branch",
                    observed.id().as_str()
                );
            }
            let root_tip =
                (pull_request.base().kind() == BaseKind::Default).then(|| default_branch.tip());
            let history = NormalizedPublishedHistory::from_observation(observed, graph)?
                .validate_existing(graph, root_tip)?;
            validate_nonlocal_pull_request(&history, &pull_request, default_branch)
        },
    )
}

fn validate_nonlocal_pull_request(
    history: &ValidatedPublishedHistory,
    pull_request: &ManagedOpenPullRequest,
    default_branch: &DefaultBranch,
) -> Result<()> {
    validate_pull_request(
        history.id(),
        pull_request,
        history.contains_published_head(pull_request.head_oid()),
        history.contains_published_first_parent(pull_request.base().oid()),
        None,
        default_branch,
    )
}

fn validate_pull_request(
    id: &GherritPrId,
    pull_request: &ManagedOpenPullRequest,
    published_head: bool,
    published_first_parent: bool,
    desired_base: Option<BaseKind>,
    default_branch: &DefaultBranch,
) -> Result<()> {
    if pull_request.id() != id {
        bail!(
            "OPEN pull request for '{}' cannot validate history for '{}'",
            pull_request.id().as_str(),
            id.as_str()
        );
    }
    if !published_head {
        bail!(
            "OPEN pull request for '{}' has a head not present in published history",
            id.as_str()
        );
    }
    match pull_request.base().kind() {
        BaseKind::Default if pull_request.base().oid() != default_branch.tip() => {
            bail!("OPEN pull request for '{}' has the wrong default-branch object ID", id.as_str());
        }
        BaseKind::Owned if !published_first_parent => {
            bail!(
                "OPEN pull request for '{}' has an owned-base object ID not present in published history",
                id.as_str()
            );
        }
        BaseKind::Default | BaseKind::Owned => {}
    }
    if pull_request.has_landing_automation()
        && (pull_request.base().kind() == BaseKind::Owned || desired_base == Some(BaseKind::Owned))
    {
        bail!(
            "OPEN pull request for '{}' cannot use landing automation with an owned base",
            id.as_str()
        );
    }
    Ok(())
}

fn prepare_projection(
    repository_id: String,
    default_branch: &DefaultBranch,
    drafts: Vec<ProjectionDraft>,
    initial_identities: super::pull_request::InitialPullRequestIdentities,
    recipes: StackBodyRecipes,
) -> Result<ReadyProjection> {
    if drafts.len() != recipes.final_bodies().titles().len() {
        bail!("body recipe and projection evidence have different change counts");
    }
    let (provisional, final_bodies) = recipes.into_parts();
    let mut provisional = provisional.into_vec().into_iter();
    let mut create_operations = Vec::new();
    let mut entries = Vec::with_capacity(drafts.len());
    let titles = final_bodies.titles().collect::<Vec<_>>();

    for (index, (draft, (title_id, title))) in drafts.into_iter().zip(titles).enumerate() {
        match draft {
            ProjectionDraft::Existing(open) => {
                let (id, identity, observed_base, observed_title, observed_body) =
                    open.into_parts();
                if &id != title_id {
                    bail!("title recipe at stack position {index} does not match GitHub evidence");
                }
                let desired_base = desired_base(index);
                let desired_base_name = desired_base.branch_name(default_branch.name(), &id);
                let title_update =
                    (observed_title.as_ref() != title.as_str()).then(|| title.as_str().to_owned());
                let base_update =
                    (observed_base.kind() != desired_base).then_some(desired_base_name);
                entries.push(ProjectionEntry::Existing(ExistingProjection {
                    id,
                    identity,
                    observed_body,
                    title_update,
                    base_update,
                }));
            }
            ProjectionDraft::Create(id) => {
                if &id != title_id {
                    bail!("title recipe at stack position {index} does not match terminal history");
                }
                let rendered = provisional.next().ok_or_else(|| {
                    color_eyre::eyre::eyre!(
                        "body recipe omitted provisional change '{}'",
                        title_id.as_str()
                    )
                })?;
                let (body_id, body) = rendered.into_parts();
                if body_id != *title_id {
                    bail!(
                        "provisional body for '{}' cannot create change '{}'",
                        body_id.as_str(),
                        title_id.as_str()
                    );
                }
                let base_update = (desired_base(index) == BaseKind::Default)
                    .then(|| BaseKind::Default.branch_name(default_branch.name(), &id));
                create_operations.push(CreatePullRequest::new(
                    id.clone(),
                    repository_id.clone(),
                    BaseKind::Owned.branch_name(default_branch.name(), &id),
                    title.as_str().to_owned(),
                    body.into_string(),
                ));
                entries.push(ProjectionEntry::Created { id, base_update });
            }
        }
    }
    if let Some(extra) = provisional.next() {
        bail!("body recipe contains unexpected provisional change '{}'", extra.id().as_str());
    }

    if create_operations.is_empty() {
        let bodies = final_bodies.complete([])?;
        let operations = exact_updates(entries, bodies)?;
        return if operations.is_empty() {
            Ok(ReadyProjection::NoAction)
        } else {
            Ok(ReadyProjection::Updates(PreparedUpdates::new(operations)?))
        };
    }

    let representative = final_bodies.representative_bodies().collect::<Vec<_>>();
    let mut conservative = Vec::new();
    for (entry, (body_id, body)) in entries.iter().zip(representative) {
        if entry.id() != body_id {
            bail!("representative body order does not match projection evidence");
        }
        if let ProjectionEntry::Existing(existing) = entry {
            conservative.push(UpdatePreflight::new(
                &existing.identity,
                existing.title_update.as_deref(),
                Some(body.as_str()),
                existing.base_update.as_deref(),
            )?);
        }
    }
    preflight_updates(&conservative)?;

    let creates = PreparedCreates::from_exact(initial_identities, create_operations)?;
    Ok(ReadyProjection::Creates {
        creates: Box::new(creates),
        projection: ProjectionSeed { entries: entries.into_boxed_slice(), final_bodies },
    })
}

fn exact_updates(
    entries: Vec<ProjectionEntry>,
    bodies: Box<[RenderedBody]>,
) -> Result<Vec<UpdatePullRequest>> {
    if entries.len() != bodies.len() {
        bail!("final body count does not match projection evidence");
    }
    entries
        .into_iter()
        .zip(bodies.into_vec())
        .filter_map(|(entry, rendered)| {
            let (body_id, body) = rendered.into_parts();
            if body_id != *entry.id() {
                return Some(Err(color_eyre::eyre::eyre!(
                    "final body order does not match projection evidence"
                )));
            }
            match entry {
                ProjectionEntry::Existing(existing) => existing.into_update(body).transpose(),
                ProjectionEntry::Created { .. } => Some(Err(color_eyre::eyre::eyre!(
                    "a no-create projection contains a created entry"
                ))),
            }
        })
        .collect()
}

/// Defines GHerrit's sole pull-request body comparison equivalence.
///
/// CRLF and LF spellings of a line ending compare equal. Every other byte is
/// part of the generated projection and must match.
fn canonical_body_for_comparison(body: &str) -> Cow<'_, str> {
    if body.contains("\r\n") { Cow::Owned(body.replace("\r\n", "\n")) } else { Cow::Borrowed(body) }
}

fn bodies_equal(observed: &str, desired: &str) -> bool {
    canonical_body_for_comparison(observed) == canonical_body_for_comparison(desired)
}

#[cfg(test)]
mod tests;
