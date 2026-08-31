//! The exact repository and default branch used by one publication attempt.
//!
//! Git permits a named remote to fetch from one URL and push to one or more
//! other URLs. GHerrit cannot safely observe one repository and then write to
//! another, and one atomic push cannot span several repositories. Resolving a
//! `PushDestination` establishes both the exact Git destination and the GitHub
//! repository identity used by the rest of the attempt.

use std::{
    borrow::Cow,
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
const PROBE_REMOTE_STEM: &str = "gherrit-publication-probe";
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
        // ref store, or ref namespace even when `current_dir` is exact.
        for variable in [
            "GIT_DIR",
            "GIT_COMMON_DIR",
            "GIT_WORK_TREE",
            "GIT_IMPLICIT_WORK_TREE",
            "GIT_NAMESPACE",
            "GIT_CEILING_DIRECTORIES",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        ] {
            command.env_remove(variable);
        }
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
        let resolved =
            ResolvedDestination::from_git_output(configured_remote, format!("{url}\n").as_bytes())
                .unwrap();
        Self {
            repository: RepositoryBinding::new(repository)
                .expect("test repositories use a supported repository context"),
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

        let resolved = ResolvedDestination::from_git_output(configured_remote, &output.stdout)?;

        // A URL-conditioned include which is inactive in the repository's
        // ordinary configuration can become active as soon as GHerrit injects
        // the resolved destination. Use a throwaway, proved-absent remote to
        // activate that complete finite configuration before selecting the
        // remote used for network commands. Settings which the private
        // adapter cannot represent are rejected while this remains a local
        // configuration-only operation.
        let baseline = resolved.inspect_baseline_configuration(&repository)?;
        let probe_remote = select_absent_remote(PROBE_REMOTE_STEM, &baseline);
        let active = resolved.inspect_configuration_with_remote(&repository, &probe_remote)?;
        reject_unsupported_remote_transport_configuration(
            resolved.configured_remote.as_str(),
            &active,
        )?;
        let transport = resolved.inspect_remote_transport_settings(&repository, &probe_remote)?;
        let internal_remote = select_absent_remote(INTERNAL_REMOTE_STEM, &active);

        let destination = Self { repository, resolved, internal_remote, transport };
        destination.inspect_internal_remote_configuration()?;
        destination.ensure_rewrite_fixed_point()?;
        Ok(destination)
    }

    /// Inspects the exact configuration context used by network commands.
    ///
    /// The internal name was absent after the destination probe activated all
    /// URL-conditioned includes. Adding the same URL under the final name
    /// activates the same includes, so only GHerrit's command-scoped URL and
    /// explicitly preserved transport keys can configure this remote. This
    /// final inspection defends that argument directly: every planned key
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
        options: impl IntoIterator<Item = String>,
        ref_patterns: impl IntoIterator<Item = String>,
    ) -> std::result::Result<subprocess::CommandOutput, subprocess::CommandError> {
        let mut command = self.ls_remote(options, ref_patterns);
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
    /// Test-driver runtimes pair an explicit fake API with a local Git remote,
    /// so this is enforced when the GitHub client selects its production
    /// endpoint rather than while resolving the destination.
    pub(super) fn supports_production_github(&self) -> bool {
        is_github_com_destination(&self.resolved.literal)
    }

    pub(super) fn belongs_to(&self, repository: &util::Repo) -> Result<bool> {
        Ok(self.repository == RepositoryBinding::new(repository)?)
    }

    pub(super) fn repo_url_relative(&self) -> String {
        format!("/{}/{}", self.resolved.coordinates.owner, self.resolved.coordinates.repository)
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
    if let Some((scheme, rest)) = destination.split_once("://") {
        if !scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        if rest.starts_with('/') {
            return Some(PathBuf::from(rest));
        }
        let (_, path) = rest.split_once('/')?;
        return Some(Path::new("/").join(path));
    }
    (!is_scp_form(destination)).then(|| PathBuf::from(destination))
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
    fn from_git_output(configured_remote: util::RemoteName, output: &[u8]) -> Result<Self> {
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

        let literal = str::from_utf8(destination).map(str::to_owned).map_err(|_| {
            eyre!(
                "GHerrit remote '{}' has a non-UTF-8 push destination",
                configured_remote.as_str()
            )
        })?;
        if uri_authority_has_userinfo(&literal) {
            bail!(
                "The push destination for GHerrit remote '{}' contains URI user information, which GHerrit does not support; use a Git credential helper or an SCP-style SSH destination instead",
                configured_remote.as_str()
            );
        }
        let coordinates = repository_identity(&literal).ok_or_else(|| {
            eyre!(
                "The push destination for GHerrit remote '{}' does not identify a supported GitHub repository",
                configured_remote.as_str()
            )
        })?;

        let http_redirect_parameters = command_scope_http_redirect_parameters(
            &literal,
            env::var_os(GIT_CONFIG_PARAMETERS_ENV).as_deref(),
        );

        Ok(Self { configured_remote, literal, coordinates, http_redirect_parameters })
    }

    /// Reads configuration which is active before GHerrit adds any remote.
    fn inspect_baseline_configuration(
        &self,
        repository: &RepositoryBinding,
    ) -> Result<Vec<Vec<u8>>> {
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

    /// Reads configuration after the destination has activated URL-conditioned
    /// includes under a throwaway, proved-absent remote name.
    fn inspect_configuration_with_remote(
        &self,
        repository: &RepositoryBinding,
        remote: &str,
    ) -> Result<Vec<Vec<u8>>> {
        let output = self
            .private_remote_command(
                repository,
                remote,
                &RemoteTransportSettings::default(),
                ["config", "--null", "--name-only", "--list"].map(str::to_owned),
            )
            .output()
            .wrap_err_with(|| {
                format!(
                    "Failed to inspect destination-conditioned Git configuration while resolving GHerrit remote '{}'",
                    self.configured_remote.as_str()
                )
            })?;
        decode_config_names(
            &output,
            self.configured_remote.as_str(),
            "destination-conditioned Git configuration",
        )
    }

    /// Reads the effective configured-remote values in the same configuration
    /// context as publication.
    ///
    /// Adding the probe URL can activate `includeIf.hasconfig:remote.*.url`
    /// files. Querying through that probe therefore observes the same proxy
    /// inputs as the final internal remote. `git config --get` deliberately
    /// selects Git's effective last value when a key occurs more than once.
    fn inspect_remote_transport_settings(
        &self,
        repository: &RepositoryBinding,
        probe_remote: &str,
    ) -> Result<RemoteTransportSettings> {
        Ok(RemoteTransportSettings {
            proxy: self.inspect_remote_transport_setting(repository, probe_remote, "proxy")?,
            proxy_auth_method: self.inspect_remote_transport_setting(
                repository,
                probe_remote,
                "proxyAuthMethod",
            )?,
        })
    }

    fn inspect_remote_transport_setting(
        &self,
        repository: &RepositoryBinding,
        probe_remote: &str,
        key: &str,
    ) -> Result<Option<String>> {
        let key = format!("remote.{}.{key}", self.configured_remote.as_str());
        let output = self
            .private_remote_command(
                repository,
                probe_remote,
                &RemoteTransportSettings::default(),
                ["config".to_owned(), "--null".to_owned(), "--get".to_owned(), key],
            )
            .output()
            .wrap_err_with(|| {
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

fn uri_authority_has_userinfo(destination: &str) -> bool {
    destination
        .split_once("://")
        .map(|(_, rest)| rest.split_once('/').map_or(rest, |(authority, _)| authority))
        .is_some_and(|authority| authority.contains('@'))
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

/// Parses the GitHub owner and repository from deliberately supported Git URL
/// forms. Query strings, fragments, controls, and ambiguous suffixes are
/// rejected rather than silently becoming part of repository identity.
fn repository_identity(destination: &str) -> Option<RepositoryCoordinates> {
    if destination.is_empty()
        || destination.chars().any(char::is_control)
        || destination.contains(['?', '#'])
    {
        return None;
    }

    let (path, is_local_path) = if let Some((scheme, rest)) = destination.split_once("://") {
        if !valid_scheme(scheme) {
            return None;
        }
        if scheme.eq_ignore_ascii_case("file") {
            let path = match rest.strip_prefix('/') {
                Some(path) => path,
                None => {
                    let (authority, path) = rest.split_once('/')?;
                    if authority.is_empty() || authority.chars().any(char::is_whitespace) {
                        return None;
                    }
                    path
                }
            };
            if path.is_empty() {
                return None;
            }
            (path, false)
        } else {
            let (authority, path) = rest.split_once('/')?;
            if authority.is_empty()
                || authority.chars().any(char::is_whitespace)
                || path.split('/').count() != 2
            {
                return None;
            }
            (path, false)
        }
    } else if is_scp_form(destination) {
        let (authority, path) = destination.split_once(':')?;
        if authority.is_empty()
            || authority.chars().any(char::is_whitespace)
            || path.split('/').count() != 2
        {
            return None;
        }
        (path, false)
    } else {
        (destination, true)
    };

    // URL and SCP paths always use `/`. A scheme-less local path uses the
    // host's path grammar: `\\` is a separator on Windows but an ordinary,
    // potentially identity-changing filename byte on Unix.
    let normalized = if cfg!(windows) && is_local_path {
        Cow::Owned(path.replace('\\', "/"))
    } else {
        Cow::Borrowed(path)
    };
    if normalized.ends_with('/') {
        return None;
    }
    let components =
        normalized.split('/').filter(|component| !component.is_empty()).collect::<Vec<_>>();
    let [.., owner, repository] = components.as_slice() else {
        return None;
    };
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    RepositoryCoordinates::new((*owner).to_owned(), repository.to_owned())
}

/// Recognizes the URI and SCP authorities which name `github.com` itself.
/// Local paths, aliases, ports, subdomains, and unrelated forges are not the
/// repository served by GHerrit's fixed production GitHub API endpoint.
fn is_github_com_destination(destination: &str) -> bool {
    if let Some((scheme, rest)) = destination.split_once("://") {
        if !["http", "https", "git", "ssh"].contains(&scheme) {
            return false;
        }
        let Some((authority, _)) = rest.split_once('/') else {
            return false;
        };
        return authority.eq_ignore_ascii_case("github.com");
    }

    if is_scp_form(destination) {
        let Some((authority, _)) = destination.split_once(':') else {
            return false;
        };
        return authority.eq_ignore_ascii_case("git@github.com")
            || authority.eq_ignore_ascii_case("github.com");
    }

    false
}

fn valid_scheme(scheme: &str) -> bool {
    !scheme.is_empty()
        && scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic()
                || index != 0 && matches!(byte, b'0'..=b'9' | b'+' | b'.' | b'-')
        })
}

fn is_scp_form(destination: &str) -> bool {
    let Some(colon) = destination.find(':') else {
        return false;
    };
    if colon == 1
        && destination.as_bytes()[0].is_ascii_alphabetic()
        && destination.as_bytes().get(2).is_some_and(|byte| matches!(byte, b'/' | b'\\'))
    {
        return false;
    }
    destination.find(['/', '\\']).is_none_or(|slash| colon < slash)
}

fn valid_repository_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
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
mod tests {
    use std::{
        ffi::OsStr,
        fs,
        io::Write as _,
        path::Path,
        process, thread,
        time::{Duration, Instant},
    };

    use super::*;

    const OBSERVATION_FIXTURE_MODE: &str = "GHERRIT_INITIAL_OBSERVATION_FIXTURE_MODE";
    const OBSERVATION_FIXTURE_TEST: &str =
        "pre_push::destination::tests::initial_observation_process_fixture";
    const TRANSPORT_FIXTURE_MODE: &str = "GHERRIT_REMOTE_TRANSPORT_FIXTURE_MODE";
    const TRANSPORT_FIXTURE_TEST: &str =
        "pre_push::destination::tests::remote_transport_settings_process_fixture";

    fn remote() -> util::RemoteName {
        util::RemoteName::from_config(b"origin").unwrap()
    }

    fn resolved(output: &[u8]) -> ResolvedDestination {
        ResolvedDestination::from_git_output(remote(), output).unwrap()
    }

    fn destination() -> PushDestination {
        PushDestination::for_test()
    }

    fn arguments(command: &Command) -> Vec<&OsStr> {
        command.get_args().collect()
    }

    fn observation_fixture(mode: &str) -> Command {
        let mut command = Command::new(env::current_exe().unwrap());
        command.env_clear();
        #[cfg(windows)]
        if let Some(system_root) = env::var_os("SystemRoot") {
            command.env("SystemRoot", &system_root).env("WINDIR", system_root);
        }
        command
            .args(["--exact", OBSERVATION_FIXTURE_TEST, "--nocapture"])
            .env(OBSERVATION_FIXTURE_MODE, mode);
        command
    }

    fn transport_fixture(context: &testutil::TestContext, mode: &str) -> process::Output {
        let global = context.dir.path().join("empty-global.config");
        fs::write(&global, "").unwrap();
        Command::new(env::current_exe().unwrap())
            .args(["--exact", TRANSPORT_FIXTURE_TEST, "--nocapture"])
            .current_dir(&context.repo_path)
            .env(TRANSPORT_FIXTURE_MODE, mode)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", global)
            .env_remove(GIT_CONFIG_PARAMETERS_ENV)
            .env_remove("GIT_CONFIG_COUNT")
            .output()
            .unwrap()
    }

    #[test]
    fn initial_observation_process_fixture() {
        let Ok(mode) = env::var(OBSERVATION_FIXTURE_MODE) else {
            return;
        };
        match mode.as_str() {
            "hang" => thread::sleep(Duration::from_secs(30)),
            "overflow" => {
                std::io::stdout().write_all(&[b'x'; 256]).unwrap();
                std::io::stdout().flush().unwrap();
                thread::sleep(Duration::from_secs(30));
            }
            mode => panic!("unknown initial-observation fixture mode {mode}"),
        }
        process::exit(0);
    }

    #[test]
    fn remote_transport_settings_process_fixture() {
        let Ok(mode) = env::var(TRANSPORT_FIXTURE_MODE) else {
            return;
        };
        let destination = resolved(b"https://github.com/owner/repo.git\n");
        let repository = util::Repo::open(".").unwrap();
        let repository = RepositoryBinding::new(&repository).unwrap();
        let settings = destination
            .inspect_remote_transport_settings(&repository, PROBE_REMOTE_STEM)
            .unwrap_or_else(|error| {
                if mode == "control" {
                    let rendered = error.to_string();
                    assert!(rendered.contains("transport setting containing control characters"));
                    assert!(!rendered.contains("opaque-control-secret"));
                    process::exit(0);
                }
                panic!("transport inspection failed: {error:?}");
            });
        match mode.as_str() {
            "last-and-empty" => {
                assert!(settings.proxy.as_deref() == Some(""), "proxy selection mismatch");
                assert!(
                    settings.proxy_auth_method.as_deref() == Some("digest"),
                    "proxy authentication selection mismatch"
                );
            }
            "auth-only" => {
                assert!(settings.proxy.is_none(), "an absent proxy became present");
                assert!(
                    settings.proxy_auth_method.as_deref() == Some("digest"),
                    "proxy authentication selection mismatch"
                );
            }
            "control" => panic!("a control-bearing transport setting was accepted"),
            mode => panic!("unknown transport fixture mode {mode}"),
        }
    }

    #[test]
    fn effective_remote_transport_settings_follow_git_value_semantics() {
        let context =
            testutil::TestContextBuilder::new("unused").with_remote().with_initial_commit().build();
        for (key, value) in [
            ("remote.origin.proxy", "first-proxy"),
            ("remote.origin.proxy", ""),
            ("remote.origin.proxyAuthMethod", "basic"),
            ("remote.origin.proxyAuthMethod", "digest"),
        ] {
            context.git_cmd().args(["config", "--add", key, value]).assert().success();
        }

        let output = transport_fixture(&context, "last-and-empty");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

        context.git_cmd().args(["config", "--unset-all", "remote.origin.proxy"]).assert().success();
        let output = transport_fixture(&context, "auth-only");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

        context
            .git_cmd()
            .args(["config", "--replace-all", "remote.origin.proxy", "opaque-control-secret\u{85}"])
            .assert()
            .success();
        let output = transport_fixture(&context, "control");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initial_observation_hang_is_bounded_off_the_async_runtime() {
        let started = Instant::now();
        let observation = observe_initial_command(
            observation_fixture("hang"),
            "origin",
            None,
            Duration::from_millis(50),
            1024,
        );
        let peer = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            started.elapsed()
        };
        let (error, peer_elapsed) = tokio::join!(observation, peer);
        let error = error.unwrap_err();

        assert!(format!("{error:?}").contains("remote Git command timed out"));
        assert!(peer_elapsed < Duration::from_secs(1));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initial_observation_excess_output_enters_bounded_cleanup() {
        let started = Instant::now();
        let error = observe_initial_command(
            observation_fixture("overflow"),
            "origin",
            None,
            Duration::from_secs(2),
            32,
        )
        .await
        .unwrap_err();

        assert!(format!("{error:?}").contains("stdout exceeded the 32-byte limit"));
        assert!(!format!("{error:?}").contains(&"x".repeat(32)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    fn environment<'command>(
        command: &'command Command,
        name: &str,
    ) -> Option<Option<&'command OsStr>> {
        command
            .get_envs()
            .find(|(candidate, _)| *candidate == OsStr::new(name))
            .map(|(_, value)| value)
    }

    fn successful_git(current_dir: &Path, arguments: &[&str]) {
        let mut command = Command::new("git");
        for variable in [
            "GIT_DIR",
            "GIT_COMMON_DIR",
            "GIT_WORK_TREE",
            "GIT_IMPLICIT_WORK_TREE",
            "GIT_NAMESPACE",
            "GIT_CEILING_DIRECTORIES",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        ] {
            command.env_remove(variable);
        }
        let output = command.current_dir(current_dir).args(arguments).output().unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn assert_repository_binding(
        repository: &util::Repo,
        expected_current: &Path,
        expected_work_tree: Option<&Path>,
        hostile_repository: &Path,
    ) {
        let binding = RepositoryBinding::new(repository).unwrap();
        let mut command = Command::new("git");
        command
            .env("GIT_DIR", hostile_repository)
            .env("GIT_COMMON_DIR", hostile_repository)
            .env("GIT_WORK_TREE", hostile_repository)
            .env("GIT_IMPLICIT_WORK_TREE", "false")
            .env("GIT_NAMESPACE", "hostile")
            .env("GIT_CEILING_DIRECTORIES", hostile_repository)
            .env("GIT_DISCOVERY_ACROSS_FILESYSTEM", "true");

        binding.bind(&mut command);

        assert_eq!(command.get_current_dir(), Some(expected_current));
        assert_eq!(environment(&command, "GIT_DIR"), Some(Some(binding.git_dir.as_os_str())));
        assert_eq!(
            fs::canonicalize(&binding.git_dir).unwrap(),
            Path::new(binding.git_dir_identity.as_os_str())
        );
        assert_eq!(
            environment(&command, "GIT_COMMON_DIR"),
            Some(Some(binding.common_dir.as_os_str()))
        );
        assert_eq!(
            environment(&command, "GIT_WORK_TREE"),
            Some(expected_work_tree.map(Path::as_os_str))
        );
        for variable in [
            "GIT_IMPLICIT_WORK_TREE",
            "GIT_NAMESPACE",
            "GIT_CEILING_DIRECTORIES",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        ] {
            assert_eq!(environment(&command, variable), Some(None), "variable={variable}");
        }

        command.args(["rev-parse", "--absolute-git-dir", "--git-common-dir"]);
        if expected_work_tree.is_some() {
            command.arg("--show-toplevel");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "bound git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let paths = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(PathBuf::from)
            .map(|path| fs::canonicalize(path).unwrap())
            .collect::<Vec<_>>();
        let expected = [
            Some(Path::new(binding.git_dir_identity.as_os_str())),
            Some(binding.common_dir.as_path()),
            expected_work_tree,
        ]
        .into_iter()
        .flatten()
        .map(|path| fs::canonicalize(path).unwrap())
        .collect::<Vec<_>>();
        assert_eq!(paths, expected);
    }

    #[test]
    fn repository_binding_models_normal_linked_and_bare_repositories() {
        let root = tempfile::tempdir().unwrap();
        let ordinary = root.path().join("ordinary");
        successful_git(root.path(), &["init", ordinary.to_str().unwrap()]);
        successful_git(
            &ordinary,
            &[
                "-c",
                "user.name=GHerrit Test",
                "-c",
                "user.email=gherrit@example.com",
                "commit",
                "--allow-empty",
                "--no-gpg-sign",
                "--no-verify",
                "-m",
                "root",
            ],
        );

        let linked = root.path().join("linked");
        successful_git(
            &ordinary,
            &["worktree", "add", "--detach", linked.to_str().unwrap(), "HEAD"],
        );

        let bare = root.path().join("bare.git");
        let hostile = root.path().join("hostile.git");
        gix::init_bare(&bare).unwrap();
        gix::init_bare(&hostile).unwrap();

        let ordinary_repository = util::Repo::open(ordinary.to_str().unwrap()).unwrap();
        assert_repository_binding(&ordinary_repository, &ordinary, Some(&ordinary), &hostile);

        let linked_repository = util::Repo::open(linked.to_str().unwrap()).unwrap();
        assert_repository_binding(&linked_repository, &linked, Some(&linked), &hostile);

        let bare_repository = util::Repo::open(bare.to_str().unwrap()).unwrap();
        assert_repository_binding(&bare_repository, &bare, None, &hostile);
    }

    #[test]
    fn every_destination_command_retains_its_repository_binding() {
        let destination = destination();
        for command in [
            destination.ls_remote(["--quiet".to_owned()], ["HEAD".to_owned()]),
            destination.exact_object_fetch(ExactObjectFetchMode::Negotiated),
            destination.push(["--porcelain".to_owned()], ["HEAD:refs/heads/G".to_owned()]),
        ] {
            assert_eq!(
                command.get_current_dir(),
                Some(destination.repository.current_dir.as_path())
            );
            assert_eq!(
                environment(&command, "GIT_DIR"),
                Some(Some(destination.repository.git_dir.as_os_str()))
            );
            assert_eq!(
                environment(&command, "GIT_COMMON_DIR"),
                Some(Some(destination.repository.common_dir.as_os_str()))
            );
            assert_eq!(environment(&command, "GIT_NAMESPACE"), Some(None));
        }
    }

    #[test]
    fn recognizes_every_private_transport_diagnostic_family() {
        for name in [
            b"GIT_TRACE".as_slice(),
            b"git_trace_curl",
            b"GiT_TrAcE2_Event",
            b"GIT_TRACE_FUTURE_FAMILY",
            b"GIT_CURL_VERBOSE",
            b"git_curl_verbose",
        ] {
            assert!(is_git_transport_diagnostic(name), "name: {name:?}");
        }
        for name in [b"GHERRIT_TRACE".as_slice(), b"GIT_CURL_VERBOSE_EXTRA", b"GIT_FLUSH", b"TRACE"]
        {
            assert!(!is_git_transport_diagnostic(name), "name: {name:?}");
        }
    }

    #[test]
    fn transport_diagnostics_remove_future_families_and_disable_trace2_config_targets() {
        let mut command = Command::new("git");
        clear_git_transport_diagnostics_from(
            &mut command,
            [
                "GIT_TRACE_FUTURE_FAMILY",
                "GIT_CURL_VERBOSE",
                "GIT_TRACE2",
                "GIT_TRACE2_PERF",
                "GIT_TRACE2_EVENT",
            ]
            .map(std::ffi::OsString::from),
        );

        for variable in ["GIT_TRACE_FUTURE_FAMILY", "GIT_CURL_VERBOSE"] {
            assert_eq!(
                command
                    .get_envs()
                    .find(|(name, _)| *name == OsStr::new(variable))
                    .map(|(_, value)| value),
                Some(None)
            );
        }
        for variable in ["GIT_TRACE2", "GIT_TRACE2_PERF", "GIT_TRACE2_EVENT"] {
            assert_eq!(
                command
                    .get_envs()
                    .find(|(name, _)| *name == OsStr::new(variable))
                    .and_then(|(_, value)| value),
                Some(OsStr::new("0"))
            );
        }
    }

    #[test]
    fn configured_trace2_targets_do_not_observe_a_destination_command() {
        let context =
            testutil::TestContextBuilder::new("unused").with_remote().with_initial_commit().build();
        let literal = String::from_utf8(
            context
                .git_cmd()
                .args(["remote", "get-url", "origin"])
                .assert()
                .success()
                .get_output()
                .stdout
                .clone(),
        )
        .unwrap();
        let literal = literal.trim();
        let repository = util::Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        let destination = PushDestination::for_test_url_in(&repository, literal);

        let system = context.dir.path().join("trace2-system.config");
        let global = context.dir.path().join("trace2-global.config");
        let traces = [
            context.dir.path().join("trace2-event.json"),
            context.dir.path().join("trace2-normal.log"),
            context.dir.path().join("trace2-perf.log"),
        ];
        fs::write(&system, "").unwrap();
        context
            .git_cmd()
            .args(["config", "--file"])
            .arg(&system)
            .args(["--add", "trace2.eventTarget"])
            .arg(&traces[0])
            .assert()
            .success();
        fs::write(&global, "").unwrap();
        for (key, trace) in [("trace2.normalTarget", &traces[1]), ("trace2.perfTarget", &traces[2])]
        {
            context
                .git_cmd()
                .args(["config", "--file"])
                .arg(&global)
                .args(["--add", key])
                .arg(trace)
                .assert()
                .success();
        }

        let mut probe = context.git_cmd();
        probe
            .env("GIT_CONFIG_NOSYSTEM", "0")
            .env("GIT_CONFIG_SYSTEM", &system)
            .env("GIT_CONFIG_GLOBAL", &global)
            .args(["rev-parse", "--git-dir"])
            .assert()
            .success();
        traces.iter().for_each(|trace| fs::remove_file(trace).unwrap());

        let mut command = destination.ls_remote(["--symref".to_owned()], ["HEAD".to_owned()]);
        command
            .current_dir(&context.repo_path)
            .env("GIT_CONFIG_NOSYSTEM", "0")
            .env("GIT_CONFIG_SYSTEM", &system)
            .env("GIT_CONFIG_GLOBAL", &global);
        let output = command.output().unwrap();

        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(traces.iter().all(|trace| !trace.exists()));
    }

    #[test]
    fn every_destination_bearing_git_command_disables_redirects() {
        let destination = destination();
        let literal = "https://github.com/owner/repo.git";
        let redirect_parameter = format!(
            "{}={}",
            quote_git_config_parameter(&format!("http.{literal}.followRedirects")),
            quote_git_config_parameter("false")
        );
        let ls_remote = destination.ls_remote(["--symref".to_string()], ["HEAD".to_string()]);
        assert_eq!(
            arguments(&ls_remote),
            [
                "--no-replace-objects",
                "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "--config-env=remote.gherrit-publication.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "-c",
                "http.followRedirects=false",
                "ls-remote",
                "--symref",
                "--",
                "gherrit-publication",
                "HEAD",
            ]
            .map(OsStr::new)
        );

        let push = destination.push(["--atomic".to_string()], ["HEAD:refs/heads/Gone".to_string()]);
        assert_eq!(
            arguments(&push),
            [
                "--no-replace-objects",
                "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "--config-env=remote.gherrit-publication.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "-c",
                "http.followRedirects=false",
                "-c",
                "push.followTags=false",
                "-c",
                "push.recurseSubmodules=no",
                "-c",
                "push.pushOption=",
                "push",
                "--atomic",
                "--",
                "gherrit-publication",
                "HEAD:refs/heads/Gone",
            ]
            .map(OsStr::new)
        );
        assert!(arguments(&push).iter().all(|argument| {
            let argument = argument.to_string_lossy();
            !argument.ends_with(".url=") && !argument.ends_with(".pushurl=")
        }));
        assert!(arguments(&push).iter().all(|argument| *argument != OsStr::new(literal)));
        assert_eq!(
            push.get_envs()
                .find(|(name, _)| *name == OsStr::new(DESTINATION_ENV))
                .and_then(|(_, value)| value),
            Some(OsStr::new(literal))
        );
        let parameters = push
            .get_envs()
            .find(|(name, _)| *name == OsStr::new(GIT_CONFIG_PARAMETERS_ENV))
            .and_then(|(_, value)| value)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(parameters.ends_with(&redirect_parameter));
        assert_eq!(
            push.get_envs()
                .find(|(name, _)| *name == OsStr::new(INTERNAL_PRE_PUSH_REMOTE_ENV))
                .and_then(|(_, value)| value),
            Some(OsStr::new("gherrit-publication"))
        );
        assert_eq!(
            environment(&push, INTERNAL_PRE_PUSH_GIT_DIR_ENV),
            Some(Some(destination.repository.git_dir_identity.as_os_str()))
        );

        let probe = destination.resolved.private_remote_command(
            &destination.repository,
            PROBE_REMOTE_STEM,
            &RemoteTransportSettings::default(),
            ["config", "--null", "--name-only", "--list"].map(str::to_owned),
        );
        assert_eq!(
            arguments(&probe),
            [
                "--no-replace-objects",
                "--config-env=remote.gherrit-publication-probe.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "--config-env=remote.gherrit-publication-probe.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "config",
                "--null",
                "--name-only",
                "--list",
            ]
            .map(OsStr::new)
        );
        assert!(arguments(&probe).iter().all(|argument| !argument.is_empty()));
        assert!(
            probe
                .get_envs()
                .find(|(name, _)| *name == OsStr::new(GIT_CONFIG_PARAMETERS_ENV))
                .and_then(|(_, value)| value)
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with(&redirect_parameter)
        );
    }

    #[test]
    fn remote_transport_settings_are_private_command_scoped_state() {
        let mut destination = destination();
        destination.transport = RemoteTransportSettings {
            proxy: Some("opaque-proxy-secret".to_owned()),
            proxy_auth_method: Some("opaque-auth-secret".to_owned()),
        };

        for command in [
            destination.ls_remote(std::iter::empty(), std::iter::empty()),
            destination.exact_object_fetch(ExactObjectFetchMode::Negotiated),
            destination.push(std::iter::empty(), std::iter::empty()),
        ] {
            let arguments = arguments(&command);
            assert!(arguments.contains(&OsStr::new(
                "--config-env=remote.gherrit-publication.proxy=GHERRIT_PRIVATE_REMOTE_PROXY"
            )));
            assert!(arguments.contains(&OsStr::new(
                "--config-env=remote.gherrit-publication.proxyAuthMethod=GHERRIT_PRIVATE_REMOTE_PROXY_AUTH_METHOD"
            )));
            for private in ["opaque-proxy-secret", "opaque-auth-secret"] {
                assert!(arguments.iter().all(|argument| *argument != OsStr::new(private)));
            }
            for (name, value) in
                [(PROXY_ENV, "opaque-proxy-secret"), (PROXY_AUTH_METHOD_ENV, "opaque-auth-secret")]
            {
                assert_eq!(
                    command
                        .get_envs()
                        .find(|(actual, _)| *actual == OsStr::new(name))
                        .and_then(|(_, value)| value),
                    Some(OsStr::new(value))
                );
            }
        }

        destination.transport =
            RemoteTransportSettings { proxy: Some(String::new()), proxy_auth_method: None };
        let command = destination.ls_remote(std::iter::empty(), std::iter::empty());
        assert_eq!(
            command
                .get_envs()
                .find(|(name, _)| *name == OsStr::new(PROXY_ENV))
                .and_then(|(_, value)| value),
            Some(OsStr::new(""))
        );
        assert_eq!(
            command
                .get_envs()
                .find(|(name, _)| *name == OsStr::new(PROXY_AUTH_METHOD_ENV))
                .map(|(_, value)| value),
            Some(None)
        );

        let probe = destination.resolved.private_remote_command(
            &destination.repository,
            PROBE_REMOTE_STEM,
            &RemoteTransportSettings::default(),
            std::iter::empty(),
        );
        assert!(arguments(&probe).iter().all(|argument| {
            !argument.to_string_lossy().contains("proxy")
                && !argument.to_string_lossy().contains("proxyAuthMethod")
        }));
    }

    #[test]
    fn exact_redirect_parameter_preserves_command_scope_and_wins() {
        let literal = "https://redirect.invalid/private/repo.git";
        let key = format!("http.{literal}.followRedirects");
        let inherited = format!(
            "{}={} {}={}",
            quote_git_config_parameter("credential.helper"),
            quote_git_config_parameter("sentinel-helper"),
            quote_git_config_parameter(&key),
            quote_git_config_parameter("true")
        );
        let parameters =
            command_scope_http_redirect_parameters(literal, Some(OsStr::new(&inherited))).unwrap();

        assert_eq!(quote_git_config_parameter("a'b"), "'a'\\''b'",);
        assert!(parameters.to_str().unwrap().starts_with(&inherited));

        let command = |arguments: &[&str]| {
            let mut command = Command::new("git");
            command
                .args(arguments)
                .env(GIT_CONFIG_PARAMETERS_ENV, &parameters)
                // The numbered environment source is read before
                // `GIT_CONFIG_PARAMETERS`. Its hostile exact value must not
                // supersede the appended value.
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", &key)
                .env("GIT_CONFIG_VALUE_0", "true");
            command.output().unwrap()
        };

        let redirect =
            command(&["config", "--bool", "--get-urlmatch", "http.followRedirects", literal]);
        assert!(redirect.status.success());
        assert_eq!(redirect.stdout, b"false\n");

        let helper = command(&["config", "--get", "credential.helper"]);
        assert!(helper.status.success());
        assert_eq!(helper.stdout, b"sentinel-helper\n");
    }

    #[cfg(unix)]
    #[test]
    fn exact_redirect_parameter_preserves_non_utf8_command_scope_bytes() {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

        let inherited = OsString::from_vec(b"'credential.helper'='helper-\xff'".to_vec());
        let parameters = command_scope_http_redirect_parameters(
            "https://github.com/owner/repo.git",
            Some(&inherited),
        )
        .unwrap();
        let bytes = parameters.as_bytes();

        assert_eq!(&bytes[..inherited.len()], inherited.as_bytes());
        assert_eq!(bytes[inherited.len()], b' ');
        assert!(bytes.ends_with(b"='false'"));
    }

    #[test]
    fn initial_observation_adds_only_the_checked_public_branch() {
        let destination = destination();
        let branch = PublicBranchName::new("release-candidate".to_owned()).unwrap();
        let command = destination.initial_observation_command(Some(&branch));
        let arguments = arguments(&command);

        assert_eq!(
            &arguments[arguments.len() - 2..],
            [OsStr::new("HEAD"), OsStr::new("refs/heads/release-candidate")]
        );
    }

    #[test]
    fn exact_object_fetch_has_one_fixed_source_only_stdin_grammar() {
        let destination = destination();
        for (mode, mode_argument) in [
            (ExactObjectFetchMode::Negotiated, None),
            (ExactObjectFetchMode::Refetch, Some("--refetch")),
        ] {
            let command = destination.exact_object_fetch(mode);
            let expected = [
                "--no-replace-objects",
                "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "--config-env=remote.gherrit-publication.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "-c",
                "http.followRedirects=false",
                "-c",
                "fetch.bundleURI=",
                "fetch",
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
            ]
            .into_iter()
            .chain(mode_argument)
            .chain(["--stdin", "--", "gherrit-publication"])
            .map(OsStr::new)
            .collect::<Vec<_>>();
            assert_eq!(arguments(&command), expected);
            assert_eq!(arguments(&command).last(), Some(&OsStr::new("gherrit-publication")));
            assert!(arguments(&command).iter().all(|argument| {
                let argument = argument.to_string_lossy();
                !argument.contains("refs/") && !argument.contains("https://github.com")
            }));
            assert_eq!(
                command
                    .get_envs()
                    .find(|(name, _)| *name == OsStr::new(DESTINATION_ENV))
                    .and_then(|(_, value)| value),
                Some(OsStr::new("https://github.com/owner/repo.git"))
            );
            for variable in ["GIT_TRACE", "GIT_CURL_VERBOSE"] {
                assert_eq!(
                    command
                        .get_envs()
                        .find(|(name, _)| *name == OsStr::new(variable))
                        .and_then(|(_, value)| value),
                    None
                );
            }
            for variable in ["GIT_TRACE2", "GIT_TRACE2_PERF", "GIT_TRACE2_EVENT"] {
                assert_eq!(
                    command
                        .get_envs()
                        .find(|(name, _)| *name == OsStr::new(variable))
                        .and_then(|(_, value)| value),
                    Some(OsStr::new("0"))
                );
            }
        }
    }

    #[test]
    fn remote_selection_uses_complete_case_insensitive_configuration_keys() {
        let configuration = [
            b"remote.gherrit-publication.url".to_vec(),
            b"ReMoTe.GhErRiT-PuBlIcAtIoN-1.ReCeIvEpAcK".to_vec(),
            b"remote.gherrit-publication-extra.url".to_vec(),
            b"remote.gherrit-publication-10.pushurl".to_vec(),
        ];

        assert_eq!(
            select_absent_remote(INTERNAL_REMOTE_STEM, &configuration),
            "gherrit-publication-2"
        );
        assert_eq!(
            select_absent_remote(PROBE_REMOTE_STEM, &configuration),
            "gherrit-publication-probe"
        );

        let probe_collisions = [
            b"remote.gherrit-publication-probe.url".to_vec(),
            b"REMOTE.GHERRIT-PUBLICATION-PROBE-1.pushURL".to_vec(),
        ];
        assert_eq!(
            select_absent_remote(PROBE_REMOTE_STEM, &probe_collisions),
            "gherrit-publication-probe-2"
        );
    }

    #[test]
    fn configured_remote_transport_policy_is_explicit_and_case_correct() {
        let allowed = [
            b"remote.origin.proxy".to_vec(),
            b"REMOTE.origin.PROXYAUTHMETHOD".to_vec(),
            b"remote.origin.fetch".to_vec(),
            b"remote.origin.push".to_vec(),
            b"remote.origin.unknownFutureKey".to_vec(),
            b"remote.Origin.receivepack".to_vec(),
            b"remote.origin-extra.vcs".to_vec(),
        ];
        assert!(reject_unsupported_remote_transport_configuration("origin", &allowed).is_ok());

        for (key, expected) in [
            ("remote.origin.uploadpack", "uploadpack"),
            ("REMOTE.origin.RECEIVEPACK", "receivepack"),
            ("remote.origin.VcS", "vcs"),
            ("remote.origin.serveroption", "serverOption"),
        ] {
            let error = reject_unsupported_remote_transport_configuration(
                "origin",
                &[key.as_bytes().to_vec()],
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(expected), "error: {error}");
            assert!(error.contains("GHerrit remote 'origin'"));
        }

        let all = [
            b"remote.origin.serverOption".to_vec(),
            b"remote.origin.receivepack".to_vec(),
            b"remote.origin.serveroption".to_vec(),
        ];
        let error = reject_unsupported_remote_transport_configuration("origin", &all)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "GHerrit remote 'origin' configures unsupported transport settings: receivepack, serverOption"
        );
    }

    #[test]
    fn config_record_framing_distinguishes_no_values_from_empty_values() {
        assert_eq!(nul_terminated_records(b""), Some(Vec::new()));
        assert_eq!(nul_terminated_records(b"value\0"), Some(vec![b"value".to_vec()]));
        assert_eq!(nul_terminated_records(b"\0"), Some(vec![Vec::new()]));
        assert_eq!(
            nul_terminated_records(b"one\0\0three\0"),
            Some(vec![b"one".to_vec(), Vec::new(), b"three".to_vec()])
        );
        assert_eq!(nul_terminated_records(b"unterminated"), None);
    }

    #[test]
    fn final_remote_validation_accepts_only_the_planned_transport() {
        let remote = INTERNAL_REMOTE_STEM;
        let destination = b"private/path/owner/repo.git";
        let base_names = vec![
            b"remote.gherrit-publication.url".to_vec(),
            b"remote.gherrit-publication.pushurl".to_vec(),
            b"remote.unrelated.receivepack".to_vec(),
        ];
        let valid_urls = vec![destination.to_vec()];
        let validate = |names: &[Vec<u8>],
                        urls: &[Vec<u8>],
                        pushurls: &[Vec<u8>],
                        proxies: &[Vec<u8>],
                        auth_methods: &[Vec<u8>],
                        transport: &RemoteTransportSettings| {
            validate_internal_remote_configuration(
                remote,
                names,
                ObservedRemoteValues { urls, pushurls, proxies, proxy_auth_methods: auth_methods },
                destination,
                transport,
            )
        };

        assert!(
            validate(
                &base_names,
                &valid_urls,
                &valid_urls,
                &[],
                &[],
                &RemoteTransportSettings::default(),
            )
            .is_ok()
        );

        for transport in [
            RemoteTransportSettings { proxy: Some(String::new()), proxy_auth_method: None },
            RemoteTransportSettings { proxy: None, proxy_auth_method: Some("basic".to_owned()) },
            RemoteTransportSettings {
                proxy: Some("proxy-value".to_owned()),
                proxy_auth_method: Some("digest".to_owned()),
            },
        ] {
            let mut names = base_names.clone();
            let proxies = transport
                .proxy
                .as_ref()
                .map_or_else(Vec::new, |value| vec![value.as_bytes().to_vec()]);
            let auth_methods = transport
                .proxy_auth_method
                .as_ref()
                .map_or_else(Vec::new, |value| vec![value.as_bytes().to_vec()]);
            if transport.proxy.is_some() {
                names.push(b"remote.gherrit-publication.proxy".to_vec());
            }
            if transport.proxy_auth_method.is_some() {
                names.push(b"REMOTE.GHERRIT-PUBLICATION.PROXYAUTHMETHOD".to_vec());
            }
            assert!(
                validate(&names, &valid_urls, &valid_urls, &proxies, &auth_methods, &transport,)
                    .is_ok(),
                "transport was rejected"
            );
        }

        for (names, urls, pushurls, proxies, auth_methods, transport) in [
            (
                vec![
                    b"remote.gherrit-publication.url".to_vec(),
                    b"remote.gherrit-publication.url".to_vec(),
                    b"remote.gherrit-publication.pushurl".to_vec(),
                ],
                vec![destination.to_vec(), destination.to_vec()],
                valid_urls.clone(),
                Vec::new(),
                Vec::new(),
                RemoteTransportSettings::default(),
            ),
            (
                vec![
                    b"remote.gherrit-publication.url".to_vec(),
                    b"remote.gherrit-publication.pushurl".to_vec(),
                    b"remote.gherrit-publication.pushURL".to_vec(),
                ],
                valid_urls.clone(),
                vec![destination.to_vec(), destination.to_vec()],
                Vec::new(),
                Vec::new(),
                RemoteTransportSettings::default(),
            ),
            (
                base_names.clone(),
                vec![b"different".to_vec()],
                valid_urls.clone(),
                Vec::new(),
                Vec::new(),
                RemoteTransportSettings::default(),
            ),
            (
                base_names.clone(),
                valid_urls.clone(),
                vec![b"different".to_vec()],
                Vec::new(),
                Vec::new(),
                RemoteTransportSettings::default(),
            ),
            (
                vec![
                    b"remote.gherrit-publication.url".to_vec(),
                    b"remote.gherrit-publication.pushurl".to_vec(),
                    b"remote.gherrit-publication.receivepack".to_vec(),
                ],
                valid_urls.clone(),
                valid_urls.clone(),
                Vec::new(),
                Vec::new(),
                RemoteTransportSettings::default(),
            ),
            (
                {
                    let mut names = base_names.clone();
                    names.push(b"remote.gherrit-publication.proxy".to_vec());
                    names
                },
                valid_urls.clone(),
                valid_urls.clone(),
                vec![b"unplanned".to_vec()],
                Vec::new(),
                RemoteTransportSettings::default(),
            ),
            (
                base_names.clone(),
                valid_urls.clone(),
                valid_urls.clone(),
                Vec::new(),
                Vec::new(),
                RemoteTransportSettings {
                    proxy: Some("missing".to_owned()),
                    proxy_auth_method: None,
                },
            ),
            (
                vec![b"remote.gherrit-publication.url".to_vec()],
                valid_urls.clone(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                RemoteTransportSettings::default(),
            ),
            (
                vec![b"remote.gherrit-publication.pushurl".to_vec()],
                Vec::new(),
                valid_urls.clone(),
                Vec::new(),
                Vec::new(),
                RemoteTransportSettings::default(),
            ),
        ] {
            assert!(
                validate(&names, &urls, &pushurls, &proxies, &auth_methods, &transport,).is_err(),
                "names: {names:?}, urls: {urls:?}, pushurls: {pushurls:?}, proxies: {proxies:?}, auth methods: {auth_methods:?}"
            );
        }
    }

    #[test]
    fn derives_identity_from_supported_push_destinations() {
        for (destination, owner, repository) in [
            ("https://github.com/owner/repo.git", "owner", "repo"),
            ("https://github.com/owner/repo", "owner", "repo"),
            ("git@github.com:owner/repo.git", "owner", "repo"),
            ("git@github.com:owner/repo", "owner", "repo"),
            ("alias:owner/repo.git", "owner", "repo"),
            ("alias:owner/repo", "owner", "repo"),
            ("http://localhost:3000/owner/repo.git", "owner", "repo"),
            ("http://my-gh.com/owner/repo", "owner", "repo"),
            ("file:///tmp/test/owner/repo.git", "owner", "repo"),
            ("FILE:///tmp/owner/repo", "owner", "repo"),
            ("/tmp/test/owner/repo.git", "owner", "repo"),
            ("/tmp/owner/repo", "owner", "repo"),
            ("owner/repo", "owner", "repo"),
            ("https://github.com/user-name/repo", "user-name", "repo"),
            ("https://github.com/user_name/repo", "user_name", "repo"),
            ("https://github.com/user.name/repo.name.git", "user.name", "repo.name"),
        ] {
            let destination = resolved(format!("{destination}\n").as_bytes());

            assert_eq!(destination.coordinates.owner, owner);
            assert_eq!(destination.coordinates.repository, repository);
        }
    }

    #[test]
    fn recognizes_only_the_public_github_forge_for_production() {
        for destination in [
            "https://github.com/owner/repo.git",
            "https://GITHUB.COM/owner/repo",
            "git://github.com/owner/repo.git",
            "ssh://github.com/owner/repo.git",
            "git@github.com:owner/repo.git",
            "github.com:owner/repo",
        ] {
            assert!(is_github_com_destination(destination), "destination={destination:?}");
        }

        for destination in [
            "https://evil.example/owner/repo.git",
            "https://github.com.evil.example/owner/repo.git",
            "https://github.com:443/owner/repo.git",
            "evil://github.com/owner/repo.git",
            "ftp://github.com/owner/repo.git",
            "git+ssh://github.com/owner/repo.git",
            "HTTP://github.com/owner/repo.git",
            "HTTPS://github.com/owner/repo.git",
            "GIT://github.com/owner/repo.git",
            "SSH://github.com/owner/repo.git",
            "git@evil.example:owner/repo.git",
            "alias:owner/repo.git",
            "file:///tmp/owner/repo.git",
            "/tmp/owner/repo.git",
            "owner/repo.git",
        ] {
            assert!(!is_github_com_destination(destination), "destination={destination:?}");
        }
    }

    #[test]
    fn publication_target_retains_repository_and_literal_destination() {
        let repository = util::Repo::open(".").unwrap();
        let https =
            PushDestination::for_test_url_in(&repository, "https://github.com/owner/repo.git");
        let ssh = PushDestination::for_test_url_in(&repository, "git@github.com:owner/repo.git");
        let same =
            PushDestination::for_test_url_in(&repository, "https://github.com/owner/repo.git");

        assert!(https.publication_target() == same.publication_target());
        assert!(https.publication_target() != ssh.publication_target());
    }

    #[cfg(windows)]
    #[test]
    fn windows_accepts_native_paths_and_crlf_output() {
        let destination = resolved(br"C:\tmp\owner\repo.git");
        assert_eq!(destination.coordinates.owner, "owner");
        assert_eq!(destination.coordinates.repository, "repo");

        let destination = resolved(b"https://github.com/owner/repo.git\r\n");
        assert_eq!(destination.coordinates.owner, "owner");
        assert_eq!(destination.coordinates.repository, "repo");

        let oid = "1111111111111111111111111111111111111111";
        assert_eq!(
            parse_default_branch(
                format!("ref: refs/heads/main\tHEAD\r\n{oid}\tHEAD\r\n").as_bytes()
            )
            .unwrap(),
            DefaultBranch::new("main".to_string(), ObjectId::from_hex(oid.as_bytes()).unwrap())
                .unwrap()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_preserves_terminal_cr_and_treats_backslash_as_data() {
        let destination = b"https://github.com/owner/repo.git\r\n";
        let error = ResolvedDestination::from_git_output(remote(), destination)
            .err()
            .expect("a Unix destination's terminal CR must not be discarded");
        assert!(!error.to_string().contains("https://"));

        for destination in [r"C:\tmp\owner\repo.git", r"/tmp/attacker/owner\repo.git"] {
            assert_eq!(repository_identity(destination), None, "destination={destination:?}");
        }
    }

    #[test]
    fn rejects_ambiguous_or_malformed_destinations_without_disclosure() {
        for destination in [
            "",
            "credential-without-a-repository",
            "1https://github.com/owner/repo",
            "https://git hub/owner/repo",
            "https://github.com/owner/repo?token=secret",
            "https://github.com/owner/repo#secret",
            "https://github.com/owner/repo/extra",
            r"https://github.com/owner\attacker/repo",
            "git@github.com:owner/repo/extra",
            r"git@github.com:owner\repo",
            r"file:///tmp/attacker/owner\repo",
            "https://github.com/owner/repo\r",
            "https://github.com/owner/repo\npoison",
            "https://github.com/owner/repo\0suffix",
        ] {
            let error = ResolvedDestination::from_git_output(remote(), destination.as_bytes())
                .err()
                .expect("malformed destination must be rejected");
            if !destination.is_empty() {
                assert!(!error.to_string().contains(destination), "destination: {destination:?}");
            }
        }
        assert!(ResolvedDestination::from_git_output(remote(), b"\xff\n").is_err());
    }

    #[test]
    fn rejects_uri_user_information_without_disclosure() {
        for destination in [
            "https://token-secret@github.com/owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
            "file://user:password-secret@localhost/tmp/owner/repo.git",
        ] {
            let error = ResolvedDestination::from_git_output(
                remote(),
                format!("{destination}\n").as_bytes(),
            )
            .err()
            .expect("URI user information must be rejected");
            assert!(error.to_string().contains("use a Git credential helper or an SCP-style SSH"));
            assert!(!error.to_string().contains(destination));
            assert!(!error.to_string().contains("secret"));
        }

        assert!(
            ResolvedDestination::from_git_output(remote(), b"git@github.com:owner/repo.git\n")
                .is_ok()
        );
    }

    #[test]
    fn rejects_zero_or_multiple_destinations_without_disclosing_them() {
        let error = ResolvedDestination::from_git_output(remote(), b"")
            .err()
            .expect("an empty destination must be rejected");
        assert_eq!(error.to_string(), "GHerrit remote 'origin' has no push destination");

        let secret = "https://user:secret@example.com/owner/repo.git";
        let output = format!("{secret}\nowner/other.git\n");
        let error = ResolvedDestination::from_git_output(remote(), output.as_bytes())
            .err()
            .expect("multiple destinations must be rejected");
        assert_eq!(
            error.to_string(),
            "GHerrit remote 'origin' has 2 push destinations; exactly one is required for atomic publication"
        );
        assert!(!error.to_string().contains(secret));
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn child_diagnostic_is_local_bounded_redacted_and_terminal_safe() {
        let directory = tempfile::tempdir().unwrap();
        let canonical_path = directory.path().join("private-owner/private-repository.git");
        fs::create_dir_all(&canonical_path).unwrap();
        let private =
            directory.path().join("private-owner/../private-owner/private-repository.git");
        let literal = private.to_string_lossy().into_owned();
        let canonical = fs::canonicalize(&canonical_path).unwrap().to_string_lossy().into_owned();
        assert_ne!(literal, canonical);
        let repository = util::Repo::open(".").unwrap();
        let destination = PushDestination {
            repository: RepositoryBinding::new(&repository).unwrap(),
            resolved: resolved(format!("{literal}\n").as_bytes()),
            internal_remote: INTERNAL_REMOTE_STEM.to_owned(),
            transport: RemoteTransportSettings {
                proxy: Some("opaque-proxy-secret".to_owned()),
                proxy_auth_method: Some("opaque-auth-secret".to_owned()),
            },
        };
        let diagnostic = format!(
            "policy denied publication for {literal}\x1b[31m\r\n\
             normalized local destination {canonical}\n\
             proxy opaque-proxy-secret uses opaque-auth-secret\n\
             alternate transport ssh://private-host/private-owner/private-repository.git\n\
             remote: private server text for {canonical}\n\
             error: failed to push some refs to '{canonical}'\n\
             error: atomic push failed for ref refs/heads/Gone\n\
             ! [rejected] Gone -> Gone (atomic push failed)\n\
             fatal: the remote end hung up unexpectedly\n"
        );

        let rendered = destination
            .render_child_diagnostic(diagnostic.as_bytes(), diagnostic.len() as u64)
            .unwrap();

        assert!(rendered.contains("policy denied publication for <private destination>"));
        assert!(rendered.contains("normalized local destination <private destination>"));
        assert!(
            rendered.contains("proxy <private transport setting> uses <private transport setting>")
        );
        assert!(rendered.contains("alternate transport <path or URL redacted>"));
        assert!(rendered.contains("\\u{1b}[31m\\r"));
        for private in [
            &literal,
            &canonical,
            "private-host",
            "private-owner",
            "private-repository",
            "opaque-proxy-secret",
            "opaque-auth-secret",
        ] {
            assert!(!rendered.contains(private), "rendered diagnostic disclosed {private:?}");
        }
        assert!(!rendered.contains("private server text"));
        assert!(!rendered.contains("atomic push failed"));
        assert!(!rendered.contains("[rejected]"));
        assert!(!rendered.contains("remote end hung up"));
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\r'));
    }

    #[test]
    fn truncated_child_diagnostics_discard_the_unclassifiable_first_line() {
        let destination = destination();
        let local = "local policy denied https://github.com/owner/repo.git";

        for (first_line, private_fragment) in [
            ("server-private text whose remote prefix was omitted", "server-private"),
            ("ner/repo.git whose destination prefix was omitted", "ner/repo.git"),
        ] {
            let retained = format!("{first_line}\n{local}\n");
            let rendered = destination
                .render_child_diagnostic(
                    retained.as_bytes(),
                    u64::try_from(retained.len()).unwrap() + 17,
                )
                .unwrap();

            assert_eq!(
                rendered,
                format!(
                    "local policy denied <private destination>\n[{} earlier diagnostic bytes omitted]",
                    17 + first_line.len() + 1
                )
            );
            assert!(!rendered.contains(private_fragment));
        }

        let retained = b"ivate/repository.git";
        let total = u64::try_from(retained.len()).unwrap() + 29;
        assert_eq!(
            destination.render_child_diagnostic(retained, total),
            Some(format!("[{total} earlier diagnostic bytes omitted]"))
        );
    }

    #[test]
    fn empty_or_remote_only_child_diagnostics_are_suppressed() {
        let destination = destination();
        let remote_only = b"remote: server-private text\n";

        assert_eq!(destination.render_child_diagnostic(b"", 0), None);
        assert_eq!(
            destination
                .render_child_diagnostic(remote_only, u64::try_from(remote_only.len()).unwrap()),
            None
        );
    }

    #[test]
    fn parses_exact_symbolic_default_branch_observation() {
        let oid = "1111111111111111111111111111111111111111";
        assert_eq!(
            parse_default_branch(format!("ref: refs/heads/master\tHEAD\n{oid}\tHEAD\n").as_bytes())
                .unwrap(),
            DefaultBranch::new("master".to_string(), ObjectId::from_hex(oid.as_bytes()).unwrap())
                .unwrap()
        );
    }

    #[test]
    fn initial_observation_reports_exact_public_branch_presence_or_absence() {
        let head = "1111111111111111111111111111111111111111";
        let public = "2222222222222222222222222222222222222222";
        let branch = PublicBranchName::new("release-candidate".to_owned()).unwrap();

        for (extra, expected) in [
            (String::new(), RemoteBranchState::Absent),
            (
                format!("{public}\trefs/heads/release-candidate\n"),
                RemoteBranchState::At(ObjectId::from_hex(public.as_bytes()).unwrap()),
            ),
        ] {
            let output = format!("ref: refs/heads/main\tHEAD\n{head}\tHEAD\n{extra}");
            let observation =
                parse_initial_remote_observation(output.as_bytes(), Some(branch.clone())).unwrap();
            let (default, observed) = observation.into_parts();
            assert_eq!(default.name(), "main");
            let (observed_name, observed_state) = observed.unwrap().into_parts();
            assert_eq!(observed_name, branch);
            assert_eq!(observed_state, expected);
        }
    }

    #[test]
    fn initial_observation_preserves_valid_unicode_c1_public_branch_data() {
        let head = "1111111111111111111111111111111111111111";
        let public = "2222222222222222222222222222222222222222";
        let branch = PublicBranchName::new("release-/\u{85}candidate".to_owned()).unwrap();
        let output = format!(
            "ref: refs/heads/main\tHEAD\n{head}\tHEAD\n{public}\trefs/heads/{}\n",
            branch.as_str()
        );

        let observation =
            parse_initial_remote_observation(output.as_bytes(), Some(branch.clone())).unwrap();
        let (_, observed) = observation.into_parts();
        let (observed_name, observed_state) = observed.unwrap().into_parts();

        assert_eq!(observed_name, branch);
        assert_eq!(
            observed_state,
            RemoteBranchState::At(ObjectId::from_hex(public.as_bytes()).unwrap())
        );
    }

    #[test]
    fn initial_observation_rejects_ambiguous_public_branch_records() {
        let oid = "1111111111111111111111111111111111111111";
        let branch = PublicBranchName::new("release-candidate".to_owned()).unwrap();
        for extra in [
            format!("{oid}\trefs/heads/release-candidate\n{oid}\trefs/heads/release-candidate\n"),
            "ref: refs/heads/other\trefs/heads/release-candidate\n".to_owned(),
        ] {
            let output = format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n{extra}");
            assert!(
                parse_initial_remote_observation(output.as_bytes(), Some(branch.clone())).is_err(),
                "extra={extra:?}"
            );
        }
    }

    #[test]
    fn initial_observation_rejects_malformed_public_branch_records() {
        let oid = "1111111111111111111111111111111111111111";
        let branch = PublicBranchName::new("release-candidate".to_owned()).unwrap();
        for extra in [
            "not-an-object-id\trefs/heads/release-candidate\n".to_owned(),
            "0000000000000000000000000000000000000000\trefs/heads/release-candidate\n".to_owned(),
            format!("{oid}\trefs/heads/invalid..branch\n"),
        ] {
            let output = format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n{extra}");
            assert!(
                parse_initial_remote_observation(output.as_bytes(), Some(branch.clone())).is_err(),
                "extra={extra:?}"
            );
        }
    }

    #[test]
    fn initial_observation_ignores_valid_unrequested_records_in_any_order() {
        let head = "1111111111111111111111111111111111111111";
        let public = "2222222222222222222222222222222222222222";
        let unrelated = "3333333333333333333333333333333333333333";
        let branch = PublicBranchName::new("release-candidate".to_owned()).unwrap();
        let output = format!(
            "{unrelated}\trefs/heads/candidate\n\
             {public}\trefs/heads/release-candidate\n\
             {head}\tHEAD\n\
             ref: refs/heads/other\trefs/remotes/upstream/HEAD\n\
             ref: refs/heads/main\tHEAD\n"
        );

        let observation =
            parse_initial_remote_observation(output.as_bytes(), Some(branch.clone())).unwrap();
        let (default, observed) = observation.into_parts();
        assert_eq!(default.name(), "main");
        let (observed_name, observed_state) = observed.unwrap().into_parts();
        assert_eq!(observed_name, branch);
        assert_eq!(
            observed_state,
            RemoteBranchState::At(ObjectId::from_hex(public.as_bytes()).unwrap())
        );
    }

    #[test]
    fn default_branch_rejects_the_owned_base_namespace() {
        let oid = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        for name in ["gherrit-bases", "gherrit-bases/main"] {
            assert_eq!(
                DefaultBranch::new(name.to_owned(), oid).unwrap_err().to_string(),
                "The repository default branch is in GHerrit's reserved 'gherrit-bases' namespace"
            );
        }
        assert!(DefaultBranch::new("gherrit-bases-other".to_owned(), oid).is_ok());
    }

    #[test]
    fn default_branch_record_order_is_irrelevant() {
        let oid = "1111111111111111111111111111111111111111";
        let expected =
            DefaultBranch::new("main".to_string(), ObjectId::from_hex(oid.as_bytes()).unwrap())
                .unwrap();

        for output in [
            format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n"),
            format!("{oid}\tHEAD\nref: refs/heads/main\tHEAD\n"),
        ] {
            assert_eq!(parse_default_branch(output.as_bytes()).unwrap(), expected);
        }
    }

    #[test]
    fn ignores_valid_unrelated_remote_records_including_non_utf8_names() {
        let oid = "1111111111111111111111111111111111111111";
        let other = "2222222222222222222222222222222222222222";
        let mut output = format!(
            "{other}\trefs/heads/HEAD\n\
             {other}\trefs/tags/HEAD\n\
             {other}\trefs/tags/HEAD^{{}}\n\
             ref: refs/heads/elsewhere\trefs/remotes/origin/HEAD\n\
             {oid}\tHEAD\n\
             ref: refs/heads/main\tHEAD\n"
        )
        .into_bytes();
        output.extend_from_slice(format!("{other}\trefs/heads/").as_bytes());
        output.extend_from_slice(b"\xff-HEAD\n");

        assert_eq!(
            parse_default_branch(&output).unwrap(),
            DefaultBranch::new("main".to_string(), ObjectId::from_hex(oid.as_bytes()).unwrap(),)
                .unwrap()
        );
    }

    #[test]
    fn rejects_every_malformed_default_branch_observation() {
        let oid = "1111111111111111111111111111111111111111";
        let null_oid = "0000000000000000000000000000000000000000";
        for output in [
            Vec::new(),
            format!("{oid}\tHEAD\n").into_bytes(),
            format!("ref: refs/tags/main\tHEAD\n{oid}\tHEAD\n").into_bytes(),
            format!("ref: refs/heads/main\r\tHEAD\n{oid}\tHEAD\n").into_bytes(),
            "ref: refs/heads/main\tHEAD\nxyz\tHEAD\n".to_string().into_bytes(),
            format!("ref: refs/heads/main\tHEAD\n{null_oid}\tHEAD\n").into_bytes(),
            format!("ref: refs/heads/main\tHEAD\n{oid}\tOTHER\n").into_bytes(),
            format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\nextra\n").into_bytes(),
            format!(
                "ref: refs/heads/main\tHEAD\n\
                 ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n"
            )
            .into_bytes(),
            format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n{oid}\tHEAD\n").into_bytes(),
            format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\nxyz\trefs/heads/other\n")
                .into_bytes(),
            format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n{null_oid}\trefs/heads/other\n")
                .into_bytes(),
            format!("ref: invalid\trefs/heads/other\n{oid}\tHEAD\n").into_bytes(),
            format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n{oid}\tinvalid\n").into_bytes(),
            format!(
                "ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n\
                 {oid}\trefs/heads/other^{{}}\n"
            )
            .into_bytes(),
            format!(
                "ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n\
                 ref: refs/heads/other\trefs/tags/HEAD^{{}}\n"
            )
            .into_bytes(),
            {
                let mut output = b"ref: refs/heads/".to_vec();
                output.extend_from_slice(b"\xff\tHEAD\n");
                output.extend_from_slice(format!("{oid}\tHEAD\n").as_bytes());
                output
            },
        ] {
            assert!(parse_default_branch(&output).is_err(), "output: {output:?}");
        }
    }
}
