//! Install small hook shims while respecting existing hook ownership.
//!
//! Inspect all three destinations before changing any of them. Each hook is
//! then prepared completely and renamed into place. Replacement is atomic for
//! one directory entry, not for the whole set of hooks; a later failure can
//! leave an installed prefix. Rerunning installation repairs that prefix.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{fs, io::Write as _, path::PathBuf};

use eyre::{Result, WrapErr, bail};
use owo_colors::OwoColorize;

use crate::util::Repo;

const REQUIRED_HOOKS: &[&str] = &["pre-push", "commit-msg", "post-checkout"];
// This marker, rather than a byte-for-byte template match, grants permission
// to replace an existing hook without --force. Keep it in generated shims.
const PROLOGUE: &str = "# gherrit-installer: managed";
const SHIM_TEMPLATE: &str = r#"#!/bin/sh
# gherrit-installer: managed
# This hook is managed by GHerrit.
# Any manual changes to this file may be overwritten by 'gherrit install'.

gherrit hook {} "$@"
"#;

struct HookReplacement {
    path: PathBuf,
    content: String,
}

/// A complete executable sibling; failure or drop leaves the old hook intact.
struct PreparedHook {
    destination: PathBuf,
    temporary: tempfile::NamedTempFile,
}

impl HookReplacement {
    fn new(directory: &std::path::Path, name: &'static str) -> Self {
        Self { path: directory.join(name), content: SHIM_TEMPLATE.replace("{}", name) }
    }

    fn prepare(self) -> Result<PreparedHook> {
        // A sibling temporary file permits rename over the destination without
        // exposing a partly written hook. Its lifetime also handles cleanup if
        // writing or setting executable permissions fails.
        let parent = self.path.parent().expect("hook destination has a parent");
        let mut temporary = tempfile::Builder::new()
            .prefix(".gherrit-hook-")
            .tempfile_in(parent)
            // tempfile includes a random path in its creation error. Keep
            // diagnostics stable and identify the intended destination below.
            .map_err(|error| eyre::eyre!("Cannot prepare hook replacement: {}", error.kind()))
            .wrap_err_with(|| format!("Failed to write hook file: {:?}", self.path))?;
        temporary.write_all(self.content.as_bytes())?;
        #[cfg(unix)]
        temporary.as_file().set_permissions(fs::Permissions::from_mode(0o755))?;
        temporary.as_file().sync_all()?;
        Ok(PreparedHook { destination: self.path, temporary })
    }
}

impl PreparedHook {
    fn replace(self) -> Result<()> {
        // Rename replaces a symlink entry instead of writing through it. The
        // new file has its own metadata; this does not preserve the old file's
        // owner/ACLs or establish durability of the directory after power loss.
        self.temporary
            .persist(&self.destination)
            .map_err(|error| error.error)
            .wrap_err_with(|| format!("Failed to replace hook file: {:?}", self.destination))?;
        Ok(())
    }
}

struct InstallPlan {
    directory: PathBuf,
}

fn is_managed_hook(content: &[u8]) -> bool {
    content.windows(PROLOGUE.len()).any(|window| window == PROLOGUE.as_bytes())
}

impl InstallPlan {
    /// Inspect every destination before creating directories or writing hooks.
    /// This retains sentinel ownership; it does not lock out concurrent writers.
    fn inspect(directory: PathBuf, force: bool) -> Result<Self> {
        let mut conflicts = Vec::new();
        for &name in REQUIRED_HOOKS {
            let path = directory.join(name);
            match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    let content = match fs::read(&path) {
                        Ok(content) => Some(content),
                        Err(error)
                            if metadata.file_type().is_symlink()
                                && error.kind() == std::io::ErrorKind::NotFound =>
                        {
                            None
                        }
                        Err(error) => {
                            return Err(error)
                                .wrap_err_with(|| format!("Failed to read hook file: {path:?}"));
                        }
                    };
                    if !content.as_deref().is_some_and(is_managed_hook) {
                        conflicts.push(path.clone());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .wrap_err_with(|| format!("Failed to inspect hook entry: {path:?}"));
                }
            }
        }
        if !conflicts.is_empty() && !force {
            let mut message = String::from("Refusing to overwrite unmanaged hooks:\n");
            for path in conflicts {
                message.push_str(&format!("  - {}\n", path.display()));
            }
            message.push_str("\nUse --force to overwrite them.");
            bail!(message);
        }
        Ok(Self { directory })
    }

    fn apply(self) -> Result<()> {
        self.apply_using(|hook| hook.prepare()?.replace())
    }

    fn apply_using(self, mut replace: impl FnMut(HookReplacement) -> Result<()>) -> Result<()> {
        fs::create_dir_all(&self.directory)
            .wrap_err_with(|| format!("Failed to create hooks directory: {:?}", self.directory))?;
        // The required set is fixed. The inspected directory determines each
        // hook's path and payload; do not store a second independently mutable
        // list of those derived values in the plan.
        for &name in REQUIRED_HOOKS {
            let hook = HookReplacement::new(&self.directory, name);
            replace(hook)?;
            log::info!("Installed {}", name.green());
        }
        Ok(())
    }
}

