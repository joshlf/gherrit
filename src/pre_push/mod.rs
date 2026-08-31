use std::{
    env,
    ffi::{OsStr, OsString},
    io,
};

use color_eyre::eyre::{Context as _, Result, bail};

use crate::util;

mod autosquash;
mod destination;
mod json;
mod local;
mod publication_attempt;
mod subprocess;

use destination::PushDestination;

const INTERNAL_PRE_PUSH_GIT_DIR_ENV: &str = "GHERRIT_INTERNAL_PRE_PUSH_GIT_DIR";
const INTERNAL_PRE_PUSH_REMOTE_ENV: &str = "GHERRIT_INTERNAL_PRE_PUSH_REMOTE";

/// The complete externally relevant shape of one hook invocation.
///
/// GHerrit's own nested pushes return before constructing this value. Direct
/// invocation has no enclosing effect. For a managed Git invocation, one byte
/// is enough to distinguish a no-op from a push which could mutate a ref after
/// this hook returns. Unmanaged invocations never consume their input.
pub(crate) struct Invocation {
    source: InvocationSource,
}

enum InvocationSource {
    /// Direct invocation of the hidden hook command has no enclosing Git
    /// process and therefore no later ref effect.
    Direct,
    Git {
        remote_name: OsString,
        remote_location: OsString,
    },
}

impl Invocation {
    pub(crate) fn new(
        remote_name: Option<OsString>,
        remote_location: Option<OsString>,
    ) -> Result<Self> {
        let source = match (remote_name, remote_location) {
            (None, None) => InvocationSource::Direct,
            (Some(remote_name), Some(remote_location)) => {
                InvocationSource::Git { remote_name, remote_location }
            }
            (Some(_), None) | (None, Some(_)) => {
                bail!("Git pre-push hook arguments are incomplete")
            }
        };
        Ok(Self { source })
    }

    fn input_has_ref_updates(input: &mut impl io::Read) -> Result<bool> {
        let mut first = [0_u8; 1];
        loop {
            match input.read(&mut first) {
                Ok(0) => return Ok(false),
                Ok(_) => return Ok(true),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error).wrap_err("Failed to read Git pre-push input"),
            }
        }
    }

    /// Proves that no enclosing Git process can write a ref after GHerrit's
    /// acknowledged publication finishes.
    pub(super) fn require_managed_noop(self) -> Result<()> {
        self.require_managed_noop_from(&mut io::stdin().lock())
    }

    fn require_managed_noop_from(self, input: &mut impl io::Read) -> Result<()> {
        match self.source {
            InvocationSource::Direct => Ok(()),
            InvocationSource::Git { remote_name, remote_location }
                if remote_name == OsStr::new(".") && remote_location == OsStr::new(".") =>
            {
                if Self::input_has_ref_updates(input)? {
                    bail!(
                        "A managed GHerrit push cannot include an enclosing Git ref update; run plain 'git push' with the branch configuration created by 'gherrit manage'"
                    );
                }
                Ok(())
            }
            InvocationSource::Git { .. } => bail!(
                "A managed GHerrit push must use the local no-op destination created by 'gherrit manage'; run plain 'git push'"
            ),
        }
    }
}

/// Returns whether Git invoked this hook for a push started by GHerrit.
///
/// This marker is cooperative recursion control, not a private value or a
/// security boundary. Binding it to the exact per-worktree Git directory and
/// Git's remote-name argument prevents an inherited marker from disabling
/// GHerrit for an unrelated nested push.
pub(crate) fn is_internal_publication_push(
    repository: &util::Repo,
    remote_name: Option<&OsStr>,
    remote_location: Option<&OsStr>,
) -> bool {
    internal_publication_push_matches(
        repository.git_dir_identity().as_os_str(),
        remote_name,
        remote_location,
        env::var_os(INTERNAL_PRE_PUSH_REMOTE_ENV).as_deref(),
        env::var_os(INTERNAL_PRE_PUSH_GIT_DIR_ENV).as_deref(),
    )
}

fn internal_publication_push_matches(
    git_dir: &OsStr,
    remote_name: Option<&OsStr>,
    remote_location: Option<&OsStr>,
    remote_marker: Option<&OsStr>,
    git_dir_marker: Option<&OsStr>,
) -> bool {
    matches!(
        (remote_name, remote_location, remote_marker, git_dir_marker),
        (Some(remote_name), Some(remote_location), Some(remote_marker), Some(git_dir_marker))
            if !remote_name.is_empty()
                && !remote_location.is_empty()
                && remote_marker == remote_name
                && git_dir_marker == git_dir
    )
}

#[derive(Eq, PartialEq)]
pub(crate) enum GithubEndpoint {
    Production,
    #[cfg(feature = "test-driver")]
    Custom(String),
    #[cfg(any(test, feature = "test-driver"))]
    Disabled,
}

impl GithubEndpoint {
    fn is_disabled(&self) -> bool {
        #[cfg(any(test, feature = "test-driver"))]
        {
            *self == Self::Disabled
        }
        #[cfg(not(any(test, feature = "test-driver")))]
        {
            false
        }
    }

    fn custom_url(&self) -> Option<&str> {
        #[cfg(feature = "test-driver")]
        if let Self::Custom(url) = self {
            return Some(url);
        }
        None
    }

