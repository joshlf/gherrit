//! Reads managed branch tips without fetching their objects.
//!
//! Each successful `ls-remote` response gives exact branch/OID pairs for later
//! leases. Several requests may be needed, so the results are not a snapshot of
//! all remote refs. A tip may still change before the push.

use std::collections::{HashMap, hash_map::Entry};

use color_eyre::eyre::{Context as _, Result, bail};
use gix::ObjectId;

use super::{publication::remote_query_batches, refs};
use crate::util::{self, CommandExt as _};

/// Reads the managed branch tips for the current stack.
///
/// # Errors
///
/// Returns an error if a Git request fails or its output is malformed, contains
/// invalid object IDs, or repeats a requested branch.
pub(super) fn observe_managed_branches(
    repo: &util::Repo,
    gherrit_ids: &[String],
) -> Result<HashMap<String, String>> {
    remote_query_batches(gherrit_ids).try_fold(HashMap::new(), |mut states, chunk| {
        let mut arguments = vec!["ls-remote".to_string(), repo.default_remote_name()];
        arguments.extend(chunk.iter().map(|id| refs::head(id)));

        let output = util::cmd("git", arguments).checked_output()?;

        parse_managed_branches(&output.stdout, chunk)?.into_iter().try_for_each(
            |(id, object_id)| match states.entry(id) {
                Entry::Vacant(entry) => {
                    entry.insert(object_id);
                    Ok(())
                }
                Entry::Occupied(entry) => {
                    bail!("`git ls-remote` reported managed branch {} more than once", entry.key());
                }
            },
        )?;

        Ok(states)
    })
}

/// Parses the standard output of `git ls-remote`.
///
/// The output contains zero or more `<object ID>   <fully qualified ref name>`
/// records separated by line feeds. An absent requested ref produces no record.
/// The final line feed is optional. Ref names need not be UTF-8, so parsing
/// uses bytes and keeps only exact requested refs after validating every
/// complete record and object ID.
///
/// # Errors
///
/// Returns an error for malformed records, invalid object IDs, or duplicate
/// requested refs, including malformed records for refs that were not
/// requested.
fn parse_managed_branches(
    output: &[u8],
    requested_ids: &[String],
) -> Result<HashMap<String, String>> {
    let requested_refs =
        requested_ids.iter().map(|id| (refs::head(id).into_bytes(), id)).collect::<HashMap<_, _>>();
    if output.is_empty() {
        return Ok(HashMap::new());
    }
    let output = output.strip_suffix(b"\n").unwrap_or(output);

    output.split(|byte| *byte == b'\n').try_fold(HashMap::new(), |mut states, line| {
        let mut fields = line.split(|byte| *byte == b'\t');
        let (Some(object_id), Some(ref_name), None) = (fields.next(), fields.next(), fields.next())
        else {
            bail!("malformed `git ls-remote` line: {line:?}");
        };
        if ref_name.is_empty() {
            bail!("malformed `git ls-remote` line: {line:?}");
        }
        let object_id = ObjectId::from_hex(object_id)
            .wrap_err_with(|| format!("invalid object ID in `git ls-remote` line: {line:?}"))?;

        let Some(id) = requested_refs.get(ref_name) else {
            // Git's patterns may match refs we did not request. Use only exact
            // requested names when choosing a managed branch's lease.
            return Ok(states);
        };

        match states.entry((*id).clone()) {
            Entry::Vacant(entry) => {
                entry.insert(object_id.to_string());
            }
            Entry::Occupied(_) => {
                bail!("`git ls-remote` reported refs/heads/{id} more than once");
            }
        }
        Ok(states)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBJECT_A: &str = "1111111111111111111111111111111111111111";
    const OBJECT_B: &str = "2222222222222222222222222222222222222222";

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_requested_managed_branches_and_absence() {
        let requested = ids(&["Gone", "Gtwo", "Gabsent"]);
        let output = format!(
            "{OBJECT_B}\trefs/heads/Gtwo\n\
             {OBJECT_A}\trefs/tags/Gone\n\
             {OBJECT_A}\trefs/heads/archive/refs/heads/Gone\n\
             {OBJECT_A}\trefs/heads/Gother\n\
             {OBJECT_A}\trefs/heads/Gone\n"
        );

        assert_eq!(
            parse_managed_branches(output.as_bytes(), &requested).unwrap(),
            HashMap::from([
                ("Gone".to_string(), OBJECT_A.to_string()),
                ("Gtwo".to_string(), OBJECT_B.to_string()),
            ])
        );
        assert!(parse_managed_branches(b"", &requested).unwrap().is_empty());
    }

    #[test]
    fn ignores_non_utf8_in_an_unrelated_ref() {
        let requested = ids(&["Gone"]);
        let mut output = format!("{OBJECT_A}\trefs/heads/archive/").into_bytes();
        output.extend_from_slice(b"\xff/refs/heads/Gone\n");
        output.extend_from_slice(format!("{OBJECT_B}\trefs/heads/Gone\n").as_bytes());

        assert_eq!(
            parse_managed_branches(&output, &requested).unwrap(),
            HashMap::from([("Gone".to_string(), OBJECT_B.to_string())])
        );
    }

    #[test]
    fn rejects_every_untrusted_output_shape() {
        let requested = ids(&["Gone"]);
        for output in [
            b"\n".to_vec(),
            b"not a remote-ref line\n".to_vec(),
            b"xyz\trefs/heads/Gone\n".to_vec(),
            format!("{OBJECT_A}\t\n").into_bytes(),
            format!("{OBJECT_A}\trefs/heads/Gone\textra\n").into_bytes(),
            b"xyz\trefs/heads/Gother\n".to_vec(),
            format!("{OBJECT_A}\trefs/heads/Gone\n{OBJECT_A}\trefs/heads/Gone\n").into_bytes(),
        ] {
            assert!(parse_managed_branches(&output, &requested).is_err(), "output={output:?}");
        }
    }
}
