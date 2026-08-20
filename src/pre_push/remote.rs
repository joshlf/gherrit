use std::{
    collections::{BTreeMap, HashMap},
    num::NonZeroUsize,
    str,
};

use color_eyre::eyre::{Context as _, Result, bail};
use gix::ObjectId;

use super::publication::{RemoteHistory, RemotePublication, parse_version, remote_query_batches};
use crate::util::{self, CommandExt as _};

#[derive(Default)]
struct RawPublication {
    branch: Option<ObjectId>,
    versions: BTreeMap<NonZeroUsize, ObjectId>,
}

/// Observes each managed branch and its complete remote version history.
pub(super) fn observe_publications(
    repo: &util::Repo,
    gherrit_ids: &[&str],
) -> Result<Vec<RemotePublication>> {
    remote_query_batches(gherrit_ids).try_fold(
        Vec::with_capacity(gherrit_ids.len()),
        |mut publications, chunk| {
            let mut arguments =
                vec!["ls-remote".to_string(), "--refs".to_string(), repo.default_remote_name()];
            arguments.extend(
                chunk.iter().flat_map(|id| {
                    [format!("refs/heads/{id}"), format!("refs/tags/gherrit/{id}/v*")]
                }),
            );

            let output = util::cmd("git", arguments).checked_output()?;
            publications.extend(parse_publications(&output.stdout, chunk)?);
            Ok(publications)
        },
    )
}