    /// Validates that this API endpoint and Git destination identify the same
    /// service boundary.
    ///
    /// A custom endpoint is an explicit test dependency and may accompany a
    /// custom Git transport. A disabled endpoint can execute only plans which
    /// need no GitHub operation. The production endpoint accepts only HTTPS or
    /// SSH destinations on the public GitHub service.
    fn validate_destination(&self, destination: &PushDestination) -> Result<()> {
        if matches!(self, Self::Production) && !destination.supports_production_github() {
            bail!("Production publication requires an HTTPS or SSH destination on github.com");
        }
        Ok(())
    }
}

pub async fn run(
    repository: &util::Repo,
    endpoint: &GithubEndpoint,
    invocation: Invocation,
) -> Result<()> {
    publication_attempt::run(repository, endpoint, invocation).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "test-driver")]
    #[test]
    fn endpoint_compatibility_is_an_explicit_runtime_matrix() {
        let repository = util::Repo::open(".").unwrap();
        let https = PushDestination::for_test_url_in(
            &repository,
            "https://github.com/owner/repository.git",
        );
        let ssh =
            PushDestination::for_test_url_in(&repository, "git@github.com:owner/repository.git");
        let http =
            PushDestination::for_test_url_in(&repository, "http://github.com/owner/repository.git");
        let git =
            PushDestination::for_test_url_in(&repository, "git://github.com/owner/repository.git");
        let helper = PushDestination::for_test_url_in(
            &repository,
            "fixture://example.test/owner/repository.git",
        );
        let directory = tempfile::tempdir().unwrap();
        let local = directory.path().join("owner/repository.git");
        std::fs::create_dir_all(&local).unwrap();
        let local = PushDestination::for_test_url_in(&repository, local.to_str().unwrap());
        let custom = GithubEndpoint::Custom("http://127.0.0.1:1".to_owned());
        let disabled = GithubEndpoint::Disabled;

        assert!(GithubEndpoint::Production.validate_destination(&https).is_ok());
        assert!(GithubEndpoint::Production.validate_destination(&ssh).is_ok());
        assert!(GithubEndpoint::Production.validate_destination(&http).is_err());
        assert!(GithubEndpoint::Production.validate_destination(&git).is_err());
        assert!(GithubEndpoint::Production.validate_destination(&local).is_err());
        assert!(GithubEndpoint::Production.validate_destination(&helper).is_err());
        assert!(custom.validate_destination(&http).is_ok());
        assert!(custom.validate_destination(&git).is_ok());
        assert!(custom.validate_destination(&local).is_ok());
        assert!(custom.validate_destination(&helper).is_ok());
        assert!(disabled.validate_destination(&local).is_ok());
    }

    #[test]
    fn internal_push_marker_must_match_complete_hook_arguments() {
        let git_dir = OsStr::new("/repository/.git/worktrees/current");
        let remote_marker = OsStr::new("gherrit-publication-2");
        let git_dir_marker = OsStr::new("/repository/.git/worktrees/current");

        assert!(internal_publication_push_matches(
            git_dir,
            Some(OsStr::new("gherrit-publication-2")),
            Some(OsStr::new("private-destination")),
            Some(remote_marker),
            Some(git_dir_marker),
        ));
        for (remote_name, remote_location, remote_marker, git_dir_marker) in [
            (
                Some(OsStr::new("origin")),
                Some(OsStr::new("private-destination")),
                Some(remote_marker),
                Some(git_dir_marker),
            ),
            (
                Some(OsStr::new("gherrit-publication-2")),
                Some(OsStr::new("")),
                Some(remote_marker),
                Some(git_dir_marker),
            ),
            (
                Some(OsStr::new("")),
                Some(OsStr::new("private-destination")),
                Some(remote_marker),
                Some(git_dir_marker),
            ),
            (
                Some(OsStr::new("gherrit-publication-2")),
                None,
                Some(remote_marker),
                Some(git_dir_marker),
            ),
            (
                None,
                Some(OsStr::new("private-destination")),
                Some(remote_marker),
                Some(git_dir_marker),
            ),
            (
                Some(OsStr::new("gherrit-publication-2")),
                Some(OsStr::new("private-destination")),
                None,
                Some(git_dir_marker),
            ),
            (
                Some(OsStr::new("gherrit-publication-2")),
                Some(OsStr::new("private-destination")),
                Some(remote_marker),
                None,
            ),
            (
                Some(OsStr::new("gherrit-publication-2")),
                Some(OsStr::new("private-destination")),
                Some(remote_marker),
                Some(OsStr::new("/repository/.git/worktrees/other")),
            ),
        ] {
            assert!(!internal_publication_push_matches(
                git_dir,
                remote_name,
                remote_location,
                remote_marker,
                git_dir_marker,
            ));
        }
    }

    #[test]
    fn managed_invocations_admit_only_direct_or_empty_loopback_pushes() {
        let invocation =
            |remote_name: Option<&str>, remote_location: Option<&str>, input: &[u8]| {
                let invocation = Invocation::new(
                    remote_name.map(OsString::from),
                    remote_location.map(OsString::from),
                )
                .unwrap();
                invocation.require_managed_noop_from(&mut &*input)
            };

        invocation(None, None, b"").unwrap();
        invocation(Some("."), Some("."), b"").unwrap();

        for candidate in [
            invocation(Some("."), Some("."), b"update\n"),
            invocation(Some("origin"), Some("repository.git"), b""),
            invocation(Some("."), Some("repository.git"), b""),
        ] {
            assert!(candidate.is_err());
        }

        assert!(Invocation::new(Some(OsString::from(".")), None).is_err());
        assert!(Invocation::new(None, Some(OsString::from("."))).is_err());
    }
}
