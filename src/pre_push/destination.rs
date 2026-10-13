//! The exact repository and default branch used by one publication attempt.
//!
//! Git permits a named remote to fetch from one URL and push to one or more
//! other URLs. GHerrit cannot safely observe one repository and then write to
//! another, and one atomic push cannot span several repositories. Resolving a
//! `PushDestination` establishes both the exact Git destination and the GitHub
//! repository identity used by the rest of the attempt.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::Command,
    str,
    time::Duration,
};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::{ObjectId, bstr::ByteSlice as _};

use super::{INTERNAL_PRE_PUSH_GIT_DIR_ENV, INTERNAL_PRE_PUSH_REMOTE_ENV, subprocess};
use crate::{manage::PublicBranchName, util};

const DESTINATION_ENV: &str = "GHERRIT_PRIVATE_PUSH_DESTINATION";
const PROXY_ENV: &str = "GHERRIT_PRIVATE_REMOTE_PROXY";
const PROXY_AUTH_METHOD_ENV: &str = "GHERRIT_PRIVATE_REMOTE_PROXY_AUTH_METHOD";
const GIT_CONFIG_PARAMETERS_ENV: &str = "GIT_CONFIG_PARAMETERS";
const DISABLE_HTTP_REDIRECTS: &str = "http.followRedirects=false";
const DISABLE_BUNDLE_URI: &str = "fetch.bundleURI=";
const DISABLE_FOLLOW_TAGS: &str = "push.followTags=false";
const DISABLE_SUBMODULE_PUSHES: &str = "push.recurseSubmodules=no";
const CLEAR_PUSH_OPTIONS: &str = "push.pushOption=";
const INTERNAL_REMOTE_STEM: &str = "gherrit-publication";
const INTERNAL_REMOTE_PROBE_STEM: &str = "gherrit-publication-probe";
const PRIVATE_DESTINATION_REDACTION: &str = "<private-destination-redacted>";
const PRIVATE_TRANSPORT_REDACTION: &str = "<private-transport-redacted>";
const PATH_OR_URL_REDACTION: &str = "<path-or-URL-redacted>";

/// The configured remote's one resolved push destination.
///
/// This value is kept separate from `PushDestination` until Git configuration
/// has established an internal remote name which is absent in the exact
/// command context used for publication.
struct ResolvedDestination {
    configured_remote: util::RemoteName,
    literal: String,
    coordinates: RepositoryCoordinates,
    production_github: bool,
    http_redirect_parameters: Option<OsString>,
}

/// Configured-remote settings which change how Git reaches one destination.
///
/// These values are private for the same reason as the destination itself and
/// deliberately do not implement `Debug`. They are copied explicitly rather
/// than inheriting the configured remote's open-ended configuration surface.
#[derive(Default, Eq, PartialEq)]
struct RemoteTransportSettings {
    proxy: Option<String>,
    proxy_auth_method: Option<String>,
}

/// The exact local repository context for every destination-bound Git child.
///
/// Git's repository-selection environment can override a command's current
/// directory. Retaining the paths selected by `Repo::open` and applying them
/// to every child prevents destination resolution, observation, acquisition,
/// and publication from silently operating in another repository.
///
/// The canonical per-worktree Git directory defines repository identity. The
/// remaining paths are spellings for Git child execution and do not affect
/// equality.
#[derive(Clone)]
struct RepositoryBinding {
    git_dir_identity: util::GitDirIdentity,
    git_dir: PathBuf,
    common_dir: PathBuf,
    work_tree: Option<PathBuf>,
    current_dir: PathBuf,
}

impl PartialEq for RepositoryBinding {
    fn eq(&self, other: &Self) -> bool {
        self.git_dir_identity == other.git_dir_identity
    }
}

impl Eq for RepositoryBinding {}

impl RepositoryBinding {
    fn new(repository: &util::Repo) -> Result<Self> {
        if repository.namespace().is_some() {
            bail!("GHerrit does not support publishing from a namespaced Git repository");
        }

        let opened_from = std::path::absolute(repository.current_dir())
            .wrap_err("Failed to make the Git repository's opening directory absolute")?;
        let absolute = |path: &std::path::Path| {
            Ok::<_, color_eyre::Report>(if path.is_absolute() {
                path.to_owned()
            } else {
                opened_from.join(path)
            })
        };
        // Rust's canonical Windows spelling can begin with `\\?\`, which Git
        // for Windows does not accept as GIT_DIR. Keep gix's discovered path
        // spelling for child execution and the canonical path only for stable
        // repository identity and hook recursion suppression.
        let git_dir_identity = repository.git_dir_identity().clone();
        let git_dir = absolute(repository.path())?;
        let common_dir = absolute(repository.common_dir())?;
        let work_tree = repository.workdir().map(absolute).transpose()?;
        let current_dir = work_tree.clone().unwrap_or_else(|| git_dir.clone());
        Ok(Self { git_dir_identity, git_dir, common_dir, work_tree, current_dir })
    }

    fn bind(&self, command: &mut Command) {
        // These values can select a different repository, worktree, common
        // ref store, ref namespace, or local configuration even when
        // `current_dir` is exact.
        util::clear_git_repository_overrides(command);
        command
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_COMMON_DIR", &self.common_dir)
            .current_dir(&self.current_dir);
        if let Some(work_tree) = &self.work_tree {
            command.env("GIT_WORK_TREE", work_tree);
        }
    }
}

/// A validated GitHub repository identity derived from the exact push
/// destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RepositoryCoordinates {
    owner: String,
    repository: String,
}

/// The exact local repository and remote Git destination represented by a
/// publication client or executable action.
///
/// This value deliberately has no `Debug` implementation: its repository
/// paths and destination literal may be private. Equality is used only to
/// prevent evidence or effects prepared for one target from being relabelled
/// as authority for another target with the same GitHub coordinates.
#[derive(Clone, Eq, PartialEq)]
pub(super) struct PublicationTarget {
    repository: RepositoryBinding,
    literal: String,
    coordinates: RepositoryCoordinates,
}

impl PublicationTarget {
    pub(super) fn coordinates(&self) -> &RepositoryCoordinates {
        &self.coordinates
    }
}

impl RepositoryCoordinates {
    fn new(owner: String, repository: String) -> Option<Self> {
        (valid_repository_component(&owner) && valid_repository_component(&repository))
            .then_some(Self { owner, repository })
    }

    #[cfg(test)]
    pub(super) fn for_test(owner: &str, repository: &str) -> Self {
        Self::new(owner.to_owned(), repository.to_owned())
            .expect("test repository coordinates must be valid")
    }

    pub(super) fn owner(&self) -> &str {
        &self.owner
    }

    pub(super) fn repository(&self) -> &str {
        &self.repository
    }
}

/// One validated push destination for the configured GHerrit remote.
///
/// The destination itself is deliberately private and this type does not
/// implement `Debug`. A local path can contain private information even though
/// credential-bearing URI authorities are unsupported. Callers can pass the
/// destination to Git or use its derived repository identity, but must not log
/// it.
pub(super) struct PushDestination {
    repository: RepositoryBinding,
    resolved: ResolvedDestination,
    internal_remote: String,
    transport: RemoteTransportSettings,
}

/// Exact state of the requested public branch projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RemoteBranchState {
    Absent,
    At(ObjectId),
}

/// The requested public branch and the exact state observed for that same
/// branch.
///
/// Keeping the request identity with its evidence prevents a planner from
/// accidentally pairing one branch name with another branch's remote state.
#[derive(Debug)]
pub(super) struct ObservedPublicBranch {
    name: PublicBranchName,
    state: RemoteBranchState,
}

impl ObservedPublicBranch {
    pub(super) fn into_parts(self) -> (PublicBranchName, RemoteBranchState) {
        (self.name, self.state)
    }

    #[cfg(test)]
    pub(super) fn for_test(name: PublicBranchName, state: RemoteBranchState) -> Self {
        Self { name, state }
    }
}

