use std::collections::{HashMap, HashSet, hash_map::Entry};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::ObjectId;

use super::publication::remote_query_batches;
use crate::util::{self, CommandExt as _};

/// Observes the managed branches relevant to the current stack.
pub(super) fn observe_managed_branches(
    repo: &util::Repo,
    gherrit_ids: &[String],
) -> Result<HashMap<String, String>> {
    remote_query_batches(gherrit_ids).try_fold(HashMap::new(), |mut states, chunk| {
        let mut arguments = vec!["ls-remote".to_string(), repo.default_remote_name()];
        arguments.extend(chunk.iter().map(|id| format!("refs/heads/{id}")));

        let output = util::cmd("git", arguments).checked_output()?;
        let stdout = core::str::from_utf8(&output.stdout)
            .wrap_err("`git ls-remote` produced non-UTF-8 output")?;

        parse_managed_branches(stdout, chunk)?.into_iter().try_for_each(|(id, object_id)| {
            match states.entry(id) {
                Entry::Vacant(entry) => {
                    entry.insert(object_id);
                    Ok(())
                }
                Entry::Occupied(entry) => {
                    bail!("`git ls-remote` reported managed branch {} more than once", entry.key())
                }
            }
        })?;

        Ok(states)
    })
}

fn parse_managed_branches(
    output: &str,
    requested_ids: &[String],
) -> Result<HashMap<String, String>> {
    let requested_ids = requested_ids.iter().map(String::as_str).collect::<HashSet<_>>();

    output.lines().try_fold(HashMap::new(), |mut states, line| {
        let (object_id, ref_name) = line
            .split_once('\t')
            .ok_or_else(|| eyre!("malformed `git ls-remote` line: {line:?}"))?;
        ObjectId::from_hex(object_id.as_bytes())
            .wrap_err_with(|| format!("invalid object ID in `git ls-remote` line: {line:?}"))?;

        let id = ref_name
            .strip_prefix("refs/heads/")
            .filter(|id| requested_ids.contains(id))
            .ok_or_else(|| eyre!("unexpected ref in `git ls-remote` line: {line:?}"))?;

        match states.entry(id.to_string()) {
            Entry::Vacant(entry) => {
                entry.insert(object_id.to_string());
            }
            Entry::Occupied(_) => {
                bail!("`git ls-remote` reported {ref_name} more than once");
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
        let output = format!("{OBJECT_B}\trefs/heads/Gtwo\n{OBJECT_A}\trefs/heads/Gone\n");

        assert_eq!(
            parse_managed_branches(&output, &requested).unwrap(),
            HashMap::from([
                ("Gone".to_string(), OBJECT_A.to_string()),
                ("Gtwo".to_string(), OBJECT_B.to_string()),
            ])
        );
        assert!(parse_managed_branches("", &requested).unwrap().is_empty());
    }

    #[test]
    fn rejects_every_untrusted_output_shape() {
        let requested = ids(&["Gone"]);
        for output in [
            "not a remote-ref line\n".to_string(),
            "xyz\trefs/heads/Gone\n".to_string(),
            format!("{OBJECT_A}\trefs/tags/Gone\n"),
            format!("{OBJECT_A}\trefs/heads/Gother\n"),
            format!("{OBJECT_A}\trefs/heads/Gone\n{OBJECT_A}\trefs/heads/Gone\n"),
        ] {
            assert!(parse_managed_branches(&output, &requested).is_err(), "output={output:?}");
        }
    }
}
