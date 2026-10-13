//! Read-only facts. Cached local refs and reports are never publication authority.

use std::collections::HashSet;

use eyre::{Result, WrapErr, bail};
use serde_json::{Value, json};

use crate::{
    identity::{self, GherritIdTrailer, GherritPrId},
    manage,
    util::{self, CommandExt as _},
};

const MAX_INSPECT_COMMITS: usize = 1000;

/// Lossless byte representation, including non-UTF-8 paths and names.
fn bytes(value: &[u8]) -> Value {
    json!({"utf8": std::str::from_utf8(value).ok(), "hex": value.iter().map(|byte| format!("{byte:02x}")).collect::<String>()})
}

fn git(args: &[&str]) -> Result<Option<Vec<u8>>> {
    let output = util::cmd("git", args).bounded_output()?;
    match output.status.code() {
        Some(0) => Ok(Some(output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout).to_owned())),
        Some(1) if output.stdout.is_empty() => Ok(None),
        _ => bail!("Local Git inspection failed for {:?}", args),
    }
}

fn fact(args: &[&str]) -> Value {
    match git(args) {
        Ok(Some(value)) => json!({"state": "observed", "value": bytes(&value)}),
        Ok(None) => json!({"state": "absent"}),
        Err(_) => json!({"state": "unavailable"}),
    }
}

fn commit_fact(commit: gix::Commit<'_>, seen: &mut HashSet<String>) -> Result<Value> {
    let message = commit.message_raw()?;
    let occurrences = identity::gherrit_id_trailers(message.as_ref());
    let mut problems = Vec::new();
    let id = match occurrences.as_slice() {
        [] => {
            problems.push("missing identity");
            None
        }
        [GherritIdTrailer::Exact { value, .. }] => {
            match GherritPrId::from_trailer(commit.id, value) {
                Ok(id) => Some(id.as_str().to_owned()),
                Err(_) => {
                    problems.push("invalid identity");
                    None
                }
            }
        }
        [GherritIdTrailer::Malformed] => {
            problems.push("malformed identity syntax");
            None
        }
        _ => {
            problems.push("multiple identities");
            None
        }
    };
    if id.as_ref().is_some_and(|id| !seen.insert(id.clone())) {
        problems.push("repeated identity");
    }
    if std::str::from_utf8(message.as_ref()).is_err() {
        problems.push("non-UTF-8 message");
    }
    let title = message.split(|byte| *byte == b'\n').next().unwrap_or_default();
    if title.is_empty() {
        problems.push("empty title");
    }
    if std::str::from_utf8(title).is_ok_and(|title| title.chars().count() > 256) {
        problems.push("oversized title");
    }
    Ok(json!({"oid": commit.id.to_string(), "id": id, "title": bytes(title), "problems": problems}))
}

fn stack(repo: &util::Repo, base: &str) -> Result<Value> {
    let base_oid = repo.rev_parse_single(base)?.detach();
    let head = repo.head_id()?.detach();
    // Traverse lazily with an inspection budget. Missing objects stay errors;
    // literal open options and GIT_NO_LAZY_FETCH prevent acquisition.
    let mut commits = Vec::new();
    let mut reached = false;
    for commit in repo.rev_walk([head]).first_parent_only().all()? {
        let commit = commit?;
        if commit.id == base_oid {
            reached = true;
            break;
        }
        if commits.len() == MAX_INSPECT_COMMITS {
            bail!("Inspection exceeds 1000 commits");
        }
        commits.push(commit.object()?);
    }
    if !reached {
        bail!("Inspection base is not on HEAD's first-parent path");
    }
    commits.reverse();
    let mut seen = HashSet::new();
    let commits = commits
        .into_iter()
        .map(|commit| commit_fact(commit, &mut seen))
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"state": "observed", "base_ref": base, "base_oid": base_oid.to_string(), "head_oid": head.to_string(),
        "basis": "local refs only; may be stale", "commits": commits, "publication_authority": false}),
    )
}