pub fn install(repo: &Repo, force: bool, allow_global: bool) -> Result<()> {
    InstallPlan::inspect(resolve_hooks_dir(repo, allow_global)?, force)?.apply()
}

fn resolve_hooks_dir(repo: &Repo, allow_global: bool) -> Result<PathBuf> {
    if let Some(path_buf) = repo.config_path("core.hooksPath")? {
        // External hook paths can affect repositories beyond this one, so
        // require the separate --allow-global opt-in. --force only authorizes
        // replacing hooks; it does not authorize using an external path.
        let layout = repo.layout();
        let root = layout.workdir.unwrap_or(layout.worktree_git_dir);
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

        // Existing paths can be compared after resolving symlinks. Missing
        // paths retain their spelling. This is a best-effort location check,
        // not a filesystem capability or protection from concurrent changes.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_interrupted_replacement_preserves_complete_hook_files() {
        for existing in [false, true] {
            for fail_at in 0..REQUIRED_HOOKS.len() {
                for fail_after_prepare in [false, true] {
                    let directory = tempfile::tempdir().unwrap();
                    let old = b"# gherrit-installer: managed\noriginal hook";
                    if existing {
                        for name in REQUIRED_HOOKS {
                            fs::write(directory.path().join(name), old).unwrap();
                        }
                    }
                    let plan = InstallPlan::inspect(directory.path().to_owned(), false).unwrap();
                    // This executor test owns payload delivery; integration
                    // tests independently own complete shim encoding and mode.
                    let expected: Vec<_> = REQUIRED_HOOKS
                        .iter()
                        .map(|name| SHIM_TEMPLATE.replace("{}", name))
                        .collect();
                    let mut index = 0;
                    let result = plan.apply_using(|hook| {
                        let current = index;
                        index += 1;
                        if current == fail_at && !fail_after_prepare {
                            bail!("before preparation");
                        }
                        let prepared = hook.prepare()?;
                        if current == fail_at {
                            bail!("before replacement");
                        }
                        prepared.replace()
                    });
                    assert!(result.is_err());
                    for (index, name) in REQUIRED_HOOKS.iter().enumerate() {
                        let path = directory.path().join(name);
                        if index < fail_at {
                            assert_eq!(fs::read(path).unwrap(), expected[index].as_bytes());
                        } else if existing {
                            assert_eq!(fs::read(path).unwrap(), old);
                        } else {
                            assert!(!path.exists());
                        }
                    }
                    assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
                        !entry.unwrap().file_name().to_string_lossy().starts_with(".gherrit-hook-")
                    }));
                    InstallPlan::inspect(directory.path().to_owned(), false)
                        .unwrap()
                        .apply()
                        .unwrap();
                    for (index, name) in REQUIRED_HOOKS.iter().enumerate() {
                        assert_eq!(
                            fs::read(directory.path().join(name)).unwrap(),
                            expected[index].as_bytes()
                        );
                    }
                }
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn replacement_replaces_symlink_entry_without_writing_its_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("outside-target");
        let original = b"# gherrit-installer: managed\nexternal original";
        fs::write(&target, original).unwrap();
        let hooks = directory.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        let link = hooks.join("pre-push");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        InstallPlan::inspect(hooks, false).unwrap().apply().unwrap();
        assert_eq!(fs::read(target).unwrap(), original);
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_file());
    }
}

#[cfg(all(test, unix))]
#[test]
fn dangling_hook_links_require_force_without_touching_the_target() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("missing-target");
    let link = directory.path().join("pre-push");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(InstallPlan::inspect(directory.path().to_owned(), false).is_err());
    assert_eq!(fs::read_link(&link).unwrap(), target);
    assert!(!target.exists());
    InstallPlan::inspect(directory.path().to_owned(), true).unwrap().apply().unwrap();
    assert!(!fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    assert!(!target.exists());
}
