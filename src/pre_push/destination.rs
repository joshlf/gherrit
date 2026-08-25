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
use crate::util;

const DESTINATION_ENV: &str = "GHERRIT_PRIVATE_PUSH_DESTINATION";
const PROXY_ENV: &str = "GHERRIT_PRIVATE_REMOTE_PROXY";
const PROXY_AUTH_METHOD_ENV: &str = "GHERRIT_PRIVATE_REMOTE_PROXY_AUTH_METHOD";
const GIT_CONFIG_PARAMETERS_ENV: &str = "GIT_CONFIG_PARAMETERS";
const DISABLE_HTTP_REDIRECTS: &str = "http.followRedirects=false";
const DISABLE_FOLLOW_TAGS: &str = "push.followTags=false";
const DISABLE_SUBMODULE_PUSHES: &str = "push.recurseSubmodules=no";
const CLEAR_PUSH_OPTIONS: &str = "push.pushOption=";
const INTERNAL_REMOTE_STEM: &str = "gherrit-publication";
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

/// A validated GitHub repository identity derived from the exact push
/// destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RepositoryCoordinates {
    owner: String,
    repository: String,
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
    resolved: ResolvedDestination,
    internal_remote: String,
    transport: RemoteTransportSettings,
}

impl PushDestination {
    /// Resolves the one exact destination Git would use for pushing.
    ///
    /// The configured remote is supplied by the caller so configuration is
    /// decoded and validated exactly once per publication attempt. `--` is
    /// required because Git permits manually configured remote names beginning
    /// with a hyphen.
    pub(super) fn resolve(configured_remote: util::RemoteName) -> Result<Self> {
        // The private adapter below depends on `--config-env`, introduced in
        // Git 2.31. Check explicitly instead of letting an older Git reject an
        // otherwise opaque internal command later in the attempt.
        util::require_git_config_env()?;

        let current_dir = env::current_dir()
            .wrap_err("Failed to resolve the Git repository's current directory")?;
        let mut command = util::cmd(
            "git",
            ["remote", "get-url", "--push", "--all", "--", configured_remote.as_str()],
        );
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

        let resolved =
            ResolvedDestination::from_git_output(configured_remote, &output.stdout, &current_dir)?;

        // Inspecting the active configuration includes any inherited inputs
        // which participated in `includeIf.hasconfig:remote.*.url` selection.
        // Git does not rescan those conditions after the later `--config-env`
        // options add GHerrit's private remote. Use the observed finite set to
        // validate transport policy and select an absent remote name.
        let configuration = resolved.inspect_configuration()?;
        reject_unsupported_remote_transport_configuration(
            resolved.configured_remote.as_str(),
            &configuration,
        )?;
        let transport = resolved.inspect_remote_transport_settings()?;
        let internal_remote = select_absent_remote(INTERNAL_REMOTE_STEM, &configuration);

        let destination = Self { resolved, internal_remote, transport };
        destination.inspect_internal_remote_configuration()?;
        destination.ensure_rewrite_fixed_point()?;
        Ok(destination)
    }

    /// Inspects the exact configuration context used by network commands.
    ///
    /// The internal name was absent from the complete configuration active
    /// before GHerrit added its private values. Git does not rescan
    /// `hasconfig:remote.*.url` includes after GHerrit's `--config-env` options,
    /// so only the command-scoped URL and explicitly preserved transport keys
    /// can configure this remote.
    /// This final inspection defends that argument directly: every planned key
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
        self.resolved.private_remote_command(&self.internal_remote, &self.transport, arguments)
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

    /// Observes destination refs through the sole bounded execution path.
    ///
    /// Keeping the raw `ls-remote` command private prevents an observation
    /// caller from bypassing the execution deadline, output limit, descendant
    /// cleanup, or asynchronous process supervision.
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

