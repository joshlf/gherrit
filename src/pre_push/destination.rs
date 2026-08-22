//! The exact repository and default branch used by one publication attempt.
//!
//! Git permits a named remote to fetch from one URL and push to one or more
//! other URLs. GHerrit cannot safely observe one repository and then write to
//! another, and one atomic push cannot span several repositories. Resolving a
//! `PushDestination` establishes both the exact Git destination and the GitHub
//! repository identity used by the rest of the attempt.

use std::{borrow::Cow, env, process::Command, str};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::ObjectId;

use super::subprocess;
use crate::util;

const DESTINATION_ENV: &str = "GHERRIT_PRIVATE_PUSH_DESTINATION";
const DISABLE_HTTP_REDIRECTS: &str = "http.followRedirects=false";
const CLEAR_PUSH_OPTIONS: &str = "push.pushOption=";
const INTERNAL_REMOTE_STEM: &str = "gherrit-publication";
const PROBE_REMOTE_STEM: &str = "gherrit-publication-probe";
const OWNED_BASE_BRANCH_ROOT: &str = "gherrit-bases";

/// The configured remote's one resolved push destination.
///
/// This value is kept separate from `PushDestination` until Git configuration
/// has established an internal remote name which is absent in the exact
/// command context used for publication.
struct ResolvedDestination {
    configured_remote: util::RemoteName,
    literal: String,
    owner: String,
    repository: String,
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
    #[cfg(test)]
    test_environment: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
}

impl PushDestination {
    #[cfg(test)]
    pub(super) fn for_test(
        configured_remote: &str,
        literal: &str,
        environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    ) -> Result<Self> {
        let configured_remote = util::RemoteName::from_config(configured_remote.as_bytes())?;
        let output = format!("{literal}\n");
        let resolved = ResolvedDestination::from_git_output(configured_remote, output.as_bytes())?;
        Ok(Self {
            resolved,
            internal_remote: INTERNAL_REMOTE_STEM.to_owned(),
            test_environment: Some(environment),
        })
    }

    /// Resolves the one exact destination Git would use for pushing.
    ///
    /// The configured remote is supplied by the caller so configuration is
    /// decoded and validated exactly once per publication attempt. `--` is
    /// required because Git permits manually configured remote names beginning
    /// with a hyphen.
    pub(super) async fn resolve(configured_remote: util::RemoteName) -> Result<Self> {
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

        let resolved = ResolvedDestination::from_git_output(configured_remote, &output.stdout)?;

        // A URL-conditioned include which is inactive in the repository's
        // ordinary configuration can become active as soon as GHerrit injects
        // the resolved destination. Use a throwaway, proved-absent remote to
        // activate that complete finite configuration before selecting the
        // remote used for network commands.
        let baseline = resolved.inspect_baseline_configuration()?;
        let probe_remote = select_absent_remote(PROBE_REMOTE_STEM, &baseline);
        let active = resolved.inspect_configuration_with_remote(&probe_remote)?;
        let internal_remote = select_absent_remote(INTERNAL_REMOTE_STEM, &active);

        let destination = Self {
            resolved,
            internal_remote,
            #[cfg(test)]
            test_environment: None,
        };
        destination.inspect_internal_remote_configuration()?;
        destination.ensure_rewrite_fixed_point().await?;
        destination.ensure_http_redirects_disabled()?;
        Ok(destination)
    }

