use std::{
    env,
    ffi::{OsStr, OsString},
    io,
};

use color_eyre::eyre::{Context as _, Result, bail};

use crate::util;

mod autosquash;
mod destination;
mod local;
mod publication_attempt;
mod subprocess;

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
/// security boundary. Binding it to Git's remote-name argument leaves the
/// marker inert for an ordinary nested push under a different name. A
/// cooperative descendant using the same name can still match it.
pub(crate) fn is_internal_publication_push(
    remote_name: Option<&OsStr>,
    remote_location: Option<&OsStr>,
) -> bool {
    internal_publication_push_matches(
        remote_name,
        remote_location,
        env::var_os(INTERNAL_PRE_PUSH_REMOTE_ENV).as_deref(),
    )
}

fn internal_publication_push_matches(
    remote_name: Option<&OsStr>,
    remote_location: Option<&OsStr>,
    marker: Option<&OsStr>,
) -> bool {
    matches!(
        (remote_name, remote_location, marker),
        (Some(remote_name), Some(remote_location), Some(marker))
            if !remote_name.is_empty()
                && !remote_location.is_empty()
                && marker == remote_name
    )
}

#[derive(Eq, PartialEq)]
pub(crate) enum GithubEndpoint {
    Production,
    #[cfg(feature = "test-driver")]
    Custom(String),
    #[cfg(feature = "test-driver")]
    Disabled,
}

impl GithubEndpoint {
    fn is_disabled(&self) -> bool {
        #[cfg(feature = "test-driver")]
        {
            *self == Self::Disabled
        }
        #[cfg(not(feature = "test-driver"))]
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

    #[test]
    fn internal_push_marker_must_match_complete_hook_arguments() {
        let marker = OsStr::new("gherrit-publication-2");

        assert!(internal_publication_push_matches(
            Some(OsStr::new("gherrit-publication-2")),
            Some(OsStr::new("private-destination")),
            Some(marker),
        ));
        for (remote_name, remote_location, marker) in [
            (Some(OsStr::new("origin")), Some(OsStr::new("private-destination")), Some(marker)),
            (Some(OsStr::new("gherrit-publication-2")), Some(OsStr::new("")), Some(marker)),
            (Some(OsStr::new("")), Some(OsStr::new("private-destination")), Some(marker)),
            (Some(OsStr::new("gherrit-publication-2")), None, Some(marker)),
            (None, Some(OsStr::new("private-destination")), Some(marker)),
            (
                Some(OsStr::new("gherrit-publication-2")),
                Some(OsStr::new("private-destination")),
                None,
            ),
        ] {
            assert!(!internal_publication_push_matches(remote_name, remote_location, marker));
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