/// Parses and validates authoritative publication state from `git ls-remote`.
///
/// Valid published state has a contiguous, nonempty version history beginning
/// at v1, and the managed branch points to the latest version tag. An absent
/// branch and absent history represent a change that has never been published.
fn parse_publications(output: &[u8], requested_ids: &[&str]) -> Result<Vec<RemotePublication>> {
    let requested = requested_ids.iter().enumerate().try_fold(
        HashMap::with_capacity(requested_ids.len()),
        |mut requested, (index, id)| {
            if requested.insert(id.as_bytes(), index).is_some() {
                bail!("Publication ref queries must be unique");
            }
            Ok(requested)
        },
    )?;
    let mut publications =
        (0..requested_ids.len()).map(|_| RawPublication::default()).collect::<Vec<_>>();

    if !output.is_empty() {
        let output = output.strip_suffix(b"\n").unwrap_or(output);
        output.split(|byte| *byte == b'\n').try_for_each(|line| {
            let mut fields = line.split(|byte| *byte == b'\t');
            let (Some(object_id), Some(ref_name), None) =
                (fields.next(), fields.next(), fields.next())
            else {
                bail!("malformed `git ls-remote` line: {line:?}");
            };
            if ref_name.is_empty() {
                bail!("malformed `git ls-remote` line: {line:?}");
            }
            let object_id = ObjectId::from_hex(object_id)
                .wrap_err_with(|| format!("invalid object ID in `git ls-remote` line: {line:?}"))?;

            if let Some(id) = ref_name.strip_prefix(b"refs/heads/") {
                if let Some(&index) = requested.get(id)
                    && publications[index].branch.replace(object_id).is_some()
                {
                    bail!(
                        "`git ls-remote` reported {} more than once",
                        String::from_utf8_lossy(ref_name)
                    );
                }
                return Ok(());
            }

            let Some(tag) = ref_name.strip_prefix(b"refs/tags/gherrit/") else {
                return Ok(());
            };
            let Some(separator) = tag.iter().position(|byte| *byte == b'/') else {
                return Ok(());
            };
            let (id, suffix) = (&tag[..separator], &tag[separator + 1..]);
            let Some(&index) = requested.get(id) else {
                return Ok(());
            };
            let Some(version) = suffix.strip_prefix(b"v") else {
                return Ok(());
            };
            let version = str::from_utf8(version).wrap_err_with(|| {
                format!(
                    "Malformed remote GHerrit version tag: {}",
                    String::from_utf8_lossy(ref_name)
                )
            })?;
            let version = parse_version(version).ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "Malformed remote GHerrit version tag: {}",
                    String::from_utf8_lossy(ref_name)
                )
            })?;
            if publications[index].versions.insert(version, object_id).is_some() {
                bail!(
                    "`git ls-remote` reported {} more than once",
                    String::from_utf8_lossy(ref_name)
                );
            }
            Ok(())
        })?;
    }

    publications
        .into_iter()
        .zip(requested_ids)
        .map(|(publication, id)| match (publication.branch, publication.versions.last_key_value()) {
            (None, None) => Ok(RemotePublication::Unpublished),
            (None, Some(_)) => {
                bail!("Remote GHerrit history for '{id}' has version tags but no managed branch")
            }
            (Some(_), None) => {
                bail!("Remote GHerrit history for '{id}' has a managed branch but no version tags")
            }
            (Some(branch), Some((&latest_version, &latest_oid))) => {
                if publication.versions.len() != latest_version.get() {
                    bail!("Remote GHerrit version history for '{id}' is not contiguous from v1")
                }
                if branch != latest_oid {
                    bail!(
                        "Remote managed branch refs/heads/{id} does not match latest version tag v{}",
                        latest_version.get()
                    )
                }
                let mut versions = publication.versions.into_values();
                let first = versions.next().expect("a validated published history is nonempty");
                Ok(RemotePublication::Published(RemoteHistory::new(
                    first,
                    versions.collect(),
                )))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBJECT_A: &str = "1111111111111111111111111111111111111111";
    const OBJECT_B: &str = "2222222222222222222222222222222222222222";

    fn object_id(value: &str) -> ObjectId {
        ObjectId::from_hex(value.as_bytes()).unwrap()
    }

    #[test]
    fn parses_valid_histories_and_absence_positionally() {
        let requested = ["Gone", "Gtwo", "Gabsent"];
        let output = format!(
            "{OBJECT_B}\trefs/tags/gherrit/Gtwo/v2\n\
             {OBJECT_A}\trefs/tags/Gone\n\
             {OBJECT_A}\trefs/heads/archive/refs/heads/Gone\n\
             {OBJECT_A}\trefs/heads/Gother\n\
             {OBJECT_A}\trefs/tags/gherrit/Gtwo/v1\n\
             {OBJECT_A}\trefs/heads/Gone\n\
             {OBJECT_A}\trefs/tags/gherrit/Gone/v1\n\
             {OBJECT_B}\trefs/heads/Gtwo\n"
        );

        assert_eq!(
            parse_publications(output.as_bytes(), &requested).unwrap(),
            [
                RemotePublication::Published(RemoteHistory::new(object_id(OBJECT_A), vec![])),
                RemotePublication::Published(RemoteHistory::new(
                    object_id(OBJECT_A),
                    vec![object_id(OBJECT_B)],
                )),
                RemotePublication::Unpublished,
            ]
        );
        assert_eq!(
            parse_publications(b"", &requested).unwrap(),
            [
                RemotePublication::Unpublished,
                RemotePublication::Unpublished,
                RemotePublication::Unpublished,
            ]
        );
    }

    #[test]
    fn ignores_non_utf8_in_an_unrelated_ref() {
        let mut output = format!("{OBJECT_A}\trefs/heads/archive/").into_bytes();
        output.extend_from_slice(b"\xff/refs/heads/Gone\n");
        output.extend_from_slice(format!("{OBJECT_B}\trefs/heads/Gone\n").as_bytes());
        output.extend_from_slice(format!("{OBJECT_B}\trefs/tags/gherrit/Gone/v1\n").as_bytes());

        assert_eq!(
            parse_publications(&output, &["Gone"]).unwrap(),
            [RemotePublication::Published(RemoteHistory::new(object_id(OBJECT_B), vec![]))]
        );
    }

    #[test]
    fn rejects_duplicate_queries_and_untrusted_output_shapes() {
        assert!(parse_publications(b"", &["Gone", "Gone"]).is_err());

        for output in [
            b"\n".to_vec(),
            b"not a remote-ref line\n".to_vec(),
            b"xyz\trefs/heads/Gone\n".to_vec(),
            format!("{OBJECT_A}\t\n").into_bytes(),
            format!("{OBJECT_A}\trefs/heads/Gone\textra\n").into_bytes(),
            b"xyz\trefs/heads/Gother\n".to_vec(),
            format!(
                "{OBJECT_A}\trefs/heads/Gone\n\
                 {OBJECT_A}\trefs/heads/Gone\n\
                 {OBJECT_A}\trefs/tags/gherrit/Gone/v1\n"
            )
            .into_bytes(),
            format!(
                "{OBJECT_A}\trefs/heads/Gone\n\
                 {OBJECT_A}\trefs/tags/gherrit/Gone/v1\n\
                 {OBJECT_A}\trefs/tags/gherrit/Gone/v1\n"
            )
            .into_bytes(),
        ] {
            assert!(parse_publications(&output, &["Gone"]).is_err(), "output={output:?}");
        }
    }

    #[test]
    fn rejects_malformed_or_incoherent_publication_histories() {
        let overflow = ((usize::MAX as u128) + 1).to_string();
        for output in [
            format!("{OBJECT_A}\trefs/heads/Gone\n"),
            format!("{OBJECT_A}\trefs/tags/gherrit/Gone/v1\n"),
            format!("{OBJECT_A}\trefs/heads/Gone\n{OBJECT_B}\trefs/tags/gherrit/Gone/v1\n"),
            format!("{OBJECT_A}\trefs/heads/Gone\n{OBJECT_A}\trefs/tags/gherrit/Gone/v2\n"),
            format!(
                "{OBJECT_A}\trefs/heads/Gone\n\
                 {OBJECT_B}\trefs/tags/gherrit/Gone/v1\n\
                 {OBJECT_A}\trefs/tags/gherrit/Gone/v3\n"
            ),
            format!("{OBJECT_A}\trefs/heads/Gone\n{OBJECT_A}\trefs/tags/gherrit/Gone/v0\n"),
            format!("{OBJECT_A}\trefs/heads/Gone\n{OBJECT_A}\trefs/tags/gherrit/Gone/v01\n"),
            format!(
                "{OBJECT_A}\trefs/heads/Gone\n\
                 {OBJECT_A}\trefs/tags/gherrit/Gone/v1/child\n"
            ),
            format!(
                "{OBJECT_A}\trefs/heads/Gone\n\
                 {OBJECT_A}\trefs/tags/gherrit/Gone/v{overflow}\n"
            ),
        ] {
            assert!(parse_publications(output.as_bytes(), &["Gone"]).is_err(), "output={output:?}");
        }
    }
}