/// The complete initial observation together with the exact destination
/// capability which produced it.
pub(super) struct InitialRemoteObservation {
    destination: PushDestination,
    refs: ParsedInitialRemoteObservation,
}

#[derive(Debug)]
struct ParsedInitialRemoteObservation {
    default_branch: DefaultBranch,
    public_branch: Option<ObservedPublicBranch>,
}

impl InitialRemoteObservation {
    pub(super) fn into_parts(
        self,
    ) -> (PushDestination, DefaultBranch, Option<ObservedPublicBranch>) {
        let ParsedInitialRemoteObservation { default_branch, public_branch } = self.refs;
        (self.destination, default_branch, public_branch)
    }
}

impl ParsedInitialRemoteObservation {
    #[cfg(test)]
    fn into_parts(self) -> (DefaultBranch, Option<ObservedPublicBranch>) {
        (self.default_branch, self.public_branch)
    }
}

/// The one acquisition behavior selected from exact graph-load evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ExactObjectFetchMode {
    Negotiated,
    Refetch,
}

impl PushDestination {
    #[cfg(test)]
    pub(super) fn for_test() -> Self {
        let repository = util::Repo::open(".").expect("tests run inside the GHerrit repository");
        Self::for_test_in(&repository)
    }

    #[cfg(test)]
    pub(super) fn for_test_in(repository: &util::Repo) -> Self {
        Self::for_test_url_in(repository, "https://github.com/owner/repo.git")
    }

    #[cfg(test)]
    pub(super) fn for_test_url_in(repository: &util::Repo, url: &str) -> Self {
        let configured_remote = util::RemoteName::from_config(b"origin").unwrap();
        let repository = RepositoryBinding::new(repository)
            .expect("test repositories use a supported repository context");
        let resolved = ResolvedDestination::from_git_output(
            configured_remote,
            format!("{url}\n").as_bytes(),
            &repository.current_dir,
        )
        .unwrap();
        Self {
            repository,
            resolved,
            internal_remote: INTERNAL_REMOTE_STEM.to_owned(),
            transport: RemoteTransportSettings::default(),
        }
    }

    /// Resolves the one exact destination Git would use for pushing.
    ///
    /// The configured remote is supplied by the caller so configuration is
    /// decoded and validated exactly once per publication attempt. `--` is
    /// required because Git permits manually configured remote names beginning
    /// with a hyphen.
    pub(super) fn resolve(
        repository: &util::Repo,
        configured_remote: util::RemoteName,
    ) -> Result<Self> {
        // The private adapter below depends on `--config-env`, introduced in
        // Git 2.31. Check explicitly instead of letting an older Git reject an
        // otherwise opaque internal command later in the attempt.
        util::require_git_config_env()?;

        let repository = RepositoryBinding::new(repository)?;
        let mut command = util::cmd(
            "git",
            ["remote", "get-url", "--push", "--all", "--", configured_remote.as_str()],
        );
        repository.bind(&mut command);
        clear_git_transport_diagnostics(&mut command);
        let output = command.output().wrap_err_with(|| {
            format!(
                "Failed to resolve the push destination for GHerrit remote '{}'",
                configured_remote.as_str()
            )
        })?;

        if !output.status.success() {
            bail!(
                "GHerrit remote '{}' has no resolvable push destination",
                configured_remote.as_str()
            );
        }

        let resolved = ResolvedDestination::from_git_output(
            configured_remote,
            &output.stdout,
            &repository.current_dir,
        )?;

        // First inspect the configuration which selected the configured
        // remote and its transport settings. Then add the private destination
        // under a throwaway absent name and inspect again before choosing the
        // name used for network commands. A command-scoped remote URL may
        // participate in `includeIf.hasconfig:remote.*.url`; the second
        // observation materializes any configuration it activates. Git
        // rejects remote URLs in files reached through that condition, so the
        // activated include set depends on the destination, not on which
        // absent probe name carries it.
        let configuration = resolved.inspect_configuration(&repository)?;
        reject_unsupported_remote_transport_configuration(
            resolved.configured_remote.as_str(),
            &configuration,
        )?;
        let transport = resolved.inspect_remote_transport_settings(&repository)?;
        let probe_remote = select_absent_remote(INTERNAL_REMOTE_PROBE_STEM, &configuration);
        let private_configuration =
            resolved.inspect_private_configuration(&repository, &probe_remote, &transport)?;
        let internal_remote = select_absent_remote(INTERNAL_REMOTE_STEM, &private_configuration);

        let destination = Self { repository, resolved, internal_remote, transport };
        destination.inspect_internal_remote_configuration()?;
        destination.ensure_rewrite_fixed_point()?;
        Ok(destination)
    }

    /// Inspects the exact configuration context used by network commands.
    ///
    /// The internal name was absent from the complete configuration observed
    /// with the private destination already present under a throwaway name.
    /// This final inspection defends that choice directly: every planned key
    /// must occur once, carry the exact private value, and be the only key for
    /// the internal remote.
    fn inspect_internal_remote_configuration(&self) -> Result<()> {
        let names = self.inspect_configuration()?;
        let urls = self.inspect_internal_remote_values("url")?;
        let pushurls = self.inspect_internal_remote_values("pushurl")?;
        let proxies = self.inspect_internal_remote_values("proxy")?;
        let proxy_auth_methods = self.inspect_internal_remote_values("proxyAuthMethod")?;
        validate_internal_remote_configuration(
            &self.internal_remote,
            &names,
            ObservedRemoteValues {
                urls: &urls,
                pushurls: &pushurls,
                proxies: &proxies,
                proxy_auth_methods: &proxy_auth_methods,
            },
            self.resolved.literal.as_bytes(),
            &self.transport,
        )
        .wrap_err_with(|| {
            format!(
                "Git configuration changes the private publication remote for GHerrit remote '{}'",
                self.configured_remote()
            )
        })
    }

    fn inspect_configuration(&self) -> Result<Vec<Vec<u8>>> {
        let output = self
            .adapter_command([
                "config".to_owned(),
                "--null".to_owned(),
                "--name-only".to_owned(),
                "--list".to_owned(),
            ])
            .output()
            .wrap_err_with(|| {
                format!(
                    "Failed to inspect the private Git remote for GHerrit remote '{}'",
                    self.configured_remote()
                )
            })?;
        decode_config_names(&output, self.configured_remote(), "private Git remote")
    }

    fn inspect_internal_remote_values(&self, key: &str) -> Result<Vec<Vec<u8>>> {
        let key = format!("remote.{}.{key}", self.internal_remote);
        let output = self
            .adapter_command([
                "config".to_owned(),
                "--null".to_owned(),
                "--get-all".to_owned(),
                key,
            ])
            .output()
            .wrap_err_with(|| {
                format!(
                    "Failed to inspect the private Git remote for GHerrit remote '{}'",
                    self.configured_remote()
                )
            })?;
        decode_optional_config_records(&output, self.configured_remote(), "private Git remote")
    }

    /// Proves that the resolved destination is the literal repository Git will
    /// use for both observation and publication.
    ///
    /// Initial resolution can apply one URL rewrite. Binding that result to the
    /// internal remote can otherwise apply another `insteadOf` rewrite. The
    /// no-network `ls-remote --get-url` mode uses the exact adapter used by
    /// observation and publication: both its ordinary URL and explicit push
    /// URL are the candidate. An explicit push URL makes `pushInsteadOf`
    /// inapplicable, while `insteadOf` affects ordinary and push URLs equally.
    /// Proving that the fetch interpretation is unchanged therefore proves the
    /// exact destination used by both operations.
    fn ensure_rewrite_fixed_point(&self) -> Result<()> {
        let output = self
            .ls_remote(["--get-url".to_owned()], std::iter::empty())
            .output()
            .wrap_err_with(|| {
                format!(
                    "Failed to verify Git URL rewriting for GHerrit remote '{}'",
                    self.configured_remote()
                )
            })?;
        let mut records = git_output_records(&output.stdout);
        if !output.status.success()
            || records.next() != Some(self.resolved.literal.as_bytes())
            || records.next().is_some()
        {
            bail!(
                "Git URL rewrite configuration changes the resolved push destination for GHerrit remote '{}'; chained rewrites are unsupported",
                self.configured_remote()
            );
        }
        Ok(())
    }

