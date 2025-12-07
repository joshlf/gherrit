use crate::util::Repo;
use eyre::{Result, WrapErr, bail};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

const REQUIRED_HOOKS: &[&str] = &["pre-push", "commit-msg", "post-checkout"];
const PROLOGUE: &str = "# gherrit-installer: managed";
const SHIM_TEMPLATE: &str = r#"#!/bin/sh
# gherrit-installer: managed
# This hook is managed by GHerrit.
# Any manual changes to this file may be overwritten by 'gherrit install'.

gherrit hook {} "$@"
"#;

pub fn install(repo: &Repo, force: bool) -> Result<()> {
    let hooks_dir = resolve_hooks_dir(repo)?;

    fs::create_dir_all(&hooks_dir)
        .wrap_err_with(|| format!("Failed to create hooks directory: {:?}", hooks_dir))?;

    // Phase 1: Validation
    let mut conflicts = Vec::new();
    for hook in REQUIRED_HOOKS {
        let hook_path = hooks_dir.join(hook);
        if hook_path.exists() {
            let content = fs::read_to_string(&hook_path)
                .wrap_err_with(|| format!("Failed to read hook file: {:?}", hook_path))?;
            if !content.contains(PROLOGUE) {
                conflicts.push(hook_path);
            }
        }
    }

    if !conflicts.is_empty() && !force {
        let mut msg = String::from("Refusing to overwrite unmanaged hooks:\n");
        for path in conflicts {
            msg.push_str(&format!("  - {}\n", path.display()));
        }
        msg.push_str("\nUse --force to overwrite them.");
        bail!(msg);
    }

    // Phase 2: Execution
    for hook in REQUIRED_HOOKS {
        let hook_path = hooks_dir.join(hook);
        let content = SHIM_TEMPLATE.replace("{}", hook);

        fs::write(&hook_path, &content)
            .wrap_err_with(|| format!("Failed to write hook file: {:?}", hook_path))?;

        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&hook_path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&hook_path, perms)?;
        }

        println!("Installed {}", hook);
    }

    Ok(())
}

fn resolve_hooks_dir(repo: &Repo) -> Result<PathBuf> {
    // We use git itself to resolve the hooks path, which handles core.hooksPath and
    // ensures we get the correct location even if we are not at the repo root.
    // However, we must ensure it interprets the path relative to the REPO root if it's
    // a relative path in config.
    // actually, `git rev-parse --git-path hooks` handles all this.
    // But `gix` might provide access to config.
    // The safest way consistent with `util::cmd` is invoking git.

    // Check core.hooksPath via git command because gix config resolution might be complex
    // regarding path expansion (though `repo.config_path` might work).
    // Let's use `git rev-parse --git-path hooks` which is the standard way.

    let output = Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .current_dir(repo.workdir().unwrap_or(repo.path())) // Use workdir or gitdir
        .output()
        .wrap_err("Failed to execute git rev-parse")?;

    if !output.status.success() {
        bail!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let path_str = String::from_utf8(output.stdout)
        .wrap_err("Invalid UTF-8 in git rev-parse output")?
        .trim()
        .to_string();

    let hooks_path = if let Some(work_dir) = repo.workdir() {
        work_dir.join(&path_str)
    } else {
        repo.path().join(&path_str) // Bare repo?
    };

    // Canonicalize to check if it is inside the repo
    let canonical_hooks = match hooks_path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            // If it doesn't exist yet, that's fine, we might create it.
            // But we can't canonicalize non-existent path easily.
            // We can check if it is explicitly external.
            if std::path::Path::new(&path_str).is_absolute() {
                PathBuf::from(path_str)
            } else {
                hooks_path
            }
        }
    };

    // Check if external
    // If core.hooksPath is set to something outside repo.
    // We check if `canonical_hooks` starts with `repo.path()` or `repo.work_dir()`.
    // Actually, user requirement: "If outside ... Bail with error".

    // Git usually returns relative path for internal hooks, and absolute for external.
    // If `git rev-parse` returned absolute path, we should check it.

    // Let's resolve the repo root.
    let root = repo
        .workdir()
        .unwrap_or(repo.path())
        .canonicalize()
        .unwrap_or_else(|_| repo.workdir().unwrap_or(repo.path()).to_path_buf());

    // Best effort canonicalization for hooks path if it exists
    let abs_hooks = if canonical_hooks.exists() {
        canonical_hooks.canonicalize().unwrap_or(canonical_hooks)
    } else {
        // If it doesn't exist, assume it's relative to root if relative
        // or absolute if absolute.
        canonical_hooks
    };

    if !abs_hooks.starts_with(&root) {
        // One exception: if `core.hooksPath` is NOT set, git defaults to `.git/hooks`.
        // If `.git` is a worktree, `rev-parse` might return path to main repo?
        // Let's assume standard behavior.
        // User explicitly said: "If outside (e.g. absolute path not starting with repo root): Bail".
        bail!(
            "GHerrit cannot install to external/global hooks path: {:?}",
            abs_hooks
        );
    }

    Ok(abs_hooks)
}
