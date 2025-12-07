use crate::util::Repo;
use eyre::{Result, WrapErr, bail};
use owo_colors::OwoColorize;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

const REQUIRED_HOOKS: &[&str] = &["pre-push", "commit-msg", "post-checkout"];
const PROLOGUE: &str = "# gherrit-installer: managed";
const SHIM_TEMPLATE: &str = r#"#!/bin/sh
# gherrit-installer: managed
# This hook is managed by GHerrit.
# Any manual changes to this file may be overwritten by 'gherrit install'.

gherrit hook {} "$@"
"#;

pub fn install(repo: &Repo, force: bool, allow_global: bool) -> Result<()> {
    let hooks_dir = resolve_hooks_dir(repo, allow_global)?;

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

        log::info!("Installed {}", hook.green());
    }

    Ok(())
}

fn resolve_hooks_dir(repo: &Repo, allow_global: bool) -> Result<PathBuf> {
    if let Some(path_buf) = repo.config_path("core.hooksPath")? {
        // Security check: ensure the hooks directory is within the repository.
        let root = repo.workdir().unwrap_or(repo.path());
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

        // Best effort canonicalization
        let abs_hooks = if path_buf.exists() {
            path_buf.canonicalize().unwrap_or(path_buf)
        } else {
            path_buf
        };

        if !abs_hooks.starts_with(&root) {
            if allow_global {
                log::warn!(
                    "Installing to external/global hooks path (allowed by --allow-global): {abs_hooks:?}",
                );
            } else {
                bail!(
                    "By default, GHerrit will not install to external/global hooks path: {abs_hooks:?}\nUse --allow-global to override.",
                );
            }
        }

        return Ok(abs_hooks);
    }

    // Default: .git/hooks
    Ok(repo.path().join("hooks"))
}