    /// Constructs a Git command in the private remote's exact configuration.
    ///
    /// The exact destination is supplied in a private environment variable.
    /// The top-level Git argument list contains only a proved-absent internal
    /// remote name, so GHerrit's command diagnostics and test traces cannot
    /// retain the private local path. Git may pass the URL to a trusted
    /// transport descendant when its protocol requires it. Exactly one URL
    /// and push URL and only the observed optional proxy inputs are added; no
    /// synthesized empty reset values or Git-version-dependent additive
    /// behavior participate.
    fn adapter_command(&self, arguments: impl IntoIterator<Item = String>) -> Command {
        self.resolved.private_remote_command(
            &self.repository,
            &self.internal_remote,
            &self.transport,
            arguments,
        )
    }

    /// Constructs a destination-bearing Git command with redirects disabled.
    ///
    /// Options precede `-- <remote>` and ref patterns or refspecs follow it,
    /// matching Git's remote command grammar.
    fn remote_command(
        &self,
        operation: &str,
        options: impl IntoIterator<Item = String>,
        refs: impl IntoIterator<Item = String>,
    ) -> Command {
        self.adapter_command(
            ["-c".to_owned(), DISABLE_HTTP_REDIRECTS.to_owned(), operation.to_owned()]
                .into_iter()
                .chain(options)
                .chain(["--".to_string(), self.internal_remote.clone()])
                .chain(refs),
        )
    }

    fn ls_remote(
        &self,
        options: impl IntoIterator<Item = String>,
        ref_patterns: impl IntoIterator<Item = String>,
    ) -> Command {
        self.remote_command("ls-remote", options, ref_patterns)
    }

    /// Constructs the exact-local ref observation command.
    ///
    /// Git's positional patterns remain tail filters, so the decoder must
    /// still accept only complete canonical names. The category options ask a
    /// protocol-v2 server to advertise only branch and tag namespaces. They
    /// are fixed here rather than supplied by the caller because all evidence
    /// requested by the exact-local query plan is in one of those namespaces.
    fn exact_ref_observation_command(
        &self,
        ref_patterns: impl IntoIterator<Item = String>,
    ) -> Command {
        self.ls_remote(
            ["--quiet".to_owned(), "--heads".to_owned(), "--tags".to_owned()],
            ref_patterns,
        )
    }

    /// Observes destination refs through the sole bounded execution path.
    ///
    /// This compatibility entry point remains for the active legacy runtime.
    /// The exact-local workflow uses [`Self::observe_refs_from`] so its
    /// repository binding remains explicit while that workflow is dormant.
    pub(super) async fn observe_refs(
        &self,
        options: impl IntoIterator<Item = String>,
        ref_patterns: impl IntoIterator<Item = String>,
    ) -> std::result::Result<subprocess::CommandOutput, subprocess::CommandError> {
        subprocess::output(
            self.ls_remote(options, ref_patterns),
            subprocess::REMOTE_GIT_EXECUTION_TIMEOUT,
        )
        .await
    }

    /// Observes refs from the exact repository bound to an acquisition plan.
    ///
    /// Keeping the raw `ls-remote` command private prevents callers from
    /// bypassing the execution deadline, output limit, descendant cleanup, or
    /// asynchronous process supervision. The repository supplies only the
    /// command's working directory; the destination and every transport
    /// setting still come from this validated value.
    pub(super) async fn observe_refs_from(
        &self,
        repository: &util::Repo,
        ref_patterns: impl IntoIterator<Item = String>,
    ) -> std::result::Result<subprocess::CommandOutput, subprocess::CommandError> {
        let mut command = self.exact_ref_observation_command(ref_patterns);
        command.current_dir(repository.workdir().unwrap_or(repository.path()));
        subprocess::output(command, subprocess::REMOTE_GIT_EXECUTION_TIMEOUT).await
    }

    /// Constructs the sole exact-object acquisition command.
    ///
    /// Source refs are accepted only through `fetch --stdin`, so no advertised
    /// ref, object ID, or destination literal can enter the top-level Git
    /// argument list. The empty refmap is load-bearing: without it, configured
    /// fetch refspecs could update remote-tracking refs even for source-only
    /// wants. An empty bundle URI prevents a configured secondary object
    /// source and its creation-token state from participating in this one
    /// fetch.
    pub(super) fn exact_object_fetch(&self, mode: ExactObjectFetchMode) -> Command {
        let fixed = [
            "--quiet",
            "--no-progress",
            "--no-write-fetch-head",
            "--no-tags",
            "--no-prune",
            "--no-prune-tags",
            "--no-recurse-submodules",
            "--no-auto-maintenance",
            "--no-write-commit-graph",
            "--no-update-shallow",
            "--no-filter",
            "--refmap=",
        ];
        let mode = match mode {
            ExactObjectFetchMode::Negotiated => None,
            ExactObjectFetchMode::Refetch => Some("--refetch"),
        };
        let options = fixed.into_iter().chain(mode).chain(std::iter::once("--stdin"));
        self.adapter_command(
            ["-c", DISABLE_HTTP_REDIRECTS, "-c", DISABLE_BUNDLE_URI, "fetch"]
                .into_iter()
                .chain(options)
                .chain(["--", self.internal_remote.as_str()])
                .map(str::to_owned),
        )
    }

    pub(super) fn push(
        &self,
        options: impl IntoIterator<Item = String>,
        refspecs: impl IntoIterator<Item = String>,
    ) -> Command {
        // `push.followTags` can add refs which are absent from the plan, while
        // `push.recurseSubmodules` can suppress the superproject push or write
        // to submodule remotes. A configured push option is additional server
        // input independent of the planned refspecs. Bind all three behaviors
        // here. Git treats one empty command-scoped push-option value as a
        // reset of all lower-priority values.
        let arguments = [
            "-c".to_owned(),
            DISABLE_HTTP_REDIRECTS.to_owned(),
            "-c".to_owned(),
            DISABLE_FOLLOW_TAGS.to_owned(),
            "-c".to_owned(),
            DISABLE_SUBMODULE_PUSHES.to_owned(),
            "-c".to_owned(),
            CLEAR_PUSH_OPTIONS.to_owned(),
            "push".to_owned(),
        ]
        .into_iter()
        .chain(options)
        .chain(["--".to_owned(), self.internal_remote.clone()])
        .chain(refspecs);
        let mut command = self.adapter_command(arguments);
        command
            .env(INTERNAL_PRE_PUSH_REMOTE_ENV, &self.internal_remote)
            .env(INTERNAL_PRE_PUSH_GIT_DIR_ENV, self.repository.git_dir_identity.as_os_str());
        command
    }

    pub(super) fn configured_remote(&self) -> &str {
        self.resolved.configured_remote.as_str()
    }

    pub(super) fn coordinates(&self) -> &RepositoryCoordinates {
        &self.resolved.coordinates
    }

    pub(super) fn publication_target(&self) -> PublicationTarget {
        PublicationTarget {
            repository: self.repository.clone(),
            literal: self.resolved.literal.clone(),
            coordinates: self.resolved.coordinates.clone(),
        }
    }

    /// Whether the Git destination belongs to the public GitHub forge used by
    /// the production API endpoint.
    ///
    /// The publication entrypoint checks this before initial remote
    /// observation. GitHub client construction repeats the check as defense in
    /// depth. Test-driver runtimes may instead pair a fake or disabled endpoint
    /// with a local Git remote.
    pub(super) fn supports_production_github(&self) -> bool {
        self.resolved.production_github
    }

