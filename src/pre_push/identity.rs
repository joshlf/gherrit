//! Keeps change IDs after checking their committed trailers.
//!
//! These checks are separate from the commit hook, which preserves any existing
//! trailer Git recognizes, even one the publisher must reject.

use std::{borrow::Borrow, fmt};

/// A validated, nonempty ID made of ASCII letters and digits.
///
/// The ID keeps its spelling across commit rewrites. The existing format has no
/// required prefix or additional length rule.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ChangeId(String);

impl ChangeId {
    pub fn parse(value: &[u8]) -> Option<Self> {
        if value.is_empty() || !value.iter().all(u8::is_ascii_alphanumeric) {
            return None;
        }
        Some(Self(String::from_utf8(value.to_vec()).expect("ASCII ID bytes are UTF-8")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(self.as_str())
    }
}

impl Borrow<str> for ChangeId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_legacy_spelling_without_imposing_a_new_prefix() {
        for text in ["Gabc123", "lowercase", "0123", "A", "aZ09"] {
            let id = ChangeId::parse(text.as_bytes()).unwrap();
            assert_eq!(id.as_str(), text);
            assert_eq!(id.to_string(), text);
        }
    }

    #[test]
    fn rejects_empty_and_every_non_alphanumeric_byte() {
        assert!(ChangeId::parse(b"").is_none());
        for byte in 0..=u8::MAX {
            let accepted = matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9');
            assert_eq!(ChangeId::parse(&[byte]).is_some(), accepted, "byte={byte}");
            if !accepted {
                assert!(ChangeId::parse(&[b'G', byte, b'1']).is_none());
            }
        }
    }

    #[test]
    fn borrowed_lookup_preserves_case_sensitive_identity() {
        let ids = std::collections::HashMap::from([
            (ChangeId::parse(b"Gone").unwrap(), 1),
            (ChangeId::parse(b"gone").unwrap(), 2),
        ]);
        assert_eq!(ids.get("Gone"), Some(&1));
        assert_eq!(ids.get("gone"), Some(&2));
        assert_eq!(ids.get("GONE"), None);
    }
}