pub(crate) fn report(base: Option<&str>) -> Result<Value> {
    // A real Git discovery makes status useful even when gix cannot open the
    // ref backend. Ordinary commands still require gix's supported format.
    git(&["rev-parse", "--git-dir"])?.ok_or_else(|| eyre::eyre!("No repository"))?;
    let mut report = json!({"schema_version": 1, "mode": "local", "binary_version": env!("CARGO_PKG_VERSION"),
        "formats": {"report": 1, "publication": "owned-base tuple and immutable versions", "canonical_pr_marker": "gherrit-canonical-pr-v1", "persistent_binary_marker": null},
        "git_version": fact(&["--version"]),
        "paths": {
            "worktree_git_dir": fact(&["rev-parse", "--absolute-git-dir"]),
            "common_git_dir": fact(&["rev-parse", "--path-format=absolute", "--git-common-dir"]),
            "workdir": fact(&["rev-parse", "--show-toplevel"]),
            "effective_hooks_dir": fact(&["rev-parse", "--path-format=absolute", "--git-path", "hooks"])},
        "capabilities": {
            "object_format": fact(&["rev-parse", "--show-object-format"]),
            "ref_backend": fact(&["rev-parse", "--show-ref-format"]),
            "ref_storage_config": fact(&["config", "--local", "--no-includes", "--get", "extensions.refStorage"]),
            "shallow": fact(&["rev-parse", "--is-shallow-repository"])},
        "head": {"branch": fact(&["symbolic-ref", "--quiet", "--short", "HEAD"]), "oid": fact(&["rev-parse", "--verify", "HEAD"])},
        "remote": {"state": "not_requested"}, "stack": {"state": "unavailable"}});
    if report["capabilities"]["ref_backend"]["value"]["utf8"] == "--show-ref-format" {
        report["capabilities"]["ref_backend"] =
            json!({"state": "unavailable", "reason": "Git does not report its ref backend"});
    }
    match util::Repo::open(".") {
        Ok(repo) => {
            report["capabilities"]["gix"] = json!({"state": "supported"});
            report["capabilities"]["publication_history"] = match repo.ensure_publishable_history()
            {
                Ok(()) => json!({"state": "admissible", "publication_authority": false}),
                Err(error) => json!({"state": "unsupported", "reason": format!("{error}")}),
            };
            report["management"] = match repo.current_branch().name() {
                Some(branch) => match manage::inspection(&repo, branch) {
                    Ok(value) => json!({"state": "observed", "branch": branch, "value": value}),
                    Err(error) => json!({"state": "unavailable", "reason": format!("{error}")}),
                },
                None => json!({"state": "not_on_branch"}),
            };
            let inferred = repo
                .default_remote_name()
                .ok()
                .map(|remote| format!("refs/remotes/{}/HEAD", remote.as_str()));
            let base = base.or(inferred.as_deref());
            if let Some(base) = base {
                report["stack"] = match stack(&repo, base) {
                    Ok(value) => value,
                    Err(_) => {
                        json!({"state": "unavailable", "base_ref": base, "reason": "Base or literal first-parent objects are unavailable; specify --base with a local ref"})
                    }
                };
            }
        }
        Err(_) => {
            report["capabilities"]["gix"] = json!({"state": "unsupported", "reason": "GHerrit's repository backend could not open this repository; inspect object/ref format and Git configuration"});
            report["management"] = json!({"state": "unavailable"});
        }
    }
    Ok(report)
}

pub(crate) async fn run(json_output: bool, base: Option<&str>, remote: bool) -> Result<()> {
    let mut report = report(base).wrap_err("Failed to inspect repository")?;
    if remote {
        report["mode"] = json!("remote");
        let observed = async {
            let commits = report["stack"]["commits"].as_array().ok_or_else(|| {
                eyre::eyre!("A complete local stack basis is required for remote inspection")
            })?;
            let mut ids = Vec::new();
            let mut seen = HashSet::new();
            for commit in commits {
                if let Some(id) = commit["id"].as_str()
                    && seen.insert(id.to_owned())
                {
                    ids.push(GherritPrId::from_ref_component(id.as_bytes())?);
                }
            }
            crate::pre_push::inspect_remote(&util::Repo::open(".")?, &ids).await
        }
        .await;
        report["remote"] = match observed {
            Ok(value) => value,
            Err(error) => json!({"state":"unavailable","reason":error.to_string()}),
        };
    }
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "GHerrit repository inspection ({}; schema 1)",
            if remote { "remote" } else { "local" }
        );
        println!("Management: {}", serde_json::to_string(&report["management"])?);
        println!("Paths: {}", serde_json::to_string(&report["paths"])?);
        println!("Capabilities: {}", serde_json::to_string(&report["capabilities"])?);
        println!("Stack: {}", serde_json::to_string(&report["stack"])?);
        println!("Remote: {}", serde_json::to_string(&report["remote"])?);
    }
    Ok(())
}