    pub(super) fn belongs_to(&self, repository: &util::Repo) -> Result<bool> {
        Ok(self.repository == RepositoryBinding::new(repository)?)
    }

    pub(super) fn pr_url(&self, pr_number: u64) -> String {
        format!(
            "https://github.com/{}/{}/pull/{pr_number}",
            self.resolved.coordinates.owner, self.resolved.coordinates.repository
        )
    }

    pub(super) fn repo_url_relative(&self) -> String {
        format!("/{}/{}", self.resolved.coordinates.owner, self.resolved.coordinates.repository)
    }

    /// Returns a stable, credential-free URL for commands shown in PR bodies.
    ///
    /// This projection deliberately comes from validated repository
    /// coordinates rather than the configured remote name or destination
    /// literal. The former may fetch from a different repository, while the
    /// latter may contain a private local path or transport spelling.
    pub(super) fn repo_fetch_url(&self) -> String {
        format!(
            "https://github.com/{}/{}.git",
            self.resolved.coordinates.owner, self.resolved.coordinates.repository
        )
    }

    /// Observes the symbolic default branch and optional exact public branch
    /// through the shared bounded subprocess adapter. Public mode adds one ref
    /// pattern without another network request. No local remote-tracking ref
    /// participates in the result.
    pub(super) async fn observe_initial(
        self,
        public_branch: Option<PublicBranchName>,
    ) -> Result<InitialRemoteObservation> {
        let command = self.initial_observation_command(public_branch.as_ref());
        let refs = observe_initial_command(
            command,
            self.configured_remote(),
            public_branch,
            subprocess::REMOTE_GIT_EXECUTION_TIMEOUT,
            subprocess::REMOTE_GIT_STDOUT_LIMIT,
        )
        .await?;
        Ok(InitialRemoteObservation { destination: self, refs })
    }

    /// Observes only the symbolic default for the active legacy runtime.
    ///
    /// Keep this path independent from the dormant exact-local observation so
    /// the preparatory public-branch model does not change active behavior.
    pub(super) async fn observe_default_branch(&self) -> Result<DefaultBranch> {
        observe_default_branch_command(
            self.ls_remote(["--symref".to_string()], ["HEAD".to_string()]),
            self.configured_remote(),
            subprocess::REMOTE_GIT_EXECUTION_TIMEOUT,
            subprocess::REMOTE_GIT_STDOUT_LIMIT,
        )
        .await
    }

    fn initial_observation_command(&self, public_branch: Option<&PublicBranchName>) -> Command {
        self.ls_remote(
            ["--symref".to_string()],
            std::iter::once("HEAD".to_string())
                .chain(public_branch.map(|branch| format!("refs/heads/{}", branch.as_str()))),
        )
    }

