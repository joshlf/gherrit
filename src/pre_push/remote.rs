//! Complete destination-scoped publication observations.
//!
//! A missing ref is meaningful only when its exact namespace was queried.
//! This adapter therefore observes each requested managed head together with
//! its complete remote version history and exposes only normalized states.

use std::{
    collections::{BTreeMap, HashMap, HashSet, hash_map::Entry},
    process::Stdio,
};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::{ObjectId, bstr::ByteSlice as _};

use super::destination::{PushDestination, git_output_records};

// Variable arguments stay well below Windows' roughly 32-KiB command-line
// limit. This also gives POSIX implementations a conservative bound.
const QUERY_ARGV_BUDGET_BYTES: usize = 16 * 1024;
const HEAD_PREFIX: &str = "refs/heads/";
const VERSION_PREFIX: &str = "refs/tags/gherrit/";

/// One validated destination state for a requested GHerrit change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RemotePublication {
    Absent,
    Published { head: ObjectId, latest_version: usize },
}

/// Observes the managed head and complete remote version history for every
/// requested change, in request order.
///
/// Every query is planned before the first network request. An oversized or
/// duplicate ID late in the input therefore cannot fail after an earlier
/// prefix was observed.
pub(super) fn observe_publications(
    destination: &PushDestination,
    gherrit_ids: &[String],
) -> Result<Vec<RemotePublication>> {
    let queries = plan_queries(gherrit_ids)?;

    queries.into_iter().try_fold(Vec::with_capacity(gherrit_ids.len()), |mut observed, query| {
        let mut command = destination.ls_remote(["--quiet".to_owned()], query.patterns());
        let output = command.stderr(Stdio::null()).output().wrap_err_with(|| {
            format!(
                "Failed to observe GHerrit publication state at remote '{}'",
                destination.configured_remote()
            )
        })?;
        if !output.status.success() {
            bail!(
                "`git ls-remote` failed while observing GHerrit publication state at remote '{}'",
                destination.configured_remote()
            );
        }

        observed.extend(parse_publications(&output.stdout, query.ids())?);

        Ok(observed)
    })
}

#[derive(Debug, Eq, PartialEq)]
struct Query {
    ids: Vec<String>,
}

impl Query {
    fn new(first: String) -> Self {
        Self { ids: vec![first] }
    }

    fn ids(&self) -> &[String] {
        &self.ids
    }

    fn patterns(&self) -> Vec<String> {
        self.ids.iter().flat_map(|id| publication_patterns(id)).collect()
    }
}

fn plan_queries(ids: &[String]) -> Result<Vec<Query>> {
    plan_queries_with_budget(ids, QUERY_ARGV_BUDGET_BYTES)
}

