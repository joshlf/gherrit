//! Inspection can observe a destination, but cannot build or execute plans.
use std::collections::HashSet;

use eyre::{Result, bail, eyre};
use serde_json::{Value, json};

use super::destination::PushDestination;
use crate::{hosted, identity::GherritPrId, util};

fn decode_refs(bytes: &[u8], ids: &[GherritPrId]) -> Result<Vec<Value>> {
    let heads: HashSet<_> = ids.iter().map(|id| format!("refs/heads/{}", id.as_str())).collect();
    let bases: HashSet<_> =
        ids.iter().map(|id| format!("refs/heads/gherrit-bases/{}", id.as_str())).collect();
    let prefixes: Vec<_> =
        ids.iter().map(|id| format!("refs/tags/gherrit/{}/", id.as_str())).collect();
    let mut seen = HashSet::new();
    let mut refs = Vec::new();
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        bail!("Incomplete inspection advertisement");
    }
    if bytes.is_empty() {
        return Ok(refs);
    }
    for line in bytes.strip_suffix(b"\n").unwrap_or(bytes).split(|byte| *byte == b'\n') {
        let tab = line
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| eyre!("Malformed inspection advertisement"))?;
        let (oid, name) = (&line[..tab], &line[tab + 1..]);
        // Positional patterns are tail filters. Filter exact byte namespaces
        // before decoding; an unrelated byte-valued ref is ordinary Git data.
        if name.ends_with(b"^{}")
            || !(heads.iter().any(|head| head.as_bytes() == name)
                || bases.iter().any(|base| base.as_bytes() == name)
                || prefixes.iter().any(|prefix| name.starts_with(prefix.as_bytes())))
        {
            continue;
        }
        let oid = std::str::from_utf8(oid)?;
        let name = std::str::from_utf8(name)?;
        if !matches!(oid.len(), 40 | 64)
            || !oid.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!("Invalid inspection OID");
        }
        if !seen.insert(name.to_owned()) {
            bail!("Repeated inspection ref");
        }
        refs.push(json!({"name":name,"oid":oid}));
    }
    Ok(refs)
}

fn ref_queries(ids: &[GherritPrId], budget: usize) -> Result<Vec<Vec<String>>> {
    let mut queries = Vec::new();
    let mut current = Vec::new();
    let mut used = 0;
    for id in ids {
        let patterns = [
            format!("refs/heads/{}", id.as_str()),
            format!("refs/heads/gherrit-bases/{}", id.as_str()),
            format!("refs/tags/gherrit/{}/*", id.as_str()),
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
        let mut refs = Vec::new();
        for patterns in queries {
            let output = destination.observe_refs_from(repo, patterns).await?;
            if !output.status().success() {
                bail!("Git ref inspection did not complete");
            }
            refs.extend(decode_refs(output.stdout(), ids)?);
        }
        Ok::<_, eyre::Report>(refs)
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
        assert_eq!(queries.iter().map(Vec::len).sum::<usize>(), ids.len() * 3);
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
}
