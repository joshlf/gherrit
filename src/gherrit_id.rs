use std::{io::Write as _, process::Stdio};

use eyre::{Result, WrapErr as _, bail, eyre};

use crate::util;

pub const TRAILER_TOKEN: &[u8] = b"gherrit-pr-id";

/// The ID spellings GHerrit intentionally accepts.
///
/// New IDs use `CurrentBase32`. The two 40-hex forms are retained solely for
/// repositories created by pre-base32 GHerrit versions and Gerrit Change-Id
/// migrations, respectively. Accepting a small explicit legacy set avoids
/// turning an arbitrary commit-message string into a remote ref name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    CurrentBase32,
    LegacyGHex,
    LegacyChangeId,
}

pub fn validate(id: &str) -> Result<Format> {
    let bytes = id.as_bytes();

    if bytes.len() == 33
        && bytes[0] == b'G'
        && bytes[1..]
            .iter()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'2'..=b'7'))
    {
        return Ok(Format::CurrentBase32);
    }

    if bytes.len() == 41
        && matches!(bytes[0], b'G' | b'I')
        && bytes[1..]
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Ok(if bytes[0] == b'G' { Format::LegacyGHex } else { Format::LegacyChangeId });
    }

    bail!(
        "Invalid gherrit-pr-id `{id}`. Expected `G` followed by 32 lowercase base32 characters, or a supported legacy `G`/`I` ID followed by 40 lowercase hexadecimal characters."
    )
}

/// Extracts an ID from an already-parsed Git trailer block.
///
/// Callers are responsible for using a real trailer parser. In particular,
/// this function must not be fed every matching-looking line in a commit body.
pub fn from_trailers<'a>(
    trailers: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> Result<Option<String>> {
    let values = trailers
        .into_iter()
        .filter(|(token, _)| token.eq_ignore_ascii_case(TRAILER_TOKEN))
        .map(|(_, value)| value)
        .collect::<Vec<_>>();

    match values.as_slice() {
        [] => Ok(None),
        [value] => {
            let id = core::str::from_utf8(value)
                .map_err(|_| eyre!("gherrit-pr-id trailer is not valid UTF-8"))?
                .trim();
            if id.is_empty() {
                bail!("gherrit-pr-id trailer is empty");
            }
            validate(id)?;
            Ok(Some(id.to_string()))
        }
        _ => bail!("Commit contains multiple gherrit-pr-id trailers"),
    }
}

/// Parses the actual trailer block using Git's own `interpret-trailers`
/// implementation. This deliberately avoids matching lookalike lines in the
/// prose portion of the message.
pub fn from_message(message: &[u8]) -> Result<Option<String>> {
    let mut child = util::cmd("git", ["interpret-trailers", "--parse"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .wrap_err("Failed to run `git interpret-trailers --parse`")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(message)
        .wrap_err("Failed to send commit message to `git interpret-trailers`")?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`git interpret-trailers --parse` failed with status {}: {stderr}",
            output.status
        );
    }

    from_trailers(output.stdout.split(|byte| *byte == b'\n').filter_map(|line| {
        let separator = line.iter().position(|byte| *byte == b':')?;
        let token = &line[..separator];
        let value = line.get(separator + 1..)?.strip_prefix(b" ").unwrap_or(&line[separator + 1..]);
        Some((token, value))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_current_and_explicit_legacy_formats() {
        assert_eq!(
            validate("Gabcdefghijklmnopqrstuvwxyz234567").unwrap(),
            Format::CurrentBase32
        );
        assert_eq!(
            validate("G0000000000000000000000000000000000000001").unwrap(),
            Format::LegacyGHex
        );
        assert_eq!(
            validate("I0000000000000000000000000000000000000001").unwrap(),
            Format::LegacyChangeId
        );
    }

    #[test]
    fn rejects_branch_names_and_noncanonical_spellings() {
        for id in [
            "main",
            "master",
            "HEAD",
            "feature",
            "G12345",
            "GABCDEFGHIJKLMNOPQRSTUVWXYZ234567",
            "G000000000000000000000000000000000000000A",
            "Gabcdefghijklmnopqrstuvwxyz234567extra",
            "gabcdefghijklmnopqrstuvwxyz234567",
            "Iabcdefghijklmnopqrstuvwxyz234567",
        ] {
            assert!(validate(id).is_err(), "unexpectedly accepted {id}");
        }
    }

    #[test]
    fn requires_exactly_one_valid_parsed_trailer() {
        assert_eq!(
            from_trailers([(b"Other".as_slice(), b"value".as_slice())]).unwrap(),
            None
        );
        assert_eq!(
            from_trailers([(
                b"GHERRIT-PR-ID".as_slice(),
                b"Gabcdefghijklmnopqrstuvwxyz234567".as_slice(),
            )])
            .unwrap()
            .as_deref(),
            Some("Gabcdefghijklmnopqrstuvwxyz234567")
        );
        assert!(
            from_trailers([
                (b"gherrit-pr-id".as_slice(), b"Gabcdefghijklmnopqrstuvwxyz234567".as_slice()),
                (b"gherrit-pr-id".as_slice(), b"G234567abcdefghijklmnopqrstuvwxyz".as_slice()),
            ])
            .unwrap_err()
            .to_string()
            .contains("multiple")
        );
    }
}
