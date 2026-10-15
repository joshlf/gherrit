//! Pure committed-message facts; callers retain their admission and diagnostics.

use std::ops::Range;

use crate::identity::{GherritIdTrailer, gherrit_id_trailers};

pub(crate) const MAX_TITLE_SCALARS: usize = 256;

pub(crate) enum IdentityProblem {
    Missing,
    Multiple,
    Malformed,
}

pub(crate) struct RawIdentity {
    pub(crate) value: Vec<u8>,
    pub(crate) line: Range<usize>,
}

/// Classify exactly the supplied region. A successful range is relative to
/// that region; choosing full message versus body remains the caller's policy.
pub(crate) fn identity(source: &[u8]) -> Result<RawIdentity, IdentityProblem> {
    let mut occurrences = gherrit_id_trailers(source);
    match occurrences.len() {
        0 => Err(IdentityProblem::Missing),
        1 => match occurrences.pop().expect("one occurrence") {
            GherritIdTrailer::Exact { value, line } => Ok(RawIdentity { value, line }),
            GherritIdTrailer::Malformed => Err(IdentityProblem::Malformed),
        },
        _ => Err(IdentityProblem::Multiple),
    }
}

pub(crate) enum TitleProblem {
    Empty,
    Oversized,
}

pub(crate) fn title_problem(title: &str) -> Option<TitleProblem> {
    if title.is_empty() {
        Some(TitleProblem::Empty)
    } else if title.chars().nth(MAX_TITLE_SCALARS).is_some() {
        Some(TitleProblem::Oversized)
    } else {
        None
    }
}

pub(crate) fn pending_autosquash(title: &[u8]) -> bool {
    [b"fixup!".as_slice(), b"squash!", b"amend!"].iter().any(|prefix| title.starts_with(prefix))
}
