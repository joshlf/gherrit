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
        let line = std::str::from_utf8(line)?;
        let (oid, name) =
            line.split_once('\t').ok_or_else(|| eyre!("Malformed inspection advertisement"))?;
        if !matches!(oid.len(), 40 | 64)
            || !oid.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!("Invalid inspection OID");
        }
        // Git's positional patterns are tail filters. Unexpected names and
        // peeled tag lines are never correlated to a local change.
        if name.ends_with("^{}") {
            continue;
        }
        if !(heads.contains(name)
            || bases.contains(name)
            || prefixes.iter().any(|prefix| name.starts_with(prefix)))
        {
            continue;
        }
        if !seen.insert(name) {
            bail!("Repeated inspection ref");
        }
        refs.push(json!({"name":name,"oid":oid}));
    }
    Ok(refs)
}

pub(crate) async fn observe(repo: &util::Repo, ids: &[GherritPrId]) -> Result<Value> {
    let remote = repo.default_remote_name()?;
    let destination = PushDestination::resolve(repo, remote)?;
    if !destination.supports_production_github() {
        bail!("Remote inspection requires a github.com push repository");
    }
    let patterns = ids.iter().flat_map(|id| {
        [
            format!("refs/heads/{}", id.as_str()),
            format!("refs/heads/gherrit-bases/{}", id.as_str()),
            format!("refs/tags/gherrit/{}/*", id.as_str()),
        ]
    });
    let refs = if ids.is_empty() {
        Ok(Vec::new())
    } else {
        match destination.observe_refs_from(repo, patterns).await {
            Ok(output) if output.status().success() => decode_refs(output.stdout(), ids),
            _ => Err(eyre!("Git ref inspection did not complete")),
        }
    };
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
