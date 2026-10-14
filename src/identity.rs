//! Commit identity syntax shared by hooks, publication, and inspection.
//!
//! Raw occurrences carry source ranges and preserve malformed separators.
//! The hook's legacy normalized-trailer policy remains a separate operation;
//! recognizing a value does not authorize publication or edit the message.

use std::{ops::Range, str};

use eyre::{Result, bail};

pub(crate) const MAX_GHERRIT_PR_ID_BYTES: usize = 128;

/// An ASCII alphanumeric GHerrit pull request ID of 1 through 128 bytes.
///
/// Construction proves the shared trailer and ref-component grammar. The
/// enclosing stack or remote-ref validation establishes where the value came
/// from and whether it identifies managed state.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct GherritPrId(
    // INVARIANT: `.0` is nonempty, ASCII alphanumeric, and at most
    // `MAX_GHERRIT_PR_ID_BYTES` bytes long.
    String,
);

impl GherritPrId {
    /// Decodes the same identity grammar from a managed remote ref component.
    ///
    /// Unlike a trailer error, this has no local commit to identify. Callers
    /// add the remote-ref context appropriate to the namespace they parse.
    pub(crate) fn from_ref_component(value: &[u8]) -> Result<Self> {
        if value.is_empty()
            || value.len() > MAX_GHERRIT_PR_ID_BYTES
            || !value.iter().all(u8::is_ascii_alphanumeric)
        {
            bail!("invalid GHerrit change ID");
        }
        Ok(Self(str::from_utf8(value)?.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

pub(crate) fn has_normalized_id(trailers: &str) -> bool {
    trailers.lines().any(|line| line.starts_with("gherrit-pr-id: "))
}

/// One occurrence of the identity key in a raw trailer block.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum GherritIdTrailer {
    /// An occurrence with GHerrit's exact `: ` separator.
    Exact { value: Vec<u8>, line: Range<usize> },
    /// An occurrence whose separator is not exactly `: `.
    Malformed,
}

/// Reads identity-key occurrences from the final raw commit-message paragraph.
///
/// Git's trailer formatter normalizes inputs such as `key:value` into
/// `key: value`. The raw bytes are therefore the only authority for GHerrit's
/// exact `: ` separator. Same-key occurrences with other separators remain
/// visible as malformed rather than disappearing. Other keys matter only for
/// recognizing the final block. Continuations are unfolded so validation
/// rejects a continued value rather than silently accepting its first line.
pub(crate) fn gherrit_id_trailers(message: &[u8]) -> Vec<GherritIdTrailer> {
    let mut start = 0;
    let mut lines = message
        .split_inclusive(|byte| *byte == b'\n')
        .map(|terminated| {
            let end = start + terminated.len();
            let line = terminated.strip_suffix(b"\n").unwrap_or(terminated);
            let raw_line = start..end;
            start = end;
            (raw_line, line.strip_suffix(b"\r").unwrap_or(line))
        })
        .collect::<Vec<_>>();
    while lines.last().is_some_and(|(_, line)| line.iter().all(u8::is_ascii_whitespace)) {
        lines.pop();
    }
    let block_start = lines
        .iter()
        .rposition(|(_, line)| line.iter().all(u8::is_ascii_whitespace))
        .map_or(0, |index| index + 1);
    let mut identities = Vec::<GherritIdTrailer>::new();
    let mut has_entry = false;
    let mut continued_identity: Option<usize> = None;

    for (raw_line, line) in &lines[block_start..] {
        if line.first().is_some_and(|byte| matches!(byte, b' ' | b'\t')) {
            if !has_entry {
                return Vec::new();
            }
            if let Some(index) = continued_identity {
                let GherritIdTrailer::Exact { value, .. } = &mut identities[index] else {
                    unreachable!("only exact identity trailers have continuation indices")
                };
                value.push(b' ');
                let value_start = line
                    .iter()
                    .position(|byte| !matches!(byte, b' ' | b'\t'))
                    .unwrap_or(line.len());
                value.extend_from_slice(&line[value_start..]);
            }
            continue;
        }

        let Some(separator) = line.iter().position(|byte| matches!(byte, b':' | b'=')) else {
            return Vec::new();
        };
        let key = &line[..separator];
        if key.is_empty()
            || key.iter().any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Vec::new();
        }

        has_entry = true;
        continued_identity = None;
        if key.eq_ignore_ascii_case(b"gherrit-pr-id") {
            if line.get(separator..separator + 2) == Some(b": ") {
                identities.push(GherritIdTrailer::Exact {
                    value: line[separator + 2..].to_vec(),
                    line: raw_line.clone(),
                });
                continued_identity = Some(identities.len() - 1);
            } else {
                identities.push(GherritIdTrailer::Malformed);
            }
        }
    }

    identities
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_codec_identity_bytes_preserve_ranges_and_literal_alphabet() {
        let seed = b"Subject\n\ngherrit-pr-id: Gone\nReviewed-by: Person\n";
        for byte in 0..=u8::MAX {
            let expected = matches!(byte, b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z');
            assert_eq!(GherritPrId::from_ref_component(&[byte]).is_ok(), expected);
            for position in 0..=seed.len() {
                let mut input = seed.to_vec();
                input.insert(position, byte);
                for occurrence in gherrit_id_trailers(&input) {
                    if let GherritIdTrailer::Exact { line, .. } = occurrence {
                        let source =
                            input.get(line.clone()).expect("source range must remain in bounds");
                        assert!(source[..13].eq_ignore_ascii_case(b"gherrit-pr-id"));
                        assert_eq!(&source[13..15], b": ");
                        assert!(line.start == 0 || input[line.start - 1] == b'\n');
                    }
                }
            }
        }
    }
}