    /// Returns bounded, redacted, terminal-safe context from a normally
    /// completed destination-bearing command.
    ///
    /// This text is never publication evidence. In particular, a nonzero Git
    /// exit with no complete acknowledgement remains indeterminate even when
    /// the diagnostic strongly suggests that a local policy hook rejected it.
    pub(super) fn render_child_diagnostic(
        &self,
        stderr: &[u8],
        stderr_bytes: u64,
    ) -> Option<String> {
        let retained = u64::try_from(stderr.len()).unwrap_or(u64::MAX);
        let omitted_before_suffix = stderr_bytes.saturating_sub(retained);
        let (stderr, omitted) = if omitted_before_suffix == 0 {
            (stderr, 0)
        } else if let Some(newline) = stderr.iter().position(|byte| *byte == b'\n') {
            // The bounded reader retains the end of stderr. If bytes were
            // dropped, the first retained byte can be in the middle of a
            // `remote:` line or a private destination. Without its missing
            // prefix that fragment cannot be classified or safely redacted.
            // Discard through the first retained newline so filtering starts
            // at a complete logical line.
            let discarded = newline.saturating_add(1);
            (
                &stderr[discarded..],
                omitted_before_suffix.saturating_add(u64::try_from(discarded).unwrap_or(u64::MAX)),
            )
        } else {
            // There is no proof that any retained byte belongs to a complete
            // line. The total byte count is still useful and reveals none of
            // the untrusted fragment.
            (&[][..], stderr_bytes)
        };

        // Git prefixes remote-side messages with `remote:`. Those messages
        // are not evidence about a local composite pre-push policy and may
        // contain arbitrary server-private text. Git's own final failure line
        // names the destination and contributes no actionable policy detail.
        let mut diagnostic = String::from_utf8_lossy(stderr)
            .split('\n')
            .filter(|line| {
                let line = line.trim_start();
                !line.starts_with("remote:")
                    && !line.starts_with("error: failed to push some refs to ")
                    && !line.starts_with("error: atomic push failed for ref ")
                    && line != "fatal: the remote end hung up unexpectedly"
                    && !line.starts_with("! [rejected]")
                    && !line.starts_with("To ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        for private in self.private_transport_spellings() {
            if !private.is_empty() {
                diagnostic = diagnostic.replace(private, PRIVATE_TRANSPORT_REDACTION);
            }
        }
        for private in self.private_destination_spellings() {
            if !private.is_empty() {
                diagnostic = diagnostic.replace(&private, PRIVATE_DESTINATION_REDACTION);
            }
        }
        // Git, helpers, and independent hooks can normalize the same private
        // destination differently. It is not possible to enumerate every
        // equivalent spelling, so conservatively replace any remaining
        // whitespace-delimited path, URL, or SCP-style token in full. This
        // leaves ordinary policy prose useful without trusting child output to
        // identify which destination-bearing tokens are harmless.
        diagnostic = redact_path_or_url_tokens(&diagnostic)
            .replace(PRIVATE_DESTINATION_REDACTION, "<private destination>")
            .replace(PRIVATE_TRANSPORT_REDACTION, "<private transport setting>")
            .replace(PATH_OR_URL_REDACTION, "<path or URL redacted>");

        let mut safe = String::with_capacity(diagnostic.len());
        for character in diagnostic.chars() {
            match character {
                '\n' => safe.push('\n'),
                character if !character.is_control() => safe.push(character),
                character => safe.extend(character.escape_default()),
            }
        }
        let safe = safe.trim().to_owned();
        let has_visible_content = safe
            .chars()
            .any(|character| character.is_alphanumeric() || character.is_ascii_punctuation());
        if !has_visible_content && omitted == 0 {
            return None;
        }
        Some(if omitted == 0 {
            safe
        } else if safe.is_empty() {
            format!("[{omitted} earlier diagnostic bytes omitted]")
        } else {
            format!("{safe}\n[{omitted} earlier diagnostic bytes omitted]")
        })
    }

    fn private_destination_spellings(&self) -> Vec<String> {
        let mut spellings = vec![self.resolved.literal.clone()];
        if let Some(path) = local_destination_path(&self.resolved.literal) {
            if let Ok(absolute) = std::path::absolute(&path) {
                spellings.push(absolute.to_string_lossy().into_owned());
            }
            if let Ok(canonical) = fs::canonicalize(&path) {
                spellings.push(canonical.to_string_lossy().into_owned());
            }
        }

        let RepositoryCoordinates { owner, repository } = &self.resolved.coordinates;
        for suffix in [
            format!("{owner}/{repository}.git"),
            format!("{owner}/{repository}"),
            format!(r"{owner}\{repository}.git"),
            format!(r"{owner}\{repository}"),
        ] {
            spellings.push(suffix);
        }
        spellings.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
        spellings.dedup();
        spellings
    }

    fn private_transport_spellings(&self) -> Vec<&str> {
        let mut spellings =
            [self.transport.proxy.as_deref(), self.transport.proxy_auth_method.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
        spellings.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
        spellings.dedup();
        spellings
    }
}

async fn observe_default_branch_command(
    command: Command,
    configured_remote: &str,
    timeout: Duration,
    stdout_limit: usize,
) -> Result<DefaultBranch> {
    let output = run_initial_observation(command, configured_remote, timeout, stdout_limit).await?;
    parse_default_branch(output.stdout()).wrap_err_with(|| {
        format!(
            "GHerrit remote '{configured_remote}' did not report one valid symbolic default branch"
        )
    })
}

async fn observe_initial_command(
    command: Command,
    configured_remote: &str,
    public_branch: Option<PublicBranchName>,
    timeout: Duration,
    stdout_limit: usize,
) -> Result<ParsedInitialRemoteObservation> {
    let output = run_initial_observation(command, configured_remote, timeout, stdout_limit).await?;
    parse_initial_remote_observation(output.stdout(), public_branch).wrap_err_with(|| {
        format!(
            "GHerrit remote '{configured_remote}' did not report one valid initial ref observation"
        )
    })
}

async fn run_initial_observation(
    command: Command,
    configured_remote: &str,
    timeout: Duration,
    stdout_limit: usize,
) -> Result<subprocess::CommandOutput> {
    let output = subprocess::output_with_stdout_limit(command, timeout, stdout_limit)
        .await
        .wrap_err_with(|| format!("Failed to observe GHerrit remote '{configured_remote}'"))?;
    if !output.status().success() {
        bail!("`git ls-remote --symref` failed for GHerrit remote '{configured_remote}'");
    }
    Ok(output)
}

fn local_destination_path(destination: &str) -> Option<PathBuf> {
    match parse_destination(destination)? {
        ParsedDestination::Local { path } => Some(path),
        ParsedDestination::Uri { .. } | ParsedDestination::Scp { .. } => None,
    }
}

fn redact_path_or_url_tokens(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut token_start = 0;
    for (index, character) in input.char_indices() {
        if character.is_whitespace() {
            push_redacted_token(&mut output, &input[token_start..index]);
            output.push(character);
            token_start = index + character.len_utf8();
        }
    }
    push_redacted_token(&mut output, &input[token_start..]);
    output
}

fn push_redacted_token(output: &mut String, token: &str) {
    if is_path_or_url_token(token) {
        output.push_str(PATH_OR_URL_REDACTION);
    } else {
        output.push_str(token);
    }
}

fn is_path_or_url_token(token: &str) -> bool {
    token.contains(['/', '\\'])
        || token.contains("://")
        || token.split_once(':').is_some_and(|(authority, path)| {
            !authority.is_empty()
                && !path.is_empty()
                && (authority.contains('@') || path.contains(".git"))
        })
}

impl ResolvedDestination {
    fn from_git_output(
        configured_remote: util::RemoteName,
        output: &[u8],
        current_dir: &Path,
    ) -> Result<Self> {
        let mut destinations = git_output_records(output);
        let Some(destination) = destinations.next() else {
            bail!("GHerrit remote '{}' has no push destination", configured_remote.as_str());
        };
        let additional = destinations.count();
        if additional != 0 {
            bail!(
                "GHerrit remote '{}' has {} push destinations; exactly one is required for atomic publication",
                configured_remote.as_str(),
                additional + 1
            );
        }
        if destination.is_empty() {
            bail!("GHerrit remote '{}' has no push destination", configured_remote.as_str());
        }

        let mut literal = str::from_utf8(destination).map(str::to_owned).map_err(|_| {
            eyre!(
                "GHerrit remote '{}' has a non-UTF-8 push destination",
                configured_remote.as_str()
            )
        })?;
        let parsed = parse_destination(&literal);
        let production_github = parsed.as_ref().is_some_and(targets_production_github);
        let coordinates = match parsed {
            Some(ParsedDestination::Local { path }) => {
                let path = if path.is_absolute() { path } else { current_dir.join(path) };
                let canonical = dunce::canonicalize(path).map_err(|_| {
                    eyre!(
                        "The local push destination for GHerrit remote '{}' cannot be resolved",
                        configured_remote.as_str()
                    )
                })?;
                let coordinates = local_repository_identity(&canonical).ok_or_else(|| {
                    eyre!(
                        "The push destination for GHerrit remote '{}' does not identify a supported GitHub repository",
                        configured_remote.as_str()
                    )
                })?;
                literal = canonical
                    .into_os_string()
                    .into_string()
                    .map_err(|_| {
                        eyre!(
                            "The local push destination for GHerrit remote '{}' is not valid UTF-8",
                            configured_remote.as_str()
                        )
                    })?;
                Some(coordinates)
            }
            Some(ParsedDestination::Uri { scheme, authority, path }) => {
                if authority.contains('@') {
                    bail!(
                        "The push destination for GHerrit remote '{}' contains URI user information, which GHerrit does not support; use a Git credential helper or an SCP-style SSH destination instead",
                        configured_remote.as_str()
                    );
                }
                if scheme.eq_ignore_ascii_case("file") {
                    bail!(
                        "The push destination for GHerrit remote '{}' uses an unsupported file URL; configure its native filesystem path instead",
                        configured_remote.as_str()
                    );
                }
                slash_repository_identity(path)
            }
            Some(ParsedDestination::Scp { authority, path })
                if !authority.is_empty()
                    && !authority.chars().any(char::is_whitespace)
                    && !path.is_empty() =>
            {
                slash_repository_identity(path)
            }
            Some(ParsedDestination::Scp { .. }) => None,
            None => None,
        }
        .ok_or_else(|| {
            eyre!(
                "The push destination for GHerrit remote '{}' does not identify a supported GitHub repository",
                configured_remote.as_str()
            )
        })?;

        let http_redirect_parameters = command_scope_http_redirect_parameters(
            &literal,
            env::var_os(GIT_CONFIG_PARAMETERS_ENV).as_deref(),
        );

        Ok(Self {
            configured_remote,
            literal,
            coordinates,
            production_github,
            http_redirect_parameters,
        })
    }

    /// Reads configuration which is active before GHerrit adds any remote.
    fn inspect_configuration(&self, repository: &RepositoryBinding) -> Result<Vec<Vec<u8>>> {
        let mut command = util::cmd("git", ["config", "--null", "--name-only", "--list"]);
        repository.bind(&mut command);
        clear_git_transport_diagnostics(&mut command);
        let output = command.output().wrap_err_with(|| {
            format!(
                "Failed to inspect Git configuration while resolving GHerrit remote '{}'",
                self.configured_remote.as_str()
            )
        })?;
        decode_config_names(&output, self.configured_remote.as_str(), "Git configuration")
    }

    /// Reads the configuration active after the private destination exists.
    ///
    /// The probe is used only by `git config`; no transport consults it. The
    /// private destination remains in the child environment rather than its
    /// argument list, exactly as it does for the final network commands.
    fn inspect_private_configuration(
        &self,
        repository: &RepositoryBinding,
        probe_remote: &str,
        transport: &RemoteTransportSettings,
    ) -> Result<Vec<Vec<u8>>> {
        let output = self
            .private_remote_command(
                repository,
                probe_remote,
                transport,
                [
                    "config".to_owned(),
                    "--null".to_owned(),
                    "--name-only".to_owned(),
                    "--list".to_owned(),
                ],
            )
            .output()
            .wrap_err_with(|| {
                format!(
                    "Failed to inspect private Git configuration while resolving GHerrit remote '{}'",
                    self.configured_remote.as_str()
                )
            })?;
        decode_config_names(&output, self.configured_remote.as_str(), "private Git configuration")
    }

    /// Reads the effective configured-remote values from the same active
    /// configuration that can affect publication. `git config --get`
    /// deliberately selects Git's effective last value when a key occurs more
    /// than once.
    fn inspect_remote_transport_settings(
        &self,
        repository: &RepositoryBinding,
    ) -> Result<RemoteTransportSettings> {
        Ok(RemoteTransportSettings {
            proxy: self.inspect_remote_transport_setting(repository, "proxy")?,
            proxy_auth_method: self
                .inspect_remote_transport_setting(repository, "proxyAuthMethod")?,
        })
    }

    fn inspect_remote_transport_setting(
        &self,
        repository: &RepositoryBinding,
        key: &str,
    ) -> Result<Option<String>> {
        let key = format!("remote.{}.{key}", self.configured_remote.as_str());
        let mut command =
            util::cmd("git", ["config".to_owned(), "--null".to_owned(), "--get".to_owned(), key]);
        repository.bind(&mut command);
        clear_git_transport_diagnostics(&mut command);
        let output = command.output().wrap_err_with(|| {
            format!(
                "Failed to inspect Git transport configuration for GHerrit remote '{}'",
                self.configured_remote.as_str()
            )
        })?;
        let records = decode_optional_config_records(
            &output,
            self.configured_remote.as_str(),
            "Git transport configuration",
        )?;
        let ([] | [_]) = records.as_slice() else {
            bail!(
                "Git reported malformed transport configuration while resolving GHerrit remote '{}'",
                self.configured_remote.as_str()
            );
        };
        let value = records
            .into_iter()
            .next()
            .map(|value| {
                String::from_utf8(value).map_err(|_| {
                    eyre!(
                        "GHerrit remote '{}' has a non-UTF-8 transport setting",
                        self.configured_remote.as_str()
                    )
                })
            })
            .transpose()?;
        if value.as_ref().is_some_and(|value| value.chars().any(char::is_control)) {
            bail!(
                "GHerrit remote '{}' has a transport setting containing control characters",
                self.configured_remote.as_str()
            );
        }
        Ok(value)
    }

    /// Adds the private URL and explicit transport inputs for an absent name.
    fn private_remote_command(
        &self,
        repository: &RepositoryBinding,
        remote: &str,
        transport: &RemoteTransportSettings,
        arguments: impl IntoIterator<Item = String>,
    ) -> Command {
        let url_key = format!("remote.{remote}.url");
        let pushurl_key = format!("remote.{remote}.pushurl");
        let proxy_key = format!("remote.{remote}.proxy");
        let proxy_auth_method_key = format!("remote.{remote}.proxyAuthMethod");
        let transport_arguments = [
            transport.proxy.as_ref().map(|_| format!("--config-env={proxy_key}={PROXY_ENV}")),
            transport
                .proxy_auth_method
                .as_ref()
                .map(|_| format!("--config-env={proxy_auth_method_key}={PROXY_AUTH_METHOD_ENV}")),
        ];
        let arguments = [
            format!("--config-env={url_key}={DESTINATION_ENV}"),
            format!("--config-env={pushurl_key}={DESTINATION_ENV}"),
        ]
        .into_iter()
        .chain(transport_arguments.into_iter().flatten())
        .chain(arguments);
        let mut command = util::cmd("git", arguments);
        repository.bind(&mut command);
        command.env(DESTINATION_ENV, &self.literal);
        match &transport.proxy {
            Some(proxy) => command.env(PROXY_ENV, proxy),
            None => command.env_remove(PROXY_ENV),
        };
        match &transport.proxy_auth_method {
            Some(method) => command.env(PROXY_AUTH_METHOD_ENV, method),
            None => command.env_remove(PROXY_AUTH_METHOD_ENV),
        };
        if let Some(parameters) = &self.http_redirect_parameters {
            command.env(GIT_CONFIG_PARAMETERS_ENV, parameters);
        }
        clear_git_transport_diagnostics(&mut command);
        command
    }
}

/// Appends an exact URL-scoped redirect override to Git's command scope.
///
/// Git chooses the longest matching `http.<url>.*` key before considering a
/// global `-c http.followRedirects=false`, so the broad command-line setting
/// alone cannot defeat a destination-scoped value. `GIT_CONFIG_PARAMETERS` is
/// how an enclosing `git -c` invocation passes its settings to hooks. Keeping
/// those parameters and appending the equally scoped override preserves
/// credential, proxy, and SSH inputs while ensuring our later value wins. The
/// destination remains in the top-level Git child's environment alongside
/// `DESTINATION_ENV` rather than entering that child's argument list. Git may
/// pass the value to a trusted transport descendant.
fn command_scope_http_redirect_parameters(
    destination: &str,
    inherited: Option<&OsStr>,
) -> Option<OsString> {
    let (scheme, _) = destination.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }

    let key = format!("http.{destination}.followRedirects");
    let override_parameter =
        format!("{}={}", quote_git_config_parameter(&key), quote_git_config_parameter("false"));
    let mut parameters = inherited.map(OsStr::to_os_string).unwrap_or_default();
    if !parameters.is_empty() {
        parameters.push(" ");
    }
    parameters.push(override_parameter);
    Some(parameters)
}

/// Uses the single-quote encoding parsed by `GIT_CONFIG_PARAMETERS`.
fn quote_git_config_parameter(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    value.chars().for_each(|character| match character {
        '\'' => quoted.push_str("'\\''"),
        character => quoted.push(character),
    });
    quoted.push('\'');
    quoted
}

/// Chooses the first generated remote with no configuration key.
///
/// Configuration names, not `git remote` output, are authoritative: a partial
/// remote and a case-variant key can both alter Git's interpretation. At most
/// one generated candidate is blocked per configuration record, so trying one
/// more name than there are records proves that this finite search succeeds.
fn select_absent_remote(stem: &str, configuration: &[Vec<u8>]) -> String {
    (0..=configuration.len())
        .map(|index| match index {
            0 => stem.to_owned(),
            _ => format!("{stem}-{index}"),
        })
        .find(|candidate| {
            configuration.iter().all(|key| remote_configuration_suffix(key, candidate).is_none())
        })
        .expect("N configuration records cannot block N + 1 generated remote names")
}

fn remote_configuration_suffix<'a>(key: &'a [u8], remote: &str) -> Option<&'a [u8]> {
    let prefix = format!("remote.{remote}.");
    key.get(..prefix.len())
        .filter(|actual| actual.eq_ignore_ascii_case(prefix.as_bytes()))
        .map(|_| &key[prefix.len()..])
}

