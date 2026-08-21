use std::collections::{HashMap, hash_map::Entry};

use color_eyre::eyre::{Context as _, Result, bail};
use gix::ObjectId;

use super::{
    destination::{PushDestination, git_output_records},
    publication::remote_query_batches,
};
/// Observes the managed branches relevant to the current stack.
pub(super) fn observe_managed_branches(
    destination: &PushDestination,
    gherrit_ids: &[String],
) -> Result<HashMap<String, String>> {
    remote_query_batches(gherrit_ids).try_fold(HashMap::new(), |mut states, chunk| {
        let ref_patterns = chunk.iter().map(|id| format!("refs/heads/{id}"));
        let output = destination
            .ls_remote(std::iter::empty(), ref_patterns)
            .output()
            .wrap_err_with(|| {
                format!("Failed to observe GHerrit remote '{}'", destination.configured_remote())
            })?;
        if !output.status.success() {
            bail!(
                "`git ls-remote` failed for GHerrit remote '{}'",
                destination.configured_remote()
            );
        }

        parse_managed_branches(&output.stdout, chunk)?.into_iter().try_for_each(
            |(id, object_id)| match states.entry(id) {
                Entry::Vacant(entry) => {
                    entry.insert(object_id);
                    Ok(())
                }
                Entry::Occupied(entry) => {
                    bail!("`git ls-remote` reported managed branch {} more than once", entry.key())
                }
            },
        )?;

        Ok(states)
    })
}

/// Parses `git ls-remote` standard output.
///
/// The output is zero or more `<object ID>\t<fully qualified ref name>`
/// records separated by line feeds; a requested ref that is absent produces no
/// record. The final line feed is optional here. Ref names need not be UTF-8,
/// so parsing stays byte-oriented and retains only exact requested refs after
/// validating every complete record and object ID.
fn parse_managed_branches(
    output: &[u8],
    requested_ids: &[String],
) -> Result<HashMap<String, String>> {
    let requested_refs = requested_ids
        .iter()
        .map(|id| (format!("refs/heads/{id}").into_bytes(), id))
        .collect::<HashMap<_, _>>();
    if output.is_empty() {
        return Ok(HashMap::new());
    }
    git_output_records(output).try_fold(HashMap::new(), |mut states, line| {
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
        if object_id.is_null() {
            bail!("null object ID in `git ls-remote` line: {line:?}");
        }

        let Some(id) = requested_refs.get(ref_name) else {
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
    const NULL_OBJECT: &str = "0000000000000000000000000000000000000000";

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
    fn accepts_one_cr_before_each_git_line_feed() {
        let requested = ids(&["Gone", "Gtwo"]);
        let output = format!("{OBJECT_A}\trefs/heads/Gone\r\n{OBJECT_B}\trefs/heads/Gtwo\r\n");

        assert_eq!(
            parse_managed_branches(output.as_bytes(), &requested).unwrap(),
            HashMap::from([
                ("Gone".to_string(), OBJECT_A.to_string()),
                ("Gtwo".to_string(), OBJECT_B.to_string()),
            ])
        );
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
            format!("{NULL_OBJECT}\trefs/heads/Gone\n").into_bytes(),
            format!("{NULL_OBJECT}\trefs/heads/Gother\n").into_bytes(),
            format!("{OBJECT_A}\trefs/heads/Gone\n{OBJECT_A}\trefs/heads/Gone\n").into_bytes(),
        ] {
            assert!(parse_managed_branches(&output, &requested).is_err(), "output={output:?}");
        }
    }
}