fn plan_queries_with_budget(ids: &[String], budget: usize) -> Result<Vec<Query>> {
    let mut seen = HashSet::new();
    let planned = ids
        .iter()
        .map(|id| {
            if !seen.insert(id.as_str()) {
                bail!("remote observation requested GHerrit change '{id}' more than once");
            }
            let bytes = publication_pattern_bytes(id);
            if bytes > budget {
                bail!(
                    "GHerrit change ID is too long for a remote observation query ({} bytes)",
                    id.len()
                );
            }
            Ok((id.clone(), bytes))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut queries = Vec::new();
    let mut current = None::<Query>;
    let mut current_bytes = 0;
    for (id, bytes) in planned {
        if current.is_some() && current_bytes > budget - bytes {
            queries.push(current.take().expect("a full query exists"));
            current_bytes = 0;
        }
        current_bytes += bytes;
        match &mut current {
            Some(query) => query.ids.push(id),
            None => current = Some(Query::new(id)),
        }
    }
    if let Some(query) = current {
        queries.push(query);
    }
    Ok(queries)
}

fn publication_patterns(id: &str) -> [String; 3] {
    let version_root = format!("{VERSION_PREFIX}{id}");
    [format!("{HEAD_PREFIX}{id}"), version_root.clone(), format!("{version_root}/*")]
}

fn publication_pattern_bytes(id: &str) -> usize {
    publication_patterns(id).iter().map(|pattern| pattern.len() + 1).sum()
}

struct Record<'a> {
    object_id: ObjectId,
    name: &'a [u8],
    peeled: bool,
}

fn records(output: &[u8]) -> impl Iterator<Item = Result<Record<'_>>> {
    git_output_records(output).map(|record| {
        let mut fields = record.split(|byte| *byte == b'\t');
        let (Some(value), Some(name), None) = (fields.next(), fields.next(), fields.next()) else {
            bail!("malformed `git ls-remote` record: {record:?}");
        };
        if value.starts_with(b"ref: ") {
            bail!("remote publication observation unexpectedly contained a symbolic ref");
        }

        let (logical_name, peeled) = match name.strip_suffix(b"^{}") {
            Some(name) => (name, true),
            None => (name, false),
        };
        let full_name = gix::refs::FullName::try_from(logical_name.as_bstr())
            .wrap_err("remote ref has an invalid name")?;
        if peeled && full_name.category() != Some(gix::refs::Category::Tag) {
            bail!("peeled remote ref is not a tag");
        }

        let object_id =
            ObjectId::from_hex(value).wrap_err("remote ref value is not an object ID")?;
        if object_id.is_null() {
            bail!("remote ref has a null object ID");
        }
        Ok(Record { object_id, name: logical_name, peeled })
    })
}

#[derive(Default)]
struct RawPublication {
    head: Option<ObjectId>,
    versions: BTreeMap<usize, ObjectId>,
}

fn parse_publications(output: &[u8], requested_ids: &[String]) -> Result<Vec<RemotePublication>> {
    let mut raw = requested_ids.iter().try_fold(HashMap::new(), |mut raw, id| {
        match raw.entry(id.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(RawPublication::default());
            }
            Entry::Occupied(_) => {
                bail!("remote observation requested GHerrit change '{id}' more than once");
            }
        }
        Ok(raw)
    })?;
    let requested_heads = requested_ids
        .iter()
        .map(|id| (format!("{HEAD_PREFIX}{id}").into_bytes(), id.as_str()))
        .collect::<HashMap<_, _>>();
    let requested_versions = requested_ids
        .iter()
        .map(|id| (id.as_bytes().to_vec(), id.as_str()))
        .collect::<HashMap<_, _>>();

    for record in records(output) {
        let record = record?;
        if let Some(id) = requested_heads.get(record.name) {
            if record.peeled {
                bail!("managed head for GHerrit change '{id}' was advertised as a peeled tag");
            }
            if raw
                .get_mut(*id)
                .expect("requested changes have initialized observations")
                .head
                .replace(record.object_id)
                .is_some()
            {
                bail!(
                    "remote advertised the managed head for GHerrit change '{id}' more than once"
                );
            }
            continue;
        }

        let Some(suffix) = record.name.strip_prefix(VERSION_PREFIX.as_bytes()) else {
            // `ls-remote` patterns also match an arbitrary ref whose tail is
            // one of the requested names. Such a ref is outside the exact
            // managed namespace and carries no publication evidence.
            continue;
        };
        let Some(separator) = suffix.iter().position(|byte| *byte == b'/') else {
            if let Some(id) = requested_versions.get(suffix) {
                bail!("remote version namespace root exists for GHerrit change '{id}'");
            }
            continue;
        };
        let (id_component, version) = suffix.split_at(separator);
        let Some(id) = requested_versions.get(id_component) else {
            continue;
        };
        if record.peeled {
            bail!(
                "remote version tag for GHerrit change '{id}' is annotated rather than lightweight"
            );
        }
        let version = version
            .strip_prefix(b"/")
            .ok_or_else(|| eyre!("remote version tag for GHerrit change '{id}' has no version"))?;
        let version = parse_version(version).wrap_err_with(|| {
            format!("remote version tag for GHerrit change '{id}' is not canonical")
        })?;
        if raw
            .get_mut(*id)
            .expect("requested changes have initialized observations")
            .versions
            .insert(version, record.object_id)
            .is_some()
        {
            bail!("remote advertised version v{version} for GHerrit change '{id}' more than once");
        }
    }

    requested_ids
        .iter()
        .map(|id| {
            let raw = raw.remove(id).expect("requested changes have initialized observations");
            normalize_publication(id, raw)
        })
        .collect()
}