    pub(super) fn push(
        &self,
        git_dir: &util::GitDirIdentity,
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
            // The porcelain status records are machine-readable, but Git's
            // header and footer are translated. Fix their message locale so
            // receipt framing never depends on the invoking user's locale.
            .env("LC_ALL", "C")
            .env(INTERNAL_PRE_PUSH_REMOTE_ENV, &self.internal_remote)
            .env(INTERNAL_PRE_PUSH_GIT_DIR_ENV, git_dir.as_os_str());
        command
    }

    pub(super) fn configured_remote(&self) -> &str {
        self.resolved.configured_remote.as_str()
    }

    pub(super) fn coordinates(&self) -> &RepositoryCoordinates {
        &self.resolved.coordinates
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

    /// Observes the symbolic default branch and its exact tip from this
    /// destination. No local remote-tracking ref participates in the result.
    pub(super) async fn observe_default_branch(&self) -> Result<DefaultBranch> {
        observe_default_branch_command(
            self.ls_remote(["--symref".to_string()], ["HEAD".to_string()]),
            self.configured_remote(),
            subprocess::REMOTE_GIT_EXECUTION_TIMEOUT,
            subprocess::REMOTE_GIT_STDOUT_LIMIT,
        )
        .await
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
        let retained = u64::try_from(stderr.len()).unwrap_or(u64::MAX);
        let omitted = stderr_bytes.saturating_sub(retained);
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
    let output = subprocess::output_with_stdout_limit(command, timeout, stdout_limit)
        .await
        .wrap_err_with(|| format!("Failed to observe GHerrit remote '{configured_remote}'"))?;
    if !output.status().success() {
        bail!("`git ls-remote --symref` failed for GHerrit remote '{configured_remote}'");
    }
    parse_default_branch(output.stdout()).wrap_err_with(|| {
        format!(
            "GHerrit remote '{configured_remote}' did not report one valid symbolic default branch"
        )
    })
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
        let coordinates = match parse_destination(&literal) {
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

        Ok(Self { configured_remote, literal, coordinates, http_redirect_parameters })
    }

    /// Reads configuration which is active before GHerrit adds any remote.
    fn inspect_configuration(&self) -> Result<Vec<Vec<u8>>> {
        let mut command = util::cmd("git", ["config", "--null", "--name-only", "--list"]);
        clear_git_transport_diagnostics(&mut command);
        let output = command.output().wrap_err_with(|| {
            format!(
                "Failed to inspect Git configuration while resolving GHerrit remote '{}'",
                self.configured_remote.as_str()
            )
        })?;
        decode_config_names(&output, self.configured_remote.as_str(), "Git configuration")
    }

    /// Reads the effective configured-remote values from the same active
    /// configuration that can affect publication. `git config --get`
    /// deliberately selects Git's effective last value when a key occurs more
    /// than once.
    fn inspect_remote_transport_settings(&self) -> Result<RemoteTransportSettings> {
        Ok(RemoteTransportSettings {
            proxy: self.inspect_remote_transport_setting("proxy")?,
            proxy_auth_method: self.inspect_remote_transport_setting("proxyAuthMethod")?,
        })
    }

    fn inspect_remote_transport_setting(&self, key: &str) -> Result<Option<String>> {
        let key = format!("remote.{}.{key}", self.configured_remote.as_str());
        let mut command =
            util::cmd("git", ["config".to_owned(), "--null".to_owned(), "--get".to_owned(), key]);
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
    let mut symbolic_head = None;
    let mut direct_head = None;

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
    DefaultBranch::new(branch, direct_head)
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
        resolved_result(output).unwrap()
    }

    fn resolved_result(output: &[u8]) -> Result<ResolvedDestination> {
        ResolvedDestination::from_git_output(remote(), output, &env::current_dir().unwrap())
    }

    fn destination() -> PushDestination {
        PushDestination {
            resolved: resolved(b"https://github.com/owner/repo.git\n"),
            internal_remote: INTERNAL_REMOTE_STEM.to_owned(),
            transport: RemoteTransportSettings::default(),
        }
    }

    fn git_dir_identity() -> util::GitDirIdentity {
        util::Repo::open(".").unwrap().git_dir_identity().clone()
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
        let settings = destination.inspect_remote_transport_settings().unwrap_or_else(|error| {
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
        let observation = observe_default_branch_command(
            observation_fixture("hang"),
            "origin",
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
        let error = observe_default_branch_command(
            observation_fixture("overflow"),
            "origin",
            Duration::from_secs(2),
            32,
        )
        .await
        .unwrap_err();

        assert!(format!("{error:?}").contains("stdout exceeded the 32-byte limit"));
        assert!(!format!("{error:?}").contains(&"x".repeat(32)));
        assert!(started.elapsed() < Duration::from_secs(1));
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
        let destination = PushDestination {
            resolved: resolved(format!("{literal}\n").as_bytes()),
            internal_remote: INTERNAL_REMOTE_STEM.to_owned(),
            transport: RemoteTransportSettings::default(),
        };

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

        let git_dir = git_dir_identity();
        let push = destination.push(
            &git_dir,
            ["--atomic".to_string()],
            ["HEAD:refs/heads/Gone".to_string()],
        );
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
                .find(|(name, _)| *name == OsStr::new("LC_ALL"))
                .and_then(|(_, value)| value),
            Some(OsStr::new("C"))
        );
        assert_eq!(
            push.get_envs()
                .find(|(name, _)| *name == OsStr::new(INTERNAL_PRE_PUSH_REMOTE_ENV))
                .and_then(|(_, value)| value),
            Some(OsStr::new("gherrit-publication"))
        );
        assert_eq!(
            push.get_envs()
                .find(|(name, _)| *name == OsStr::new(INTERNAL_PRE_PUSH_GIT_DIR_ENV))
                .and_then(|(_, value)| value),
            Some(git_dir.as_os_str())
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
            destination.push(&git_dir_identity(), std::iter::empty(), std::iter::empty()),
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
            ("[::1]:owner/repo.git", "owner", "repo"),
            ("user@[2001:db8::1]:owner/repo", "owner", "repo"),
            ("alias:owner/repo.git", "owner", "repo"),
            ("alias:owner/repo", "owner", "repo"),
            ("http://localhost:3000/owner/repo.git", "owner", "repo"),
            ("http://my-gh.com/owner/repo", "owner", "repo"),
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
    fn canonical_local_destination_binds_git_literal_and_repository_identity() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("victim/actual-owner/actual-repository.git");
        fs::create_dir_all(&target).unwrap();
        let spelled =
            directory.path().join("victim/actual-owner/../actual-owner/actual-repository.git");
        let destination = ResolvedDestination::from_git_output(
            remote(),
            format!("{}\n", spelled.display()).as_bytes(),
            directory.path(),
        )
        .unwrap();

        assert_eq!(Path::new(&destination.literal), dunce::canonicalize(&target).unwrap());
        assert_eq!(destination.coordinates.owner, "actual-owner");
        assert_eq!(destination.coordinates.repository, "actual-repository");
    }

    #[cfg(unix)]
    #[test]
    fn canonical_local_destination_follows_one_symlink_before_deriving_identity() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("victim/actual-owner/actual-repository.git");
        let link = directory.path().join("attacker/claimed-owner/claimed-repository.git");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&target, &link).unwrap();
        let destination = ResolvedDestination::from_git_output(
            remote(),
            b"attacker/claimed-owner/claimed-repository.git\n",
            directory.path(),
        )
        .unwrap();

        assert_eq!(Path::new(&destination.literal), dunce::canonicalize(&target).unwrap());
        assert_eq!(destination.coordinates.owner, "actual-owner");
        assert_eq!(destination.coordinates.repository, "actual-repository");
    }

    #[test]
    fn rejects_file_urls_with_actionable_redacted_advice() {
        let secret = "file:///private/secret-owner/secret-repository.git";
        let error = resolved_result(format!("{secret}\n").as_bytes())
            .err()
            .expect("file URLs are unsupported");

        assert!(error.to_string().contains("uses an unsupported file URL"));
        assert!(error.to_string().contains("native filesystem path"));
        assert!(!error.to_string().contains(secret));
        assert!(!error.to_string().contains("secret-owner"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_accepts_native_paths_and_crlf_output() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owner/repo.git");
        fs::create_dir_all(&path).unwrap();
        let destination = resolved(format!("{}\r\n", path.display()).as_bytes());
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
        let error = resolved_result(destination)
            .err()
            .expect("a Unix destination's terminal CR must not be discarded");
        assert!(!error.to_string().contains("https://"));

        assert!(matches!(
            parse_destination(r"C:\tmp\owner\repo.git"),
            Some(ParsedDestination::Scp { .. })
        ));
        assert!(matches!(
            parse_destination(r"/tmp/attacker/owner\repo.git"),
            Some(ParsedDestination::Local { .. })
        ));
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
            "[::1:owner/repo",
            "[::1]owner/repo",
            "user@[::1]]:owner/repo",
            "::1:owner/repo",
            r"file:///tmp/attacker/owner\repo",
            "https://github.com/owner/repo\r",
            "https://github.com/owner/repo\npoison",
            "https://github.com/owner/repo\0suffix",
        ] {
            let error = resolved_result(destination.as_bytes())
                .err()
                .expect("malformed destination must be rejected");
            if !destination.is_empty() {
                assert!(!error.to_string().contains(destination), "destination: {destination:?}");
            }
        }
        assert!(resolved_result(b"\xff\n").is_err());
    }

    #[test]
    fn rejects_uri_user_information_without_disclosure() {
        for destination in [
            "https://token-secret@github.com/owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
            "file://user:password-secret@localhost/tmp/owner/repo.git",
        ] {
            let error = resolved_result(format!("{destination}\n").as_bytes())
                .err()
                .expect("URI user information must be rejected");
            assert!(error.to_string().contains("use a Git credential helper or an SCP-style SSH"));
            assert!(!error.to_string().contains(destination));
            assert!(!error.to_string().contains("secret"));
        }

        assert!(resolved_result(b"git@github.com:owner/repo.git\n").is_ok());
    }

    #[test]
    fn rejects_zero_or_multiple_destinations_without_disclosing_them() {
        let error = resolved_result(b"").err().expect("an empty destination must be rejected");
        assert_eq!(error.to_string(), "GHerrit remote 'origin' has no push destination");

        let secret = "https://user:secret@example.com/owner/repo.git";
        let output = format!("{secret}\nowner/other.git\n");
        let error = resolved_result(output.as_bytes())
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
        let destination = PushDestination {
            resolved: resolved(format!("{literal}\n").as_bytes()),
            internal_remote: INTERNAL_REMOTE_STEM.to_owned(),
            transport: RemoteTransportSettings {
                proxy: Some("opaque-proxy-secret".to_owned()),
                proxy_auth_method: Some("opaque-auth-secret".to_owned()),
            },
        };
        let diagnostic = format!(
            "policy denied publication for {canonical}\x1b[31m\r\n\
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
            .render_child_diagnostic(diagnostic.as_bytes(), diagnostic.len() as u64 + 7)
            .unwrap();

        assert!(rendered.contains("policy denied publication for <private destination>"));
        assert!(rendered.contains("normalized local destination <private destination>"));
        assert!(
            rendered.contains("proxy <private transport setting> uses <private transport setting>")
        );
        assert!(rendered.contains("alternate transport <path or URL redacted>"));
        assert!(rendered.contains("\\u{1b}[31m\\r"));
        assert!(rendered.contains("[7 earlier diagnostic bytes omitted]"));
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
