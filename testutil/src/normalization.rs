use std::{collections::HashMap, fs, sync::LazyLock};

use regex::Regex;

pub(super) fn normalize_command_output(output: &str) -> String {
    // This Git error message only appears on some platforms/versions.
    let output = output.replace("fatal: the remote end hung up unexpectedly\n", "");
    normalize_git_diagnostic_separators(&output)
}

#[cfg(any(test, windows))]
pub(super) fn normalize_windows_stderr(stderr: &str) -> String {
    static CLAP_USAGE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^Usage: gherrit\.exe(?P<suffix>[ \t\r]|$)").expect("Invalid regex")
    });
    static COMMAND_STATUS_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?m)^(?P<prefix>\[gherrit\](?: \[[A-Z]+\])? [^\r\n]*\bCommand(?: [^\r\n]*)? failed with status: )exit code: ",
        )
        .expect("Invalid regex")
    });
    static MISSING_TMP_WARNING_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?m)^(?P<prefix>\[gherrit\](?: \[[A-Z]+\])? [^\r\n]*\bStderr: )?bash\.exe: warning: could not find /tmp, please create!(?:\r?\n|$)",
        )
        .expect("Invalid regex")
    });

    let stderr = MISSING_TMP_WARNING_REGEX.replace_all(stderr, "${prefix}");
    let stderr = CLAP_USAGE_REGEX.replace_all(&stderr, "Usage: gherrit${suffix}");
    COMMAND_STATUS_REGEX.replace_all(&stderr, "${prefix}exit status: ").into_owned()
}

#[cfg(any(test, windows))]
pub(super) fn normalize_windows_redacted_paths(output: &str) -> String {
    static REDACTED_PATH_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\[(?:REPO_PATH|REMOTE_PATH|EXTERNAL_HOOKS_PATH)\](?:\\+[^\\\s"']+)*"#)
            .expect("Invalid regex")
    });
    static WINDOWS_SEPARATOR_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\\+").expect("Invalid regex"));

    REDACTED_PATH_REGEX
        .replace_all(output, |path: &regex::Captures<'_>| {
            WINDOWS_SEPARATOR_REGEX.replace_all(&path[0], "/").into_owned()
        })
        .into_owned()
}

pub(super) fn normalize_git_diagnostic_separators(output: &str) -> String {
    static ATOMIC_PUSH_SEPARATOR_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^(error: atomic push failed[^\r\n]*\r?\n)(?:\r?\n)+(To )")
            .expect("Invalid regex")
    });

    ATOMIC_PUSH_SEPARATOR_REGEX.replace_all(output, "${1}${2}").into_owned()
}

pub(super) fn redact_identities(output: &str, regex: &Regex, namespace: &str) -> String {
    let mut identities = HashMap::new();
    regex
        .replace_all(output, |captures: &regex::Captures<'_>| {
            let next_index = identities.len() + 1;
            let index = identities.entry(captures[0].to_string()).or_insert(next_index);
            format!("[{namespace}_{index}]")
        })
        .into_owned()
}

pub(super) fn apply_literal_redactions<'a>(
    output: &str,
    redactions: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> String {
    redactions.into_iter().fold(output.to_string(), |output, (target, replacement)| {
        output.replace(target, replacement)
    })
}

pub(super) fn expand_literal_redactions<'a>(
    redactions: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<(String, String)> {
    let mut redactions = redactions
        .into_iter()
        .flat_map(|(target, replacement)| {
            let mut targets = vec![target.to_string()];
            let debug = format!("{target:?}");
            let debug =
                debug.strip_prefix('"').and_then(|debug| debug.strip_suffix('"')).unwrap_or(&debug);
            targets.push(debug.to_string());
            if let Ok(canonical) = fs::canonicalize(target) {
                targets.push(canonical.to_string_lossy().into_owned());

                let debug = format!("{canonical:?}");
                let debug = debug
                    .strip_prefix('"')
                    .and_then(|debug| debug.strip_suffix('"'))
                    .unwrap_or(&debug);
                targets.push(debug.to_string());
            }
            let forward_slash_targets = targets
                .iter()
                .filter(|target| target.contains('\\'))
                .map(|target| target.replace('\\', "/"))
                .collect::<Vec<_>>();
            targets.extend(forward_slash_targets);
            targets.sort();
            targets.dedup();
            targets.sort_by_key(|target| std::cmp::Reverse(target.len()));
            targets.into_iter().map(move |target| (target, replacement.to_string()))
        })
        .collect::<Vec<_>>();
    redactions.sort_by_key(|(target, _)| std::cmp::Reverse(target.len()));
    redactions
}
