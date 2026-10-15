//! Inspection can observe a destination, but cannot build or execute plans.
use std::collections::{HashMap, HashSet};

use eyre::{Result, bail, eyre};
use serde_json::{Value, json};

use super::destination::PushDestination;
use crate::{hosted, identity::GherritPrId, util};

fn visit_refs(
    bytes: &[u8],
    ids: &[GherritPrId],
    mut accept: impl FnMut(&str, &str) -> Result<()>,
) -> Result<()> {
    let requested: HashSet<&[u8]> = ids.iter().map(|id| id.as_str().as_bytes()).collect();
    let mut seen = HashSet::new();
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        bail!("Incomplete inspection advertisement");
    }
    if bytes.is_empty() {
        return Ok(());
    }
    for line in bytes.strip_suffix(b"\n").unwrap_or(bytes).split(|byte| *byte == b'\n') {
        let tab = line
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| eyre!("Malformed inspection advertisement"))?;
        let (oid, name) = (&line[..tab], &line[tab + 1..]);
        // Positional patterns are tail filters. Filter exact byte namespaces
        // before decoding; an unrelated byte-valued ref is ordinary Git data.
        let selected = if let Some(head) = name.strip_prefix(b"refs/heads/") {
            requested.contains(head.strip_prefix(b"gherrit-bases/").unwrap_or(head))
        } else if let Some(tag) = name.strip_prefix(b"refs/tags/gherrit/") {
            requested.contains(tag)
                || tag
                    .iter()
                    .position(|byte| *byte == b'/')
                    .is_some_and(|slash| requested.contains(&tag[..slash]))
        } else {
            false
        };
        if name.ends_with(b"^{}") || !selected {
            continue;
        }
        let oid = std::str::from_utf8(oid)?;
        let name = std::str::from_utf8(name)?;
        if !matches!(oid.len(), 40 | 64)
            || oid.bytes().all(|byte| byte == b'0')
            || !oid.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!("Invalid inspection OID");
        }
        if !seen.insert(name.to_owned()) {
            bail!("Repeated inspection ref");
        }
        accept(name, oid)?;
    }
    Ok(())
}

const MAX_REF_BYTES: usize = 64 * 1024 * 1024;
const MAX_SELECTED_REFS: usize = 100_000;

#[derive(Default)]
struct RefObservation {
    bytes: usize,
    seen: HashMap<String, String>,
    refs: Vec<Value>,
}

impl RefObservation {
    fn append(&mut self, bytes: &[u8], ids: &[GherritPrId]) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .filter(|total| *total <= MAX_REF_BYTES)
            .ok_or_else(|| eyre!("Inspection ref output exceeds the aggregate byte budget"))?;
        visit_refs(bytes, ids, |name, oid| {
            if let Some(previous) = self.seen.get(name) {
                if previous != oid {
                    bail!("Inspection ref changed between query batches");
                }
                return Ok(());
            }
            if self.refs.len() == MAX_SELECTED_REFS {
                bail!("Inspection refs exceed the aggregate row budget");
            }
            self.seen.insert(name.to_owned(), oid.to_owned());
            self.refs.push(json!({"name":name,"oid":oid}));
            Ok(())
        })
    }
}

fn ref_queries(ids: &[GherritPrId], budget: usize) -> Result<Vec<Vec<String>>> {
    let mut queries = Vec::new();
    let mut current = Vec::new();
    let mut used = 0;
    for id in ids {
        let patterns = [
            crate::managed_refs::head(id),
            crate::managed_refs::owned_base(id),
            crate::managed_refs::tag_root(id),
            format!("{}/*", crate::managed_refs::tag_root(id)),
        ];
        let bytes: usize = patterns.iter().map(|pattern| pattern.len() + 1).sum();
        if bytes > budget {
            bail!("Inspection ID exceeds the query argument budget");
        }
        if used > budget - bytes {
            queries.push(std::mem::take(&mut current));
            used = 0;
        }
        current.extend(patterns);
        used += bytes;
    }
    if !current.is_empty() {
        queries.push(current);
    }
    Ok(queries)
}

