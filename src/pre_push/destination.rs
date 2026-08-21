//! The exact repository and default branch used by one publication attempt.
//!
//! Git permits a named remote to fetch from one URL and push to one or more
//! other URLs. GHerrit cannot safely observe one repository and then write to
//! another, and one atomic push cannot span several repositories. Resolving a
//! `PushDestination` establishes both the exact Git destination and the GitHub
//! repository identity used by the rest of the attempt.

use std::{env, process::Command, str};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::{ObjectId, bstr::ByteSlice as _};

use crate::util;

const DESTINATION_ENV: &str = "GHERRIT_PRIVATE_PUSH_DESTINATION";
const DISABLE_HTTP_REDIRECTS: &str = "http.followRedirects=false";
const INTERNAL_REMOTE_STEM: &str = "gherrit-publication";

/// One validated push destination for the configured GHerrit remote.
///
/// The destination itself is deliberately private and this type does not
/// implement `Debug`: remote URLs can contain credentials. Callers can pass it
/// to Git or use its derived repository identity, but must not log it.
pub(super) struct PushDestination {
    configured_remote: util::RemoteName,
    git_argument: String,
    internal_remote: String,
    owner: String,
    repository: String,
}

impl PushDestination {
    /// Resolves the one exact destination Git would use for pushing.
    ///
    /// The configured remote is supplied by the caller so configuration is
    /// decoded and validated exactly once per publication attempt. `--` is
    /// required because Git permits manually configured remote names beginning
    /// with a hyphen.
    pub(super) fn resolve(configured_remote: util::RemoteName) -> Result<Self> {
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

        let internal_remote = Self::select_internal_remote(&configured_remote)?;
        let destination =
            Self::from_git_output(configured_remote, &output.stdout, internal_remote)?;
        let configuration = destination.inspect_internal_remote_configuration()?;
        destination.ensure_rewrite_fixed_point()?;
        destination.ensure_http_redirects_disabled(&configuration)?;
        Ok(destination)
    }

    fn from_git_output(
        configured_remote: util::RemoteName,
        output: &[u8],
        internal_remote: String,
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

        let git_argument = str::from_utf8(destination).map(str::to_owned).map_err(|_| {
            eyre!(
                "GHerrit remote '{}' has a non-UTF-8 push destination",
                configured_remote.as_str()
            )
        })?;
        let (owner, repository) = repository_identity(&git_argument).ok_or_else(|| {
            eyre!(
                "The push destination for GHerrit remote '{}' does not identify a supported GitHub repository",
                configured_remote.as_str()
            )
        })?;

        Ok(Self { configured_remote, git_argument, internal_remote, owner, repository })
    }

    /// Selects a private remote name which cannot collide with configuration.
    ///
    /// The selected name is never written to repository configuration. Each
    /// destination-bearing command defines it only for that child process.
    /// Trying one more generated name than there are configured remotes proves
    /// that the finite search always succeeds.
    fn select_internal_remote(configured_remote: &util::RemoteName) -> Result<String> {
        let mut command = util::cmd("git", ["remote"]);
        clear_git_transport_diagnostics(&mut command);
        let output = command.output().wrap_err_with(|| {
            format!(
                "Failed to inspect configured remotes while resolving GHerrit remote '{}'",
                configured_remote.as_str()
            )
        })?;
        if !output.status.success() {
            bail!(
                "Failed to inspect configured remotes while resolving GHerrit remote '{}'",
                configured_remote.as_str()
            );
        }

        let existing = git_output_records(&output.stdout).collect::<Vec<_>>();
        (0..=existing.len())
            .map(|index| match index {
                0 => INTERNAL_REMOTE_STEM.to_owned(),
                _ => format!("{INTERNAL_REMOTE_STEM}-{index}"),
            })
            .find(|candidate| existing.iter().all(|remote| *remote != candidate.as_bytes()))
            .ok_or_else(|| eyre!("failed to select an unused internal Git remote name"))
    }