/// Returns a key belonging to the configured remote under Git's matching
/// rules. Section and variable names are case-insensitive, while subsection
/// names (including remote names) are case-sensitive.
fn configured_remote_configuration_suffix<'a>(key: &'a [u8], remote: &str) -> Option<&'a [u8]> {
    let section = b"remote.";
    let remainder =
        key.get(section.len()..).filter(|_| key[..section.len()].eq_ignore_ascii_case(section))?;
    let remainder = remainder.strip_prefix(remote.as_bytes())?;
    remainder.strip_prefix(b".")
}

/// Rejects configured-remote settings whose behavior cannot be represented by
/// the private adapter without weakening its exact-destination contract.
fn reject_unsupported_remote_transport_configuration(
    remote: &str,
    configuration: &[Vec<u8>],
) -> Result<()> {
    let mut unsupported = configuration
        .iter()
        .filter_map(|key| configured_remote_configuration_suffix(key, remote))
        .filter_map(|suffix| {
            ["uploadpack", "receivepack", "vcs", "serverOption"]
                .into_iter()
                .find(|name| suffix.eq_ignore_ascii_case(name.as_bytes()))
        })
        .collect::<Vec<_>>();
    unsupported.sort_unstable();
    unsupported.dedup();
    if !unsupported.is_empty() {
        bail!(
            "GHerrit remote '{remote}' configures unsupported transport settings: {}",
            unsupported.join(", ")
        );
    }
    Ok(())
}

struct ObservedRemoteValues<'a> {
    urls: &'a [Vec<u8>],
    pushurls: &'a [Vec<u8>],
    proxies: &'a [Vec<u8>],
    proxy_auth_methods: &'a [Vec<u8>],
}

