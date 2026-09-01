//! Canonical immutable pull-request marker tag objects.
//!
//! A marker names one immutable v1 revision and one GitHub pull-request
//! number.  The tag object's bytes are its identity: the same facts always
//! render to the same SHA-1 object, while a different number cannot silently
//! satisfy the same create-only ref lease.

use color_eyre::eyre::{Result, bail, eyre};
use gix::{ObjectId, prelude::Write as _};

use super::github::PullRequestNumber;
use crate::pre_push::local::GherritPrId;

/// Deliberately small: the canonical form is under 200 bytes for every valid
/// GraphQL-safe number.  The bound rejects object-database abuse before any
/// parsing or allocation based on untrusted tag bytes.
pub(super) const MAX_TAG_BYTES: usize = 512;

const TAGGER: &str = "GHerrit <gherrit@invalid> 0 +0000";
const MESSAGE_PREFIX: &str = "gherrit-canonical-pr-v1 ";

/// The semantic payload of one validated marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PullRequestMarker {
    number: PullRequestNumber,
}

impl PullRequestMarker {
    pub(super) fn new(number: PullRequestNumber) -> Self {
        Self { number }
    }

    pub(super) fn number(self) -> PullRequestNumber {
        self.number
    }

    /// Renders the complete canonical tag-object payload, excluding Git's
    /// object header.  Only SHA-1 repositories are supported by the remote
    /// protocol, so accepting any other object format here would make the
    /// advertised object grammar ambiguous.
    pub(super) fn encode(self, id: &GherritPrId, v1: ObjectId) -> Result<Vec<u8>> {
        if v1.is_null() || v1.kind() != gix::hash::Kind::Sha1 {
            bail!("Pull-request marker v1 target must be a non-null SHA-1 object ID");
        }
        let bytes = format!(
            "object {v1}\ntype commit\ntag gherrit/{}/pr\ntagger {TAGGER}\n\n{MESSAGE_PREFIX}{}\n",
            id.as_str(),
            self.number.get(),
        )
        .into_bytes();
        debug_assert!(bytes.len() <= MAX_TAG_BYTES);
        Ok(bytes)
    }

    /// Decodes only the one exact byte grammar GHerrit writes.  Parsing the
    /// number then comparing a regenerated payload rejects alternate header
    /// order, whitespace, duplicate headers, noncanonical decimal spellings,
    /// and message framing tricks in one place.
    pub(super) fn decode(id: &GherritPrId, v1: ObjectId, bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_TAG_BYTES {
            bail!("Pull-request marker tag object exceeds the {MAX_TAG_BYTES}-byte limit");
        }
        let prefix = format!(
            "object {v1}\ntype commit\ntag gherrit/{}/pr\ntagger {TAGGER}\n\n{MESSAGE_PREFIX}",
            id.as_str()
        );
        let Some(number) =
            bytes.strip_prefix(prefix.as_bytes()).and_then(|tail| tail.strip_suffix(b"\n"))
        else {
            bail!("Pull-request marker tag object does not use GHerrit's canonical v1 framing");
        };
        if number.is_empty() || !number.iter().all(u8::is_ascii_digit) {
            bail!("Pull-request marker tag object has a non-decimal pull-request number");
        }
        if number[0] == b'0' {
            bail!("Pull-request marker tag object has a noncanonical pull-request number");
        }
        let number = std::str::from_utf8(number)
            .expect("ASCII decimal input is UTF-8")
            .parse::<u64>()
            .map_err(|_| {
                eyre!("Pull-request marker tag object has an overflowing pull-request number")
            })?;
        let marker = Self::new(PullRequestNumber::new(number)?);
        if marker.encode(id, v1)? != bytes {
            bail!("Pull-request marker tag object is not byte-for-byte canonical");
        }
        Ok(marker)
    }
}

/// A marker selected by validated history plus an exact GitHub identity, but
/// not yet written to the local object database. Keeping the bytes implicit
/// prevents a planner from publishing a ref to a marker created before all
/// relevant preflight and create-receipt checks complete.
#[derive(Clone, Debug)]
pub(super) struct MarkerTemplate {
    id: GherritPrId,
    v1: ObjectId,
    marker: PullRequestMarker,
}

/// A canonical marker object whose exact object ID is known without writing
/// to the local object database.
#[derive(Clone, Debug)]
pub(super) struct PreparedMarker {
    id: GherritPrId,
    tag: ObjectId,
    bytes: Box<[u8]>,
}

impl MarkerTemplate {
    pub(super) fn new(id: GherritPrId, v1: ObjectId, number: PullRequestNumber) -> Result<Self> {
        if v1.is_null() || v1.kind() != gix::hash::Kind::Sha1 {
            bail!("Pull-request marker v1 target must be a non-null SHA-1 object ID");
        }
        Ok(Self { id, v1, marker: PullRequestMarker::new(number) })
    }

