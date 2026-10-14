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

/// Native Git treats SP, HT, CR and LF as whitespace, retaining VT and FF.
pub(crate) fn is_git_whitespace(byte: &u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn trim_value_padding(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(is_git_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(is_git_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
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

/// Stable legacy boundaries, independent of later comment configuration.
/// Every section can carry a footer; competing IDs remain visible as ambiguity.
pub(crate) fn trailer_sections(message: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    let mut remaining = Some((0, message));
    std::iter::from_fn(move || {
        let (offset, section) = remaining.take()?;
        let mut length = 0;
        for line in section.split_inclusive(|byte| *byte == b'\n') {
            let content = line.strip_suffix(b"\n").unwrap_or(line);
            let content = content.strip_suffix(b"\r").unwrap_or(content);
            if content == b"# ------------------------ >8 ------------------------" {
                let after = length + line.len();
                remaining = Some((offset + after, &section[after..]));
                return Some((offset, &section[..length]));
            }
            length += line.len();
        }
        Some((offset, section))
    })
}

/// Reads identity-key occurrences from each final raw footer paragraph.
///
/// Literal legacy #comments and standard #scissors are interpreted without
/// consulting later repository configuration. IDs in different sections remain
/// separate occurrences, so callers can reject ambiguous before/after owners.
///
/// Git's trailer formatter normalizes inputs such as `key:value` into
/// `key: value`. The raw bytes are therefore the only authority for GHerrit's
/// exact `: ` separator. Same-key occurrences with other separators remain
/// visible as malformed rather than disappearing. Value padding follows native
/// Git whitespace rules while source ranges retain the original bytes. Other
/// keys matter only for
/// recognizing the final block. Continuations are unfolded so validation
/// rejects a continued value rather than silently accepting its first line.
pub(crate) fn gherrit_id_trailers(message: &[u8]) -> Vec<GherritIdTrailer> {
    gherrit_id_trailers_with_separators(message, &[])
}

/// Also recognize configured legacy separators as malformed raw occurrences.
/// Native legacy queries stop at scissors, so their caller must count these
/// occurrences across every section. Stored publication keeps the fixed raw
/// grammar above, independent of later repository configuration.
pub(crate) fn gherrit_id_trailers_with_separators(
    message: &[u8],
    separators: &[u8],
) -> Vec<GherritIdTrailer> {
    trailer_sections(message)
        .flat_map(|(offset, section)| id_trailers_in_section(section, offset, separators))
        .collect()
}

pub(crate) fn footer_lines(message: &[u8], offset: usize) -> Vec<(Range<usize>, &[u8])> {
    let mut start = offset;
    let mut lines = message
        .split_inclusive(|byte| *byte == b'\n')
        .map(|terminated| {
            let end = start + terminated.len();
            let line = terminated.strip_suffix(b"\n").unwrap_or(terminated);
            let raw_line = start..end;
            start = end;
            (raw_line, line.strip_suffix(b"\r").unwrap_or(line))
        })
        .filter(|(_, line)| !line.starts_with(b"#"))
        .collect::<Vec<_>>();
    while lines.last().is_some_and(|(_, line)| line.iter().all(is_git_whitespace)) {
        lines.pop();
    }
    let block_start = lines
        .iter()
        .rposition(|(_, line)| line.iter().all(is_git_whitespace))
        .map_or(0, |index| index + 1);
    lines.drain(..block_start);
    lines
}

fn id_trailers_in_section(
    message: &[u8],
    offset: usize,
    separators: &[u8],
) -> Vec<GherritIdTrailer> {
    let lines = footer_lines(message, offset);
    let mut identities = Vec::<GherritIdTrailer>::new();
    let mut has_entry = false;
    let mut continued_identity: Option<usize> = None;

    for (raw_line, line) in &lines {
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

        let key_end = b"gherrit-pr-id".len();
        let legacy_separator = line
            .get(..key_end)
            .filter(|key| key.eq_ignore_ascii_case(b"gherrit-pr-id"))
            .and_then(|_| {
                let padding =
                    line[key_end..].iter().take_while(|byte| matches!(byte, b' ' | b'\t')).count();
                let index = key_end + padding;
                line.get(index).filter(|byte| separators.contains(byte)).map(|_| index)
            });
        let Some(separator) = legacy_separator
            .or_else(|| line.iter().position(|byte| matches!(byte, b':' | b'=')))
            .or_else(|| line.iter().position(|byte| separators.contains(byte)))
        else {
            return Vec::new();
        };
        let key = &line[..if legacy_separator.is_some() { key_end } else { separator }];
        if key.is_empty()
            || key.iter().any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Vec::new();
        }

        has_entry = true;
        continued_identity = None;
        if key.eq_ignore_ascii_case(b"gherrit-pr-id") {
            if separator == key_end && line.get(separator..separator + 2) == Some(b": ") {
                identities.push(GherritIdTrailer::Exact {
                    value: trim_value_padding(&line[separator + 2..]).to_vec(),
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

    #[test]
    fn legacy_hash_comments_and_scissors_keep_source_ranges_and_ambiguity() {
        let marker = "# ------------------------ >8 ------------------------";
        for message in [
            "Subject\n\ngherrit-pr-id: Gone\n# retained comment\n".to_owned(),
            format!("Subject\n\ngherrit-pr-id: Gone\n\n{marker}\nRetained diff\n"),
            format!("{marker}\nSubject\n\ngherrit-pr-id: Gone\n"),
        ] {
            let occurrences = gherrit_id_trailers(message.as_bytes());
            let [GherritIdTrailer::Exact { value, line }] = occurrences.as_slice() else {
                panic!("{message:?}")
            };
            assert_eq!(value, b"Gone");
            assert_eq!(&message.as_bytes()[line.clone()], b"gherrit-pr-id: Gone\n");
        }
        let ambiguous =
            format!("Subject\n\ngherrit-pr-id: Gone\n{marker}\nSubject\n\ngherrit-pr-id: Gtwo\n");
        assert_eq!(gherrit_id_trailers(ambiguous.as_bytes()).len(), 2);
    }

    #[test]
    fn native_value_padding_normalizes_the_id_and_preserves_its_source_range() {
        for (padding, normalized) in [
            (b" ".as_slice(), true),
            (b"\t", true),
            (b"\r", true),
            (b"\x0b", false),
            (b"\x0c", false),
            (b"\xc2\xa0", false),
            (b"\x1c", false),
        ] {
            let mut line = b"gherrit-pr-id: ".to_vec();
            line.extend_from_slice(padding);
            line.extend_from_slice(b"Gone");
            line.extend_from_slice(padding);
            line.push(b'\n');
            let mut message = b"Subject\n\n".to_vec();
            message.extend_from_slice(&line);
            let entries = gherrit_id_trailers(&message);
            let [GherritIdTrailer::Exact { value, line: range }] = entries.as_slice() else {
                panic!("{entries:?}");
            };
            assert_eq!(&message[range.clone()], line);
            assert_eq!(GherritPrId::from_ref_component(value).is_ok(), normalized);
            if normalized {
                assert_eq!(value, b"Gone");
            }
        }
    }
    #[test]
    fn final_footer_blank_lines_follow_native_git_bytes() {
        for (line, blank) in [
            (b" ".as_slice(), true),
            (b"\t", true),
            (b"\r", true),
            (b"\x0b", false),
            (b"\x0c", false),
            (b"\x1c", false),
            (b"\xc2\xa0", false),
        ] {
            let mut message = b"Subject\n\ngherrit-pr-id: Gone\n".to_vec();
            message.extend_from_slice(line);
            message.push(b'\n');
            let occurrences = gherrit_id_trailers(&message);
            if blank {
                assert!(
                    matches!(occurrences.as_slice(), [GherritIdTrailer::Exact {value, ..}] if value == b"Gone")
                );
            } else {
                assert!(occurrences.is_empty(), "substantive final line {line:?}");
            }
        }
    }
    #[test]
    fn legacy_separator_occurrences_remain_visible_across_scissors() {
        for separator in ["%", "$", "@"] {
            let message = format!(
                "Subject\n\ngherrit-pr-id: Gbefore\n# ------------------------ >8 ------------------------\nSubject\n\ngherrit-pr-id{separator}Gafter\n"
            );
            assert_eq!(gherrit_id_trailers(message.as_bytes()).len(), 1);
            let occurrences =
                gherrit_id_trailers_with_separators(message.as_bytes(), separator.as_bytes());
            assert!(
                matches!(occurrences.as_slice(), [GherritIdTrailer::Exact {value, ..}, GherritIdTrailer::Malformed] if value == b"Gbefore")
            );
        }
    }
    #[test]
    fn legacy_separators_inside_keys_or_values_do_not_hide_identity_occurrences() {
        let canonical = b"Subject\n\ngherrit-pr-id: Gone\nReviewed-by: Person\n";
        assert!(
            matches!(gherrit_id_trailers_with_separators(canonical, b":r").as_slice(), [GherritIdTrailer::Exact {value, ..}] if value == b"Gone")
        );
        for suffix in ["%Gone:later", " %Gone", "$Gone"] {
            let message = format!("Subject\n\ngherrit-pr-id{suffix}\nOther%value\n");
            assert_eq!(
                gherrit_id_trailers_with_separators(message.as_bytes(), b":%$").as_slice(),
                [GherritIdTrailer::Malformed]
            );
        }
    }
}