    /// Inspects the exact configuration context used by network commands.
    ///
    /// A conditional include can become active only after the internal URL is
    /// present. URL and push-URL resets make values from such an include
    /// harmless, but other settings for the private remote could still alter
    /// transport behavior. Rejecting every other key keeps the adapter's
    /// meaning limited to its two explicit destinations.
    fn inspect_internal_remote_configuration(&self) -> Result<Vec<Vec<u8>>> {
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
        if !output.status.success() || !output.stdout.ends_with(&[0]) {
            bail!(
                "Failed to inspect the private Git remote for GHerrit remote '{}'",
                self.configured_remote()
            );
        }

        let names = output
            .stdout
            .strip_suffix(&[0])
            .expect("the NUL terminator was checked above")
            .split(|byte| *byte == 0)
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>();
        if names.iter().any(Vec::is_empty) {
            bail!(
                "Git reported malformed private remote configuration for GHerrit remote '{}'",
                self.configured_remote()
            );
        }
        let prefix = format!("remote.{}.", self.internal_remote);
        let mut saw_url = false;
        let mut saw_pushurl = false;
        for suffix in names.iter().filter_map(|name| {
            name.get(..prefix.len())
                .filter(|actual| actual.eq_ignore_ascii_case(prefix.as_bytes()))
                .map(|_| &name[prefix.len()..])
        }) {
            if suffix.eq_ignore_ascii_case(b"url") {
                saw_url = true;
            } else if suffix.eq_ignore_ascii_case(b"pushurl") {
                saw_pushurl = true;
            } else {
                bail!(
                    "Git configuration changes the private publication remote for GHerrit remote '{}'",
                    self.configured_remote()
                );
            }
        }
        if !saw_url || !saw_pushurl {
            bail!(
                "Git configuration does not define the private publication remote for GHerrit remote '{}'",
                self.configured_remote()
            );
        }
        Ok(names)
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
            || records.next() != Some(self.git_argument.as_bytes())
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
    /// For a URL without user information, asking Git for the effective
    /// Boolean value uses the same URL matching rules as the later network
    /// commands and avoids rejecting unrelated scoped configuration. Putting a
    /// credential-bearing URL in a process argument would disclose it. For
    /// such a URL, any scoped redirect key is rejected without reading values.
    /// Failure and non-Boolean values fail closed.
    fn ensure_http_redirects_disabled(&self, configuration: &[Vec<u8>]) -> Result<()> {
        let Some((scheme, rest)) = self.git_argument.split_once("://") else {
            return Ok(());
        };
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return Ok(());
        }
        let authority = rest
            .split_once('/')
            .map(|(authority, _)| authority)
            .ok_or_else(|| eyre!("validated HTTP destination has no path"))?;
        if authority.contains('@') {
            return self.ensure_no_scoped_http_redirect_configuration(configuration);
        }

        let output = self
            .adapter_command([
                "-c".to_owned(),
                DISABLE_HTTP_REDIRECTS.to_owned(),
                "config".to_owned(),
                "--bool".to_owned(),
                "--get-urlmatch".to_owned(),
                "http.followRedirects".to_owned(),
                self.git_argument.clone(),
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

    fn ensure_no_scoped_http_redirect_configuration(
        &self,
        configuration: &[Vec<u8>],
    ) -> Result<()> {
        if configuration.iter().any(|name| is_scoped_http_follow_redirects_key(name)) {
            bail!(
                "URL-scoped Git HTTP redirect configuration is incompatible with credential-bearing GHerrit remote '{}'",
                self.configured_remote()
            );
        }
        Ok(())
    }

    /// Constructs a Git command in the private remote's exact configuration.
    ///
    /// The exact destination is supplied in a private environment variable.
    /// The argument list contains only a proved-absent internal remote name,
    /// so process listings, debug logs, and test traces cannot retain a URL or
    /// credentials. Empty values reset any additive values supplied by a
    /// conditional include before the exact destination is appended.
    fn adapter_command(&self, arguments: impl IntoIterator<Item = String>) -> Command {
        let url_key = format!("remote.{}.url", self.internal_remote);
        let pushurl_key = format!("remote.{}.pushurl", self.internal_remote);
        let arguments = [
            "-c".to_owned(),
            format!("{url_key}="),
            "-c".to_owned(),
            format!("{pushurl_key}="),
            format!("--config-env={url_key}={DESTINATION_ENV}"),
            format!("--config-env={pushurl_key}={DESTINATION_ENV}"),
        ]
        .into_iter()
        .chain(arguments);
        let mut command = util::cmd("git", arguments);
        command.env(DESTINATION_ENV, &self.git_argument);
        clear_git_transport_diagnostics(&mut command);
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

    pub(super) fn push(
        &self,
        options: impl IntoIterator<Item = String>,
        refspecs: impl IntoIterator<Item = String>,
    ) -> Command {
        self.remote_command("push", options, refspecs)
    }

    pub(super) fn configured_remote(&self) -> &str {
        self.configured_remote.as_str()
    }

    pub(super) fn owner(&self) -> &str {
        &self.owner
    }

    pub(super) fn repository(&self) -> &str {
        &self.repository
    }

    pub(super) fn pr_url(&self, pr_number: u64) -> String {
        format!("https://github.com/{}/{}/pull/{pr_number}", self.owner, self.repository)
    }

    pub(super) fn repo_url_relative(&self) -> String {
        format!("/{}/{}", self.owner, self.repository)
    }

    /// Observes the symbolic default branch and its exact tip from this
    /// destination. No local remote-tracking ref participates in the result.
    pub(super) fn observe_default_branch(&self) -> Result<DefaultBranch> {
        let output =
            self.ls_remote(["--symref".to_string()], ["HEAD".to_string()]).output().wrap_err_with(
                || format!("Failed to observe GHerrit remote '{}'", self.configured_remote()),
            )?;
        if !output.status.success() {
            bail!(
                "`git ls-remote --symref` failed for GHerrit remote '{}'",
                self.configured_remote()
            );
        }
        parse_default_branch(&output.stdout).wrap_err_with(|| {
            format!(
                "GHerrit remote '{}' did not report one valid symbolic default branch",
                self.configured_remote()
            )
        })
    }
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

fn is_scoped_http_follow_redirects_key(key: &[u8]) -> bool {
    const PREFIX: &[u8] = b"http.";
    const SUFFIX: &[u8] = b".followredirects";

    key.len() >= PREFIX.len() + SUFFIX.len()
        && key[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
        && key[key.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX)
}

/// One repository default branch, including the exact commit it names.
#[derive(Debug, Eq, PartialEq)]
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

    let path = if let Some((scheme, rest)) = destination.split_once("://") {
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
            path
        } else {
            let (authority, path) = rest.split_once('/')?;
            if authority.is_empty()
                || authority.chars().any(char::is_whitespace)
                || path.split('/').count() != 2
            {
                return None;
            }
            path
        }
    } else if is_scp_form(destination) {
        let (authority, path) = destination.split_once(':')?;
        if authority.is_empty()
            || authority.chars().any(char::is_whitespace)
            || path.split('/').count() != 2
        {
            return None;
        }
        path
    } else {
        destination
    };

    let normalized = path.replace('\\', "/");
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

/// Splits Git's line-oriented output while accepting native Windows CRLF.
///
/// Only the CR immediately before an LF is normalized. A bare or duplicated
/// CR remains data and is subsequently rejected wherever it is invalid.
pub(super) fn git_output_records(output: &[u8]) -> impl Iterator<Item = &[u8]> {
    output.split_inclusive(|byte| *byte == b'\n').map(|record| {
        let Some(record) = record.strip_suffix(b"\n") else {
            return record;
        };
        record.strip_suffix(b"\r").unwrap_or(record)
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    fn remote() -> util::RemoteName {
        util::RemoteName::from_config(b"origin").unwrap()
    }

    fn destination() -> PushDestination {
        PushDestination::from_git_output(
            remote(),
            b"https://github.com/owner/repo.git\n",
            INTERNAL_REMOTE_STEM.to_owned(),
        )
        .unwrap()
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
    fn every_destination_bearing_git_command_disables_redirects() {
        let destination = destination();
        let ls_remote = destination.ls_remote(["--symref".to_string()], ["HEAD".to_string()]);
        assert_eq!(
            arguments(&ls_remote),
            [
                "--no-replace-objects",
                "-c",
                "remote.gherrit-publication.url=",
                "-c",
                "remote.gherrit-publication.pushurl=",
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
                "-c",
                "remote.gherrit-publication.url=",
                "-c",
                "remote.gherrit-publication.pushurl=",
                "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "--config-env=remote.gherrit-publication.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "-c",
                "http.followRedirects=false",
                "push",
                "--atomic",
                "--",
                "gherrit-publication",
                "HEAD:refs/heads/Gone",
            ]
            .map(OsStr::new)
        );
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
            ("file://token:secret@localhost/tmp/owner/repo.git", "owner", "repo"),
            ("/tmp/test/owner/repo.git", "owner", "repo"),
            ("/tmp/owner/repo", "owner", "repo"),
            ("owner/repo", "owner", "repo"),
            (r"C:\tmp\owner\repo.git", "owner", "repo"),
            ("https://github.com/user-name/repo", "user-name", "repo"),
            ("https://github.com/user_name/repo", "user_name", "repo"),
            ("https://github.com/user.name/repo.name.git", "user.name", "repo.name"),
        ] {
            let destination = PushDestination::from_git_output(
                remote(),
                format!("{destination}\n").as_bytes(),
                INTERNAL_REMOTE_STEM.to_owned(),
            )
            .unwrap();

            assert_eq!(destination.owner(), owner);
            assert_eq!(destination.repository(), repository);
        }
    }

    #[test]
    fn accepts_one_cr_before_each_git_line_feed() {
        let destination = PushDestination::from_git_output(
            remote(),
            b"https://github.com/owner/repo.git\r\n",
            INTERNAL_REMOTE_STEM.to_owned(),
        )
        .unwrap();
        assert_eq!(destination.owner(), "owner");
        assert_eq!(destination.repository(), "repo");

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
            "git@github.com:owner/repo/extra",
            "https://github.com/owner/repo\r",
            "https://github.com/owner/repo\npoison",
            "https://github.com/owner/repo\0suffix",
        ] {
            let error = PushDestination::from_git_output(
                remote(),
                destination.as_bytes(),
                INTERNAL_REMOTE_STEM.to_owned(),
            )
            .err()
            .expect("malformed destination must be rejected");
            if !destination.is_empty() {
                assert!(!error.to_string().contains(destination), "destination: {destination:?}");
            }
        }
        assert!(
            PushDestination::from_git_output(remote(), b"\xff\n", INTERNAL_REMOTE_STEM.to_owned(),)
                .is_err()
        );
    }

    #[test]
    fn rejects_zero_or_multiple_destinations_without_disclosing_them() {
        let error =
            PushDestination::from_git_output(remote(), b"", INTERNAL_REMOTE_STEM.to_owned())
                .err()
                .expect("an empty destination must be rejected");
        assert_eq!(error.to_string(), "GHerrit remote 'origin' has no push destination");

        let secret = "https://user:secret@example.com/owner/repo.git";
        let output = format!("{secret}\nowner/other.git\n");
        let error = PushDestination::from_git_output(
            remote(),
            output.as_bytes(),
            INTERNAL_REMOTE_STEM.to_owned(),
        )
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