    /// Inspects the exact configuration context used by network commands.
    ///
    /// The internal name was absent after the destination probe activated all
    /// URL-conditioned includes. Adding the same URL under the final name
    /// activates the same includes, so only GHerrit's two command-scoped keys
    /// can configure this remote. This final inspection defends that argument
    /// directly: each key must occur once, carry the exact private value, and
    /// be the only key for the internal remote.
    fn inspect_internal_remote_configuration(&self) -> Result<()> {
        let names = self.inspect_configuration()?;
        let urls = self.inspect_internal_remote_values("url")?;
        let pushurls = self.inspect_internal_remote_values("pushurl")?;
        validate_internal_remote_configuration(
            &self.internal_remote,
            &names,
            &urls,
            &pushurls,
            self.resolved.literal.as_bytes(),
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
        decode_config_records(&output, self.configured_remote(), "private Git remote")
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
    async fn ensure_rewrite_fixed_point(&self) -> Result<()> {
        let command = self.ls_remote(["--get-url".to_owned()], std::iter::empty());
        let output = subprocess::output(command, subprocess::REMOTE_GIT_EXECUTION_TIMEOUT)
            .await
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

    /// Requires the effective HTTP redirect policy to remain disabled.
    ///
    /// Git selects the most specific `http.<url>.*` value before falling back
    /// to the global `http.*` value. Consequently, even a command-line
    /// `http.followRedirects=false` does not override a more specific value.
    /// Asking Git for the effective Boolean value uses the same URL matching
    /// rules as the later network commands and avoids rejecting unrelated
    /// scoped configuration. Failure and non-Boolean values fail closed.
    fn ensure_http_redirects_disabled(&self) -> Result<()> {
        let Some((scheme, _)) = self.resolved.literal.split_once("://") else {
            return Ok(());
        };
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return Ok(());
        }

        let output = self
            .adapter_command([
                "-c".to_owned(),
                DISABLE_HTTP_REDIRECTS.to_owned(),
                "config".to_owned(),
                "--bool".to_owned(),
                "--get-urlmatch".to_owned(),
                "http.followRedirects".to_owned(),
                self.resolved.literal.clone(),
            ])
            .output()
            .wrap_err_with(|| {
                format!(
                    "Failed to verify the HTTP redirect policy for GHerrit remote '{}'",
                    self.configured_remote()
                )
            })?;
        let mut values = git_output_records(&output.stdout);
        if !output.status.success()
            || values.next() != Some(b"false".as_slice())
            || values.next().is_some()
        {
            bail!(
                "Git HTTP redirect configuration does not disable redirects for GHerrit remote '{}'",
                self.configured_remote()
            );
        }
        Ok(())
    }

    /// Constructs a Git command in the private remote's exact configuration.
    ///
    /// The exact destination is supplied in a private environment variable.
    /// The argument list contains only a proved-absent internal remote name,
    /// so process listings, debug logs, and test traces cannot retain the
    /// private local path. Exactly one URL and push URL are added; no empty
    /// reset values or Git-version-dependent additive behavior participate.
    fn adapter_command(&self, arguments: impl IntoIterator<Item = String>) -> Command {
        let command = self.resolved.private_remote_command(&self.internal_remote, arguments);
        #[cfg(test)]
        let mut command = command;
        #[cfg(test)]
        if let Some(environment) = &self.test_environment {
            command.env_clear();
            command.envs(environment.iter().cloned());
            command.env(DESTINATION_ENV, &self.resolved.literal);
            command.env("GIT_NO_REPLACE_OBJECTS", "1");
            command.env("GIT_NO_LAZY_FETCH", "1");
        }
        command
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

    pub(super) fn ls_remote(
        &self,
        options: impl IntoIterator<Item = String>,
        ref_patterns: impl IntoIterator<Item = String>,
    ) -> Command {
        self.remote_command("ls-remote", options, ref_patterns)
    }

    /// Constructs one exact source-only object-acquisition request.
    ///
    /// `source_refs` come only from the validated advertisement capability in
    /// `remote`. No destination ref is supplied, so Git writes objects without
    /// creating local refs or selecting configured fetch refspecs.
    pub(super) fn fetch(
        &self,
        source_refs: impl IntoIterator<Item = String>,
        refetch: bool,
    ) -> Command {
        self.remote_command(
            "fetch",
            [
                "--no-write-fetch-head".to_owned(),
                "--no-tags".to_owned(),
                "--no-recurse-submodules".to_owned(),
                "--no-auto-maintenance".to_owned(),
            ]
            .into_iter()
            .chain(refetch.then(|| "--refetch".to_owned())),
            source_refs,
        )
    }

    pub(super) fn push(
        &self,
        options: impl IntoIterator<Item = String>,
        refspecs: impl IntoIterator<Item = String>,
    ) -> Command {
        self.adapter_command(
            [
                "-c".to_owned(),
                DISABLE_HTTP_REDIRECTS.to_owned(),
                "-c".to_owned(),
                CLEAR_PUSH_OPTIONS.to_owned(),
                "push".to_owned(),
            ]
            .into_iter()
            .chain(options)
            .chain(["--".to_string(), self.internal_remote.clone()])
            .chain(refspecs),
        )
    }

    pub(super) fn configured_remote(&self) -> &str {
        self.resolved.configured_remote.as_str()
    }

    pub(super) fn owner(&self) -> &str {
        &self.resolved.owner
    }

    pub(super) fn repository(&self) -> &str {
        &self.resolved.repository
    }

    pub(super) fn pr_url(&self, pr_number: u64) -> String {
        format!(
            "https://github.com/{}/{}/pull/{pr_number}",
            self.resolved.owner, self.resolved.repository
        )
    }

    pub(super) fn repo_url_relative(&self) -> String {
        format!("/{}/{}", self.resolved.owner, self.resolved.repository)
    }
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
        let (owner, repository) = repository_identity(&literal).ok_or_else(|| {
            eyre!(
                "The push destination for GHerrit remote '{}' does not identify a supported GitHub repository",
                configured_remote.as_str()
            )
        })?;

        Ok(Self { configured_remote, literal, owner, repository })
    }

    /// Reads configuration which is active before GHerrit adds any remote.
    fn inspect_baseline_configuration(&self) -> Result<Vec<Vec<u8>>> {
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

    /// Reads configuration after the destination has activated URL-conditioned
    /// includes under a throwaway, proved-absent remote name.
    fn inspect_configuration_with_remote(&self, remote: &str) -> Result<Vec<Vec<u8>>> {
        let output = self
            .private_remote_command(
                remote,
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

    /// Adds exactly one private URL and push URL for a proved-absent name.
    fn private_remote_command(
        &self,
        remote: &str,
        arguments: impl IntoIterator<Item = String>,
    ) -> Command {
        let url_key = format!("remote.{remote}.url");
        let pushurl_key = format!("remote.{remote}.pushurl");
        let arguments = [
            format!("--config-env={url_key}={DESTINATION_ENV}"),
            format!("--config-env={pushurl_key}={DESTINATION_ENV}"),
        ]
        .into_iter()
        .chain(arguments);
        let mut command = util::cmd("git", arguments);
        command.env(DESTINATION_ENV, &self.literal);
        clear_git_transport_diagnostics(&mut command);
        command
    }
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

fn validate_internal_remote_configuration(
    remote: &str,
    names: &[Vec<u8>],
    urls: &[Vec<u8>],
    pushurls: &[Vec<u8>],
    destination: &[u8],
) -> Result<()> {
    let mut url_names = 0;
    let mut pushurl_names = 0;
    for suffix in names.iter().filter_map(|key| remote_configuration_suffix(key, remote)) {
        if suffix.eq_ignore_ascii_case(b"url") {
            url_names += 1;
        } else if suffix.eq_ignore_ascii_case(b"pushurl") {
            pushurl_names += 1;
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
    {
        bail!("the private remote does not have exactly one matching URL and push URL");
    }
    Ok(())
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
/// `GIT_TRACE*` spelling instead of maintaining a finite list. Git also has a
/// separate curl verbosity switch which can expose HTTP transport details.
fn clear_git_transport_diagnostics(command: &mut Command) {
    for (name, _) in env::vars_os() {
        let bytes = name.as_os_str().as_encoded_bytes();
        if is_git_transport_diagnostic(bytes) {
            command.env_remove(name);
        }
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
        if name == OWNED_BASE_BRANCH_ROOT
            || name
                .strip_prefix(OWNED_BASE_BRANCH_ROOT)
                .is_some_and(|suffix| suffix.starts_with('/'))
        {
            bail!("The repository default branch uses GHerrit's reserved owned-base namespace");
        }
        let full_name = format!("refs/heads/{name}");
        let validated = gix::refs::FullName::try_from(full_name.as_str())
            .wrap_err("The repository default branch has an invalid Git ref name")?;
        if validated.category() != Some(gix::refs::Category::LocalBranch) {
            bail!("The repository default branch is not a local branch");
        }
        if name.is_empty() || name.chars().any(char::is_control) {
            bail!("The repository default branch has an invalid name");
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

    /// Requires the local branch used for read-only stack derivation to be an
    /// exact copy of the branch observed at the push destination.
    pub(super) fn ensure_local(&self, repo: &util::Repo) -> Result<()> {
        let local_tip = repo
            .rev_parse_single(self.full_ref_name().as_str())
            .wrap_err_with(|| format!("Local default branch '{}' is unavailable", self.name))?
            .detach();
        if local_tip != self.tip {
            bail!("Local default branch '{}' does not match the push repository", self.name);
        }
        Ok(())
    }

    /// Establishes the default branch used by all write planning and intent.
    ///
    /// The local stack may be rejected before this GitHub read is necessary.
    /// A stack retained for publication was derived from the exact Git value;
    /// returning that same value after comparison proves that both systems
    /// agree before any write.
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
fn repository_identity(destination: &str) -> Option<(String, String)> {
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
    if !valid_repository_component(owner) || !valid_repository_component(repository) {
        return None;
    }

    Some(((*owner).to_owned(), repository.to_owned()))
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
    use std::ffi::OsStr;

    use super::*;

    fn remote() -> util::RemoteName {
        util::RemoteName::from_config(b"origin").unwrap()
    }

    fn resolved(output: &[u8]) -> ResolvedDestination {
        ResolvedDestination::from_git_output(remote(), output).unwrap()
    }

    fn destination() -> PushDestination {
        PushDestination {
            resolved: resolved(b"https://github.com/owner/repo.git\n"),
            internal_remote: INTERNAL_REMOTE_STEM.to_owned(),
            test_environment: None,
        }
    }

    fn arguments(command: &Command) -> Vec<&OsStr> {
        command.get_args().collect()
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
    fn default_branch_rejects_exactly_the_owned_base_namespace() {
        let tip = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();

        for name in ["gherrit-bases", "gherrit-bases/Gone", "gherrit-bases/nested/name"] {
            let error = DefaultBranch::new(name.to_owned(), tip).unwrap_err();
            assert!(error.to_string().contains("reserved owned-base namespace"), "error={error:?}");
        }

        for name in ["main", "gherrit-base", "gherrit-basesuffix", "prefix/gherrit-bases/Gone"] {
            assert!(DefaultBranch::new(name.to_owned(), tip).is_ok(), "name={name}");
        }
    }

    #[test]
    fn every_destination_bearing_git_command_disables_redirects() {
        let destination = destination();
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

        let fetch = destination.fetch(["refs/tags/gherrit/Gone/v1".to_owned()], false);
        assert_eq!(
            arguments(&fetch),
            [
                "--no-replace-objects",
                "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "--config-env=remote.gherrit-publication.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "-c",
                "http.followRedirects=false",
                "fetch",
                "--no-write-fetch-head",
                "--no-tags",
                "--no-recurse-submodules",
                "--no-auto-maintenance",
                "--",
                "gherrit-publication",
                "refs/tags/gherrit/Gone/v1",
            ]
            .map(OsStr::new)
        );

        let refetch = destination.fetch(["refs/tags/gherrit/Gone/v1".to_owned()], true);
        assert_eq!(
            arguments(&refetch),
            [
                "--no-replace-objects",
                "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "--config-env=remote.gherrit-publication.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "-c",
                "http.followRedirects=false",
                "fetch",
                "--no-write-fetch-head",
                "--no-tags",
                "--no-recurse-submodules",
                "--no-auto-maintenance",
                "--refetch",
                "--",
                "gherrit-publication",
                "refs/tags/gherrit/Gone/v1",
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
        assert!(
            arguments(&push)
                .iter()
                .all(|argument| *argument != OsStr::new("https://github.com/owner/repo.git"))
        );
        assert_eq!(
            push.get_envs()
                .find(|(name, _)| *name == OsStr::new(DESTINATION_ENV))
                .and_then(|(_, value)| value),
            Some(OsStr::new("https://github.com/owner/repo.git"))
        );

        let probe = destination.resolved.private_remote_command(
            PROBE_REMOTE_STEM,
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
    fn final_remote_validation_accepts_only_one_exact_url_pair() {
        let remote = INTERNAL_REMOTE_STEM;
        let destination = b"private/path/owner/repo.git";
        let valid_names = vec![
            b"remote.gherrit-publication.url".to_vec(),
            b"remote.gherrit-publication.pushurl".to_vec(),
            b"remote.unrelated.receivepack".to_vec(),
        ];
        let valid_urls = vec![destination.to_vec()];
        assert!(
            validate_internal_remote_configuration(
                remote,
                &valid_names,
                &valid_urls,
                &valid_urls,
                destination,
            )
            .is_ok()
        );

        for (names, urls, pushurls) in [
            (
                vec![
                    b"remote.gherrit-publication.url".to_vec(),
                    b"remote.gherrit-publication.url".to_vec(),
                    b"remote.gherrit-publication.pushurl".to_vec(),
                ],
                vec![destination.to_vec(), destination.to_vec()],
                valid_urls.clone(),
            ),
            (
                vec![
                    b"remote.gherrit-publication.url".to_vec(),
                    b"remote.gherrit-publication.pushurl".to_vec(),
                    b"remote.gherrit-publication.pushURL".to_vec(),
                ],
                valid_urls.clone(),
                vec![destination.to_vec(), destination.to_vec()],
            ),
            (valid_names.clone(), vec![b"different".to_vec()], valid_urls.clone()),
            (valid_names.clone(), valid_urls.clone(), vec![b"different".to_vec()]),
            (
                vec![
                    b"remote.gherrit-publication.url".to_vec(),
                    b"remote.gherrit-publication.pushurl".to_vec(),
                    b"remote.gherrit-publication.receivepack".to_vec(),
                ],
                valid_urls.clone(),
                valid_urls.clone(),
            ),
            (vec![b"remote.gherrit-publication.url".to_vec()], valid_urls.clone(), Vec::new()),
            (vec![b"remote.gherrit-publication.pushurl".to_vec()], Vec::new(), valid_urls.clone()),
        ] {
            assert!(
                validate_internal_remote_configuration(
                    remote,
                    &names,
                    &urls,
                    &pushurls,
                    destination,
                )
                .is_err(),
                "names: {names:?}, urls: {urls:?}, pushurls: {pushurls:?}"
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

            assert_eq!(destination.owner, owner);
            assert_eq!(destination.repository, repository);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_accepts_native_paths_and_crlf_output() {
        let destination = resolved(br"C:\tmp\owner\repo.git");
        assert_eq!(destination.owner, "owner");
        assert_eq!(destination.repository, "repo");

        let destination = resolved(b"https://github.com/owner/repo.git\r\n");
        assert_eq!(destination.owner, "owner");
        assert_eq!(destination.repository, "repo");
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
}