pub(crate) async fn observe(repo: &util::Repo, ids: &[GherritPrId]) -> Result<Value> {
    let remote = repo.default_remote_name()?;
    let destination = PushDestination::resolve(repo, remote)?;
    if !destination.supports_production_github() {
        bail!("Remote inspection requires a github.com push repository");
    }
    // Plan every query before starting the first process, under a conservative
    // Windows/POSIX argv budget. IDs remain indivisible query units.
    let queries = ref_queries(ids, 16 * 1024)?;
    let refs = async {
        let mut refs = RefObservation::default();
        for patterns in queries {
            let output = destination.observe_refs_from(repo, patterns).await?;
            if !output.status().success() {
                bail!("Git ref inspection did not complete");
            }
            refs.append(output.stdout(), ids)?;
        }
        Ok::<_, eyre::Report>(refs.refs)
    }
    .await;
    let coordinates = destination.coordinates();
    let hosted = match hosted::observe(coordinates.owner(), coordinates.repository(), ids) {
        Ok(value) => value,
        Err(error) => json!({"state":"unavailable","reason":error.to_string()}),
    };
    let refs = match refs {
        Ok(refs) => {
            json!({"state":"observed","refs":refs,"archive_objects":"not acquired or validated"})
        }
        Err(error) => json!({"state":"unavailable","reason":error.to_string()}),
    };
    let complete = refs["state"] == "observed" && hosted["state"] == "observed";
    Ok(
        json!({"state":if complete{"observed"}else{"partial"},"repository":format!("{}/{}",coordinates.owner(),coordinates.repository()),"refs":refs,"github":hosted,"publication_authority":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn decode_refs(bytes: &[u8], ids: &[GherritPrId]) -> Result<Vec<Value>> {
        let mut refs = Vec::new();
        visit_refs(bytes, ids, |name, oid| {
            refs.push(json!({"name":name,"oid":oid}));
            Ok(())
        })?;
        Ok(refs)
    }

    #[test]
    fn ref_batches_share_one_budget_and_deduplicate_tail_overlap() {
        let ids = [
            GherritPrId::from_ref_component(b"A").unwrap(),
            GherritPrId::from_ref_component(b"B").unwrap(),
        ];
        let row = b"1111111111111111111111111111111111111111\trefs/tags/gherrit/A/refs/heads/B\n";
        let mut observation = RefObservation::default();
        observation.append(row, &ids).unwrap();
        observation.append(row, &ids).unwrap();
        assert_eq!(observation.refs.len(), 1);
        assert_eq!(observation.bytes, 2 * row.len());
        let conflict = String::from_utf8(row.to_vec()).unwrap().replacen('1', "2", 1);
        assert!(observation.append(conflict.as_bytes(), &ids).is_err());
        let mut bounded = RefObservation { bytes: MAX_REF_BYTES - row.len(), ..Default::default() };
        bounded.append(row, &ids).unwrap();
        assert!(bounded.append(row, &ids).is_err());
        let mut bounded = RefObservation::default();
        bounded.refs.resize(MAX_SELECTED_REFS, Value::Null);
        assert!(bounded.append(row, &ids).is_err());
        let mut input = row.to_vec();
        input.extend_from_slice(b"malformed later row\n");
        let error = bounded.append(&input, &ids).unwrap_err();
        assert!(error.to_string().contains("aggregate row budget"));
    }
    #[test]
    fn inspection_queries_bound_large_stacks_before_process_creation() {
        let ids = (0..1000)
            .map(|n| GherritPrId::from_ref_component(format!("G{n:032}").as_bytes()).unwrap())
            .collect::<Vec<_>>();
        let queries = ref_queries(&ids, 16 * 1024).unwrap();
        assert!(queries.len() > 1);
        assert!(
            queries
                .iter()
                .all(|query| query.iter().map(|arg| arg.len() + 1).sum::<usize>() <= 16 * 1024)
        );
        assert_eq!(queries.iter().map(Vec::len).sum::<usize>(), ids.len() * 4);
        assert!(ref_queries(&ids, 1).is_err());
    }

    #[test]
    fn unrelated_non_utf8_ref_tails_do_not_poison_inspection() {
        let id = GherritPrId::from_ref_component(b"Gone").unwrap();
        let input = b"1111111111111111111111111111111111111111\trefs/archive/\xff/refs/heads/Gone\n2222222222222222222222222222222222222222\trefs/heads/Gone\n";
        let refs = decode_refs(input, &[id]).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0]["name"], "refs/heads/Gone");
    }

    #[test]
    fn inspection_tail_filters_cannot_correlate_unrelated_names() {
        let id = GherritPrId::from_ref_component(b"Gone").unwrap();
        let input=b"1111111111111111111111111111111111111111\trefs/heads/Gone\n2222222222222222222222222222222222222222\trefs/heads/other/Gone\n3333333333333333333333333333333333333333\trefs/tags/gherrit/Gone/v1\n";
        let refs = decode_refs(input, &[id]).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0]["name"], "refs/heads/Gone");
        assert_eq!(refs[1]["name"], "refs/tags/gherrit/Gone/v1");
        assert!(decode_refs(&input[..input.len() - 1], &[]).is_err());
    }
    #[test]
    fn large_requested_id_index_selects_only_exact_namespaces() {
        let ids = (0..1000)
            .map(|n| GherritPrId::from_ref_component(format!("G{n}").as_bytes()).unwrap())
            .collect::<Vec<_>>();
        let mut bytes = Vec::new();
        for n in 0..1000 {
            for name in [
                format!("refs/heads/G{n}"),
                format!("refs/heads/gherrit-bases/G{n}"),
                format!("refs/tags/gherrit/G{n}/v1"),
            ] {
                bytes.extend_from_slice(format!("{}\t{name}\n", "1".repeat(40)).as_bytes());
            }
            bytes.extend_from_slice(
                format!("invalid\trefs/other/G{n}/refs/heads/G{n}\n").as_bytes(),
            );
        }
        assert_eq!(decode_refs(&bytes, &ids).unwrap().len(), 3000);
    }
    #[test]
    fn exact_tag_namespace_root_is_requested_and_retained_as_a_fact() {
        let id = GherritPrId::from_ref_component(b"Gone").unwrap();
        let queries = ref_queries(std::slice::from_ref(&id), 16 * 1024).unwrap();
        assert!(queries[0].contains(&"refs/tags/gherrit/Gone".to_owned()));
        let input = format!(
            "{}\trefs/tags/gherrit/Gone\n{}\trefs/tags/gherrit/GoneOther\n",
            "1".repeat(40),
            "2".repeat(40)
        );
        let refs = decode_refs(input.as_bytes(), &[id]).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0]["name"], "refs/tags/gherrit/Gone");
    }

    #[test]
    fn selected_refs_require_non_null_objects_in_either_hash_format() {
        let ids = [GherritPrId::from_ref_component(b"Gone").unwrap()];
        for width in [40, 64] {
            for null in [false, true] {
                let oid = if null { "0" } else { "1" }.repeat(width);
                let selected = format!("{oid}\trefs/heads/Gone\n");
                assert_eq!(decode_refs(selected.as_bytes(), &ids).is_ok(), !null);
                let unrelated = format!("{oid}\trefs/heads/Gother\n");
                assert!(decode_refs(unrelated.as_bytes(), &ids).unwrap().is_empty());
            }
        }
    }
}