fn validate_internal_remote_configuration(
    remote: &str,
    names: &[Vec<u8>],
    values: ObservedRemoteValues<'_>,
    destination: &[u8],
    transport: &RemoteTransportSettings,
) -> Result<()> {
    let ObservedRemoteValues { urls, pushurls, proxies, proxy_auth_methods } = values;
    let mut url_names = 0;
    let mut pushurl_names = 0;
    let mut proxy_names = 0;
    let mut proxy_auth_method_names = 0;
    for suffix in names.iter().filter_map(|key| remote_configuration_suffix(key, remote)) {
        if suffix.eq_ignore_ascii_case(b"url") {
            url_names += 1;
        } else if suffix.eq_ignore_ascii_case(b"pushurl") {
            pushurl_names += 1;
        } else if suffix.eq_ignore_ascii_case(b"proxy") {
            proxy_names += 1;
        } else if suffix.eq_ignore_ascii_case(b"proxyAuthMethod") {
            proxy_auth_method_names += 1;
        } else {
            bail!("the private remote has an unexpected configuration key");
        }
    }

    if url_names != 1
        || pushurl_names != 1
        || urls.len() != 1
        || pushurls.len() != 1
        || urls[0] != destination
        || pushurls[0] != destination
        || !one_optional_value_matches(proxies, transport.proxy.as_deref())
        || !one_optional_value_matches(proxy_auth_methods, transport.proxy_auth_method.as_deref())
        || proxy_names != usize::from(transport.proxy.is_some())
        || proxy_auth_method_names != usize::from(transport.proxy_auth_method.is_some())
    {
        bail!("the private remote does not have exactly the planned transport configuration");
    }
    Ok(())
}

fn one_optional_value_matches(values: &[Vec<u8>], expected: Option<&str>) -> bool {
    match (values, expected) {
        ([], None) => true,
        ([actual], Some(expected)) => actual == expected.as_bytes(),
        _ => false,
    }
}

fn decode_config_names(
    output: &std::process::Output,
    configured_remote: &str,
    description: &str,
) -> Result<Vec<Vec<u8>>> {
    let records = decode_config_records(output, configured_remote, description)?;
    if records.iter().any(Vec::is_empty) {
        bail!(
            "Git reported malformed {description} while resolving GHerrit remote '{configured_remote}'"
        );
    }
    Ok(records)
}

fn decode_config_records(
    output: &std::process::Output,
    configured_remote: &str,
    description: &str,
) -> Result<Vec<Vec<u8>>> {
    if !output.status.success() {
        bail!("Failed to inspect {description} for GHerrit remote '{configured_remote}'");
    }
    let Some(records) = nul_terminated_records(&output.stdout) else {
        bail!(
            "Git reported malformed {description} while resolving GHerrit remote '{configured_remote}'"
        );
    };
    Ok(records)
}

/// Decodes a config lookup for which Git's exit status 1 means no value.
fn decode_optional_config_records(
    output: &std::process::Output,
    configured_remote: &str,
    description: &str,
) -> Result<Vec<Vec<u8>>> {
    if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
        return Ok(Vec::new());
    }
    decode_config_records(output, configured_remote, description)
}

fn nul_terminated_records(output: &[u8]) -> Option<Vec<Vec<u8>>> {
    if output.is_empty() {
        return Some(Vec::new());
    }
    output
        .strip_suffix(&[0])
        .map(|records| records.split(|byte| *byte == 0).map(<[u8]>::to_vec).collect())
}

/// Prevents Git's transport diagnostics from persisting a private URL.
///
/// Trace variable families grow over time, so this removes every inherited
/// `GIT_TRACE*` spelling instead of maintaining a finite list. Trace2 targets
/// can come from system or global configuration and ignore command-line
/// configuration, so the three documented environment controls are then set
/// to their explicit off value. Git also has a separate curl verbosity switch
/// which can expose HTTP transport details.
fn clear_git_transport_diagnostics(command: &mut Command) {
    clear_git_transport_diagnostics_from(command, env::vars_os().map(|(name, _)| name));
}

fn clear_git_transport_diagnostics_from(
    command: &mut Command,
    inherited_names: impl IntoIterator<Item = std::ffi::OsString>,
) {
    for name in inherited_names {
        let bytes = name.as_os_str().as_encoded_bytes();
        if is_git_transport_diagnostic(bytes) {
            command.env_remove(name);
        }
    }
    for name in ["GIT_TRACE2", "GIT_TRACE2_PERF", "GIT_TRACE2_EVENT"] {
        command.env(name, "0");
    }
}

fn is_git_transport_diagnostic(name: &[u8]) -> bool {
    name.get(..b"GIT_TRACE".len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"GIT_TRACE"))
        || name.eq_ignore_ascii_case(b"GIT_CURL_VERBOSE")
}

/// One repository default branch, including the exact commit it names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DefaultBranch {
    name: String,
    tip: ObjectId,
}

impl DefaultBranch {
    pub(super) fn new(name: String, tip: ObjectId) -> Result<Self> {
        let full_name = format!("refs/heads/{name}");
        let validated = gix::refs::FullName::try_from(full_name.as_str())
            .wrap_err("The repository default branch has an invalid Git ref name")?;
        if validated.category() != Some(gix::refs::Category::LocalBranch) {
            bail!("The repository default branch is not a local branch");
        }
        if name.is_empty() || name.chars().any(char::is_control) {
            bail!("The repository default branch has an invalid name");
        }
        if name == "gherrit-bases" || name.starts_with("gherrit-bases/") {
            bail!(
                "The repository default branch is in GHerrit's reserved 'gherrit-bases' namespace"
            );
        }
        if tip.is_null() {
            bail!("The repository default branch has a null object ID");
        }
        Ok(Self { name, tip })
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }

    pub(super) fn full_ref_name(&self) -> String {
        format!("refs/heads/{}", self.name)
    }

    pub(super) fn tip(&self) -> ObjectId {
        self.tip
    }

    /// Requires Git and GitHub to report the same default branch.
    ///
    /// The returned value binds later planning to the exact name and object ID
    /// seen by both reads. This comparison does not make either independent
    /// read an atomic snapshot of its backend.
    pub(super) fn agree(git: Self, github: Self) -> Result<Self> {
        if git.name != github.name {
            bail!(
                "Git and GitHub disagree about the repository default branch name ('{}' versus '{}')",
                git.name,
                github.name
            );
        }
        if git.tip != github.tip {
            bail!("Git and GitHub disagree about the tip of default branch '{}'", git.name);
        }

        Ok(git)
    }
}

/// The only destination syntaxes GHerrit carries into publication.
///
/// Parsing syntax once keeps Git's destination, repository coordinates, and
/// later forge checks from disagreeing about where an authority or path begins.
enum ParsedDestination<'a> {
    Local { path: PathBuf },
    Uri { scheme: &'a str, authority: &'a str, path: &'a str },
    Scp { authority: &'a str, path: &'a str },
}

fn parse_destination(destination: &str) -> Option<ParsedDestination<'_>> {
    if destination.is_empty()
        || destination.chars().any(char::is_control)
        || destination.contains(['?', '#'])
    {
        return None;
    }

    if let Some((scheme, rest)) = destination.split_once("://") {
        if !valid_scheme(scheme) {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        if authority.chars().any(char::is_whitespace) || path.is_empty() {
            return None;
        }
        // Only file URLs can have an empty authority. They are classified here
        // so resolution can reject that syntax with actionable advice.
        if authority.is_empty() && !scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        return Some(ParsedDestination::Uri { scheme, authority, path });
    }

    match split_scp_destination(destination) {
        ScpDestination::NotScp => {}
        ScpDestination::Invalid => return None,
        ScpDestination::Valid { authority, path } => {
            return Some(ParsedDestination::Scp { authority, path });
        }
    }

    Some(ParsedDestination::Local { path: PathBuf::from(destination) })
}

enum ScpDestination<'a> {
    NotScp,
    Invalid,
    Valid { authority: &'a str, path: &'a str },
}

