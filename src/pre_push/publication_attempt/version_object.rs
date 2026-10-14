//! Publication identity is distinct from the submitted commit's identity.
//!
//! A fresh nonce makes the next fixed version name an effective atomic fence:
//! another attempt cannot make our version create disappear as an equal-OID
//! no-op. A prepared action is transmitted once, never re-advertised/replayed.

use eyre::{Result, bail};
use gix::ObjectId;

use super::version::Version;
use crate::identity::GherritPrId;

pub(super) const MAX_VERSION_OBJECT_BYTES: usize = 512;

#[derive(Clone, Debug)]
pub(super) struct VersionObject {
    pub(super) oid: ObjectId,
    pub(super) bytes: Box<[u8]>,
}

impl VersionObject {
    pub(super) fn fresh(id: &GherritPrId, version: Version, head: ObjectId) -> Result<Self> {
        Self::with_nonce(id, version, head, rand::random())
    }

    fn with_nonce(
        id: &GherritPrId,
        version: Version,
        head: ObjectId,
        nonce: [u8; 32],
    ) -> Result<Self> {
        validate_target(head)?;
        let token = nonce.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        let bytes = format!(
            "object {head}\ntype commit\ntag gherrit/{}/v{version}\ntagger GHerrit <gherrit@invalid> 0 +0000\n\ngherrit-publication-v1 {token}\n", id.as_str()
        ).into_bytes();
        assert!(bytes.len() <= MAX_VERSION_OBJECT_BYTES);
        let oid = gix::objs::compute_hash(head.kind(), gix::object::Kind::Tag, &bytes)
            .expect("a bounded version tag fits Git's object format");
        Ok(Self { oid, bytes: bytes.into_boxed_slice() })
    }

    pub(super) fn validate(
        id: &GherritPrId,
        version: Version,
        head: ObjectId,
        bytes: &[u8],
    ) -> Result<()> {
        validate_target(head)?;
        if bytes.len() > MAX_VERSION_OBJECT_BYTES {
            bail!("Publication version object exceeds its byte limit");
        }
        let prefix = format!(
            "object {head}\ntype commit\ntag gherrit/{}/v{version}\ntagger GHerrit <gherrit@invalid> 0 +0000\n\ngherrit-publication-v1 ",
            id.as_str()
        );
        let token = bytes.strip_prefix(prefix.as_bytes()).and_then(|b| b.strip_suffix(b"\n"));
        if !token.is_some_and(|t| {
            t.len() == 64 && t.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        }) {
            bail!("Publication version object is not canonical for its ID, version, and revision");
        }
        Ok(())
    }
}

fn validate_target(head: ObjectId) -> Result<()> {
    if head.is_null() || head.kind() != gix::hash::Kind::Sha1 {
        bail!("Publication version target must be a non-null SHA-1 object ID");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_version_targets_are_rejected_at_both_boundaries() {
        let id = GherritPrId::from_ref_component(b"Gone").unwrap();
        let mut targets = vec![ObjectId::null(gix::hash::Kind::Sha1)];
        if let Ok(sha256) = ObjectId::from_hex(&[b'1'; 64]) {
            targets.push(sha256);
        }
        for head in targets {
            assert!(VersionObject::fresh(&id, Version::FIRST, head).is_err());
            let bytes = format!(
                "object {head}\ntype commit\ntag gherrit/Gone/v1\ntagger GHerrit <gherrit@invalid> 0 +0000\n\ngherrit-publication-v1 {}\n",
                "1".repeat(64)
            );
            assert!(VersionObject::validate(&id, Version::FIRST, head, bytes.as_bytes()).is_err());
        }
    }

    #[test]
    fn equal_revisions_have_distinct_publication_objects() {
        let id = GherritPrId::from_ref_component(b"Gone").unwrap();
        let head = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        let a = VersionObject::with_nonce(&id, Version::FIRST, head, [1; 32]).unwrap();
        let b = VersionObject::with_nonce(&id, Version::FIRST, head, [2; 32]).unwrap();
        assert_ne!(a.oid, b.oid);
        VersionObject::validate(&id, Version::FIRST, head, &a.bytes).unwrap();
        assert!(
            VersionObject::validate(&id, Version::FIRST.next().unwrap(), head, &a.bytes).is_err()
        );
        for byte in 0..=255 {
            let mut bad = a.bytes.to_vec();
            bad.push(byte);
            assert!(VersionObject::validate(&id, Version::FIRST, head, &bad).is_err());
        }
    }
}
