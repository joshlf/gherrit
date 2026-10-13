#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{fs, path::PathBuf};

use eyre::{Result, WrapErr, bail};
use owo_colors::OwoColorize;

use crate::util::Repo;

const REQUIRED_HOOKS: &[&str] = &["pre-push", "commit-msg", "post-checkout"];
const PROLOGUE: &str = "# gherrit-installer: managed";
const SHIM_TEMPLATE: &str = r#"#!/bin/sh
# gherrit-installer: managed
# This hook is managed by GHerrit.
# Any manual changes to this file may be overwritten by 'gherrit install'.

gherrit hook {} "$@"
"#;

struct HookReplacement {
    name: &'static str,
    path: PathBuf,
    content: String,
}

struct InstallPlan {
    directory: PathBuf,
    hooks: Vec<HookReplacement>,
}

fn is_managed_hook(content: &[u8]) -> bool {
    content.windows(PROLOGUE.len()).any(|window| window == PROLOGUE.as_bytes())
}

impl InstallPlan {
    /// Inspect every destination before creating directories or writing hooks.
    /// This retains sentinel ownership; it does not lock out concurrent writers.
    fn inspect(directory: PathBuf, force: bool) -> Result<Self> {
        let mut conflicts = Vec::new();
        let mut hooks = Vec::new();
        for &name in REQUIRED_HOOKS {
            let path = directory.join(name);
            if path.try_exists()? {
                let content = fs::read(&path)
                    .wrap_err_with(|| format!("Failed to read hook file: {path:?}"))?;
                if !is_managed_hook(&content) {
                    conflicts.push(path.clone());
                }
            }
            hooks.push(HookReplacement { name, path, content: SHIM_TEMPLATE.replace("{}", name) });
        }
        if !conflicts.is_empty() && !force {
            let mut message = String::from("Refusing to overwrite unmanaged hooks:\n");
            for path in conflicts {
                message.push_str(&format!("  - {}\n", path.display()));
            }
            message.push_str("\nUse --force to overwrite them.");
            bail!(message);
        }
        Ok(Self { directory, hooks })
    }

    fn apply(self) -> Result<()> {
        fs::create_dir_all(&self.directory)
            .wrap_err_with(|| format!("Failed to create hooks directory: {:?}", self.directory))?;
        for hook in self.hooks {
            fs::write(&hook.path, &hook.content)
                .wrap_err_with(|| format!("Failed to write hook file: {:?}", hook.path))?;
            #[cfg(unix)]
            {
                let mut permissions = fs::metadata(&hook.path)?.permissions();
                permissions.set_mode(0o755);
                fs::set_permissions(&hook.path, permissions)?;
            }
            log::info!("Installed {}", hook.name.green());
        }
        Ok(())
    }
}

pub fn install(repo: &Repo, force: bool, allow_global: bool) -> Result<()> {
    InstallPlan::inspect(resolve_hooks_dir(repo, allow_global)?, force)?.apply()
}

fn resolve_hooks_dir(repo: &Repo, allow_global: bool) -> Result<PathBuf> {
    if let Some(path_buf) = repo.config_path("core.hooksPath")? {
        // Security check: ensure the hooks directory is within the repository.
        let layout = repo.layout();
        let root = layout.workdir.unwrap_or(layout.worktree_git_dir);
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

        // Best effort canonicalization
        let abs_hooks =
            if path_buf.exists() { path_buf.canonicalize().unwrap_or(path_buf) } else { path_buf };

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

    // Default hooks belong to the common repository, including linked worktrees.
    Ok(repo.layout().common_git_dir.join("hooks"))
}