fn split_scp_destination(destination: &str) -> ScpDestination<'_> {
    let bytes = destination.as_bytes();
    if cfg!(windows) && bytes.get(1) == Some(&b':') && bytes[0].is_ascii_alphabetic() {
        // Both absolute and drive-relative Windows paths belong to Git's local
        // path grammar. Classify them before looking for an SCP separator.
        return ScpDestination::NotScp;
    }

    let first_path_separator = destination.find(['/', '\\']);
    let bracket = destination.char_indices().find_map(|(index, character)| {
        (character == '[' && (index == 0 || bytes.get(index.wrapping_sub(1)) == Some(&b'@')))
            .then_some(index)
    });
    let separator = if let Some(open) = bracket
        .filter(|open| first_path_separator.is_none_or(|path_separator| *open < path_separator))
    {
        let Some(close) = destination[open + 1..].find(']').map(|offset| open + 1 + offset) else {
            return ScpDestination::Invalid;
        };
        if bytes.get(close + 1) != Some(&b':') {
            return ScpDestination::Invalid;
        }
        close + 1
    } else {
        let Some(colon) = destination.find(':') else {
            return ScpDestination::NotScp;
        };
        if first_path_separator.is_some_and(|path_separator| colon > path_separator) {
            return ScpDestination::NotScp;
        }
        colon
    };
    ScpDestination::Valid {
        authority: &destination[..separator],
        path: &destination[separator + 1..],
    }
}

fn slash_repository_identity(path: &str) -> Option<RepositoryCoordinates> {
    if path.ends_with('/') || path.contains('\\') {
        return None;
    }
    let components = path.split('/').collect::<Vec<_>>();
    let [owner, repository] = components.as_slice() else {
        return None;
    };
    repository_components(owner, repository)
}

fn local_repository_identity(path: &Path) -> Option<RepositoryCoordinates> {
    let repository = path.file_name()?.to_str()?;
    let owner = path.parent()?.file_name()?.to_str()?;
    repository_components(owner, repository)
}

fn repository_components(owner: &str, repository: &str) -> Option<RepositoryCoordinates> {
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    RepositoryCoordinates::new(owner.to_owned(), repository.to_owned())
}

/// Whether one parsed destination uses a supported authenticated push
/// transport to the public GitHub service.
///
/// GitHub supports pushes over HTTPS and SSH. Plain HTTP cannot authenticate
/// the service, and GitHub has disabled the unauthenticated Git protocol.
/// Scheme spelling is deliberately case-sensitive so a custom remote helper
/// cannot impersonate a built-in transport.
fn targets_production_github(destination: &ParsedDestination<'_>) -> bool {
    match destination {
        ParsedDestination::Uri { scheme, authority, .. } => {
            ["https", "ssh"].contains(scheme) && authority.eq_ignore_ascii_case("github.com")
        }
        ParsedDestination::Scp { authority, .. } => {
            authority.eq_ignore_ascii_case("git@github.com")
                || authority.eq_ignore_ascii_case("github.com")
        }
        ParsedDestination::Local { .. } => false,
    }
}

fn valid_scheme(scheme: &str) -> bool {
    !scheme.is_empty()
        && scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic()
                || index != 0 && matches!(byte, b'0'..=b'9' | b'+' | b'.' | b'-')
        })
}

fn valid_repository_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn parse_default_branch(output: &[u8]) -> Result<DefaultBranch> {
    let ParsedInitialRemoteObservation { default_branch, public_branch } =
        parse_initial_remote_observation(output, None)?;
    debug_assert!(public_branch.is_none());
    Ok(default_branch)
}

fn parse_initial_remote_observation(
    output: &[u8],
    requested_public_branch: Option<PublicBranchName>,
) -> Result<ParsedInitialRemoteObservation> {
    let mut symbolic_head = None;
    let mut direct_head = None;
    let requested_public_ref =
        requested_public_branch.as_ref().map(|branch| format!("refs/heads/{}", branch.as_str()));
    let mut direct_public_branch = None;

    for record in git_output_records(output) {
        let mut fields = record.split(|byte| *byte == b'\t');
        let (Some(value), Some(name), None) = (fields.next(), fields.next(), fields.next()) else {
            bail!("malformed `git ls-remote --symref` record");
        };
        if let Some(target) = value.strip_prefix(b"ref: ") {
            validate_advertised_ref_name(name)?;
            let target = gix::refs::FullName::try_from(target.as_bstr())
                .wrap_err("symbolic remote ref has an invalid target")?;
            if name == b"HEAD" && symbolic_head.replace(target).is_some() {
                bail!("duplicate symbolic HEAD");
            }
            if requested_public_ref.as_deref().is_some_and(|expected| name == expected.as_bytes()) {
                bail!("requested public branch is symbolic");
            }
        } else {
            validate_direct_advertised_ref_name(name)?;
            let object_id =
                ObjectId::from_hex(value).wrap_err("remote ref value is not an object ID")?;
            if object_id.is_null() {
                bail!("remote ref has a null object ID");
            }
            if name == b"HEAD" && direct_head.replace(object_id).is_some() {
                bail!("duplicate direct HEAD");
            }
            if requested_public_ref.as_deref().is_some_and(|expected| name == expected.as_bytes())
                && direct_public_branch.replace(object_id).is_some()
            {
                bail!("duplicate requested public branch");
            }
        }
    }

    let symbolic_head = symbolic_head.ok_or_else(|| eyre!("missing symbolic HEAD"))?;
    let direct_head = direct_head.ok_or_else(|| eyre!("missing direct HEAD"))?;
    if symbolic_head.category() != Some(gix::refs::Category::LocalBranch) {
        bail!("symbolic HEAD does not target a local branch");
    }
    let branch = symbolic_head
        .as_bstr()
        .strip_prefix(b"refs/heads/")
        .ok_or_else(|| eyre!("symbolic HEAD does not target a local branch"))?;
    let branch = str::from_utf8(branch).wrap_err("default branch name is not UTF-8")?.to_owned();
    let default_branch = DefaultBranch::new(branch, direct_head)?;
    // Git output alone cannot distinguish these two states from a hidden or
    // dangling symbolic ref on an arbitrary server. Production reaches this
    // decoder only after selecting GitHub.com, whose ref contract makes
    // absent-or-direct exhaustive. Custom test destinations promise the same
    // fixture property. An explicitly advertised symbolic ref was rejected
    // above as a defensive check.
    let public_branch = requested_public_branch.map(|name| ObservedPublicBranch {
        name,
        state: direct_public_branch.map_or(RemoteBranchState::Absent, RemoteBranchState::At),
    });
    Ok(ParsedInitialRemoteObservation { default_branch, public_branch })
}

fn validate_advertised_ref_name(name: &[u8]) -> Result<()> {
    if name == b"HEAD" {
        return Ok(());
    }
    gix::refs::FullName::try_from(name.as_bstr()).wrap_err("remote ref has an invalid name")?;
    Ok(())
}

fn validate_direct_advertised_ref_name(name: &[u8]) -> Result<()> {
    let Some(tag) = name.strip_suffix(b"^{}") else {
        return validate_advertised_ref_name(name);
    };
    let tag = gix::refs::FullName::try_from(tag.as_bstr())
        .wrap_err("peeled remote ref has an invalid tag name")?;
    if tag.category() != Some(gix::refs::Category::Tag) {
        bail!("peeled remote ref is not a tag");
    }
    Ok(())
}

/// Splits Git's line-oriented output using this host's native line ending.
///
/// Git for Windows emits CRLF, so exactly one CR immediately before an LF is
/// framing there. On other hosts a terminal CR is data. In particular, a Unix
/// push destination may legally contain that byte, and removing it would make
/// GHerrit observe a different repository than Git will push to.
pub(super) fn git_output_records(output: &[u8]) -> impl Iterator<Item = &[u8]> {
    output.split_inclusive(|byte| *byte == b'\n').map(|record| {
        let Some(record) = record.strip_suffix(b"\n") else {
            return record;
        };
        #[cfg(windows)]
        {
            record.strip_suffix(b"\r").unwrap_or(record)
        }
        #[cfg(not(windows))]
        {
            record
        }
    })
}

#[cfg(test)]
mod tests;