    /// Computes the exact tag object and ID without touching the object
    /// database. Marker ref destinations and command sizes can therefore be
    /// preflighted before local materialization.
    pub(super) fn prepare(self) -> Result<PreparedMarker> {
        let bytes = self.marker.encode(&self.id, self.v1)?;
        let tag = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::object::Kind::Tag, &bytes)
            .map_err(|error| {
                eyre!("Failed to compute the pull-request marker object ID: {error}")
            })?;
        Ok(PreparedMarker { id: self.id, tag, bytes: bytes.into_boxed_slice() })
    }

    #[cfg(test)]
    pub(super) fn test_parts(&self) -> (&GherritPrId, ObjectId, PullRequestNumber) {
        (&self.id, self.v1, self.marker.number())
    }
}

impl PreparedMarker {
    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn tag(&self) -> ObjectId {
        self.tag
    }

    /// Writes the already-preflighted object and verifies that the repository
    /// assigned the exact ID used to prepare the marker push.
    pub(super) fn materialize(self, repository: &crate::util::Repo) -> Result<()> {
        if repository.object_hash() != gix::hash::Kind::Sha1 {
            bail!("GHerrit pull-request markers require a SHA-1 repository");
        }
        let written =
            repository.write_buf(gix::object::Kind::Tag, &self.bytes).map_err(|error| {
                eyre!("Failed to materialize the pull-request marker tag object: {error}")
            })?;
        if written != self.tag {
            bail!("Materialized pull-request marker has a different object ID than preflight");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> GherritPrId {
        GherritPrId::from_ref_component(b"Gone").unwrap()
    }

    fn v1() -> ObjectId {
        ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap()
    }

    #[test]
    fn canonical_codec_round_trips_every_number_boundary() {
        for number in [1, 7, i32::MAX as u32] {
            let marker = PullRequestMarker::new(PullRequestNumber::for_test(number));
            let bytes = marker.encode(&id(), v1()).unwrap();
            assert_eq!(PullRequestMarker::decode(&id(), v1(), &bytes).unwrap(), marker);
        }
    }

    #[test]
    fn codec_rejects_wrong_target_and_noncanonical_framing() {
        let marker = PullRequestMarker::new(PullRequestNumber::for_test(7));
        let bytes = marker.encode(&id(), v1()).unwrap();
        let wrong = ObjectId::from_hex(b"2222222222222222222222222222222222222222").unwrap();
        assert!(PullRequestMarker::decode(&id(), wrong, &bytes).is_err());
        for malformed in [
            b"object 1111111111111111111111111111111111111111\ntype commit\ntag gherrit/Gone/pr\ntagger GHerrit <gherrit@invalid> 0 +0000\n\ngherrit-canonical-pr-v1 07\n".as_slice(),
            b"object 1111111111111111111111111111111111111111\ntype commit\ntag gherrit/Gone/pr\ntagger GHerrit <gherrit@invalid> 0 +0000\n\ngherrit-canonical-pr-v1 7\nextra\n".as_slice(),
        ] {
            assert!(PullRequestMarker::decode(&id(), v1(), malformed).is_err());
        }
    }

    #[test]
    fn codec_rejects_oversized_objects_before_parsing() {
        let oversized = vec![b'x'; MAX_TAG_BYTES + 1];
        assert!(PullRequestMarker::decode(&id(), v1(), &oversized).is_err());
    }

    #[test]
    fn preparation_is_deterministic_and_materialization_writes_the_exact_object() {
        let directory = tempfile::tempdir().unwrap();
        gix::init_bare(directory.path()).unwrap();
        let repository = crate::util::Repo::open(directory.path().to_str().unwrap()).unwrap();

        let first = MarkerTemplate::new(id(), v1(), PullRequestNumber::for_test(7))
            .unwrap()
            .prepare()
            .unwrap();
        let same = MarkerTemplate::new(id(), v1(), PullRequestNumber::for_test(7))
            .unwrap()
            .prepare()
            .unwrap();
        let different = MarkerTemplate::new(id(), v1(), PullRequestNumber::for_test(8))
            .unwrap()
            .prepare()
            .unwrap();

        assert_eq!(first.tag(), same.tag());
        assert_ne!(first.tag(), different.tag());
        assert_eq!(first.id(), &id());
        assert!(repository.try_find_header(first.tag()).unwrap().is_none());
        let tag = first.tag();
        first.materialize(&repository).unwrap();
        assert_eq!(
            repository.try_find_header(tag).unwrap().unwrap().kind(),
            gix::object::Kind::Tag
        );
    }
}