fn parse_version(suffix: &[u8]) -> Result<usize> {
    let digits = suffix.strip_prefix(b"v").ok_or_else(|| eyre!("missing 'v' prefix"))?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        bail!("version is not decimal");
    }
    if digits[0] == b'0' {
        bail!("version is zero or has a leading zero");
    }
    digits.iter().try_fold(0_usize, |value, digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(usize::from(*digit - b'0')))
            .ok_or_else(|| eyre!("version overflows usize"))
    })
}

fn normalize_publication(id: &str, raw: RawPublication) -> Result<RemotePublication> {
    match (raw.head, raw.versions.is_empty()) {
        (None, true) => return Ok(RemotePublication::Absent),
        (Some(_), true) => {
            bail!("Remote GHerrit change '{id}' has a managed head but no version tags")
        }
        (None, false) => {
            bail!("Remote GHerrit change '{id}' has version tags but no managed head")
        }
        (Some(_), false) => {}
    }

    raw.versions.iter().enumerate().try_for_each(|(index, (actual, _))| {
        let expected = index
            .checked_add(1)
            .ok_or_else(|| eyre!("Remote GHerrit change '{id}' has too many versions"))?;
        if *actual != expected {
            bail!(
                "Remote GHerrit change '{id}' has noncontiguous version tags: expected v{expected}, observed v{actual}"
            );
        }
        Ok(())
    })?;
    let (&latest_version, &latest_head) = raw
        .versions
        .last_key_value()
        .ok_or_else(|| eyre!("Remote GHerrit change '{id}' has no version tags"))?;
    let head = raw.head.expect("nonempty publication has a managed head");
    if head != latest_head {
        bail!("Remote GHerrit change '{id}' head does not match its latest version tag");
    }

    Ok(RemotePublication::Published { head, latest_version })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: &str = "1111111111111111111111111111111111111111";
    const TWO: &str = "2222222222222222222222222222222222222222";
    const THREE: &str = "3333333333333333333333333333333333333333";

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn object_id(value: &str) -> ObjectId {
        ObjectId::from_hex(value.as_bytes()).unwrap()
    }

    #[test]
    fn parses_complete_destination_histories_and_authoritative_absence() {
        let requested = ids(&["Gone", "Gtwo", "Gabsent"]);
        let mut output = format!(
            "{ONE}\trefs/heads/Gone\n\
             {ONE}\trefs/tags/gherrit/Gone/v2\n\
             {TWO}\trefs/tags/gherrit/Gone/v1\n\
             {THREE}\trefs/heads/Gtwo\n\
             {THREE}\trefs/tags/gherrit/Gtwo/v1\n\
             {TWO}\trefs/heads/archive/refs/heads/Gone\n\
             {TWO}\trefs/tags/archive/refs/tags/gherrit/Gone/v3\n"
        )
        .into_bytes();
        output.extend_from_slice(format!("{TWO}\trefs/heads/archive/").as_bytes());
        output.extend_from_slice(b"\xff/refs/heads/Gone\n");

        assert_eq!(
            parse_publications(&output, &requested).unwrap(),
            [
                RemotePublication::Published { head: object_id(ONE), latest_version: 2 },
                RemotePublication::Published { head: object_id(THREE), latest_version: 1 },
                RemotePublication::Absent,
            ]
        );
        assert_eq!(parse_publications(b"", &requested).unwrap(), [RemotePublication::Absent; 3]);
    }

    #[test]
    fn accepts_contiguous_history_with_repeated_objects() {
        let requested = ids(&["Gone"]);
        let output = format!(
            "{TWO}\trefs/heads/Gone\n\
             {ONE}\trefs/tags/gherrit/Gone/v1\n\
             {TWO}\trefs/tags/gherrit/Gone/v3\n\
             {TWO}\trefs/tags/gherrit/Gone/v2\n"
        );

        assert_eq!(
            parse_publications(output.as_bytes(), &requested).unwrap()[0],
            RemotePublication::Published { head: object_id(TWO), latest_version: 3 }
        );
    }

    #[test]
    fn rejects_partial_gapped_and_mismatched_publications() {
        let requested = ids(&["Gone"]);
        for (output, message) in [
            (format!("{ONE}\trefs/heads/Gone\n"), "head but no version tags"),
            (format!("{ONE}\trefs/tags/gherrit/Gone/v1\n"), "tags but no managed head"),
            (
                format!(
                    "{TWO}\trefs/heads/Gone\n\
                     {ONE}\trefs/tags/gherrit/Gone/v1\n\
                     {TWO}\trefs/tags/gherrit/Gone/v3\n"
                ),
                "noncontiguous version tags",
            ),
            (
                format!(
                    "{TWO}\trefs/heads/Gone\n\
                     {ONE}\trefs/tags/gherrit/Gone/v1\n"
                ),
                "does not match its latest version tag",
            ),
        ] {
            let error = parse_publications(output.as_bytes(), &requested).unwrap_err();
            assert!(error.to_string().contains(message), "error={error:?}");
        }
    }

    #[test]
    fn rejects_duplicate_annotated_and_noncanonical_managed_refs() {
        let requested = ids(&["Gone"]);
        for output in [
            format!("{ONE}\trefs/tags/gherrit/Gone\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v0\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v01\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v1/extra\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v{}0\n", usize::MAX),
            format!("{ONE}\trefs/heads/Gone\n{TWO}\trefs/heads/Gone\n"),
            format!(
                "{ONE}\trefs/tags/gherrit/Gone/v1\n\
                 {TWO}\trefs/tags/gherrit/Gone/v1\n"
            ),
            format!(
                "{ONE}\trefs/tags/gherrit/Gone/v1\n\
                 {TWO}\trefs/tags/gherrit/Gone/v1^{{}}\n"
            ),
        ] {
            assert!(parse_publications(output.as_bytes(), &requested).is_err(), "{output:?}");
        }
    }

    #[test]
    fn rejects_every_untrusted_record_shape() {
        let requested = ids(&["Gone"]);
        for output in [
            b"\n".as_slice(),
            b"not a record\n",
            b"xyz\trefs/heads/unrelated\n",
            b"0000000000000000000000000000000000000000\trefs/heads/unrelated\n",
            b"ref: refs/heads/main\trefs/heads/unrelated\n",
        ] {
            assert!(parse_publications(output, &requested).is_err(), "{output:?}");
        }
    }

    #[test]
    fn query_planning_uses_exact_patterns_and_preflights_every_id() {
        let requested = ids(&["Gone", "Gtwo"]);
        let one = publication_pattern_bytes(&requested[0]);
        let two = publication_pattern_bytes(&requested[1]);
        let split = plan_queries_with_budget(&requested, one + two - 1).unwrap();

        assert_eq!(split.len(), 2);
        assert_eq!(
            split[0].patterns(),
            ["refs/heads/Gone", "refs/tags/gherrit/Gone", "refs/tags/gherrit/Gone/*",]
        );
        assert_eq!(
            split[1].patterns(),
            ["refs/heads/Gtwo", "refs/tags/gherrit/Gtwo", "refs/tags/gherrit/Gtwo/*",]
        );
        assert!(plan_queries_with_budget(&requested, one - 1).is_err());
        assert!(plan_queries_with_budget(&ids(&["Gone", "Gone"]), one * 2).is_err());
        assert!(plan_queries_with_budget(&[], one).unwrap().is_empty());
    }
}
