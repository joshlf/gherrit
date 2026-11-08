//! Installs hook scripts after checking existing hook ownership.
//!
//! The installer checks all three destinations before changing any of them. It
//! writes each new hook completely, then renames it into place. Each rename is
//! atomic, but the three replacements are not one transaction: if a later one
//! fails, earlier hooks stay installed. Rerunning can repair that partial
//! result.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{fs, io::Write as _, path::PathBuf};

use eyre::{Result, WrapErr, bail};
use owo_colors::OwoColorize;

use crate::util::Repo;

const REQUIRED_HOOKS: &[&str] = &["pre-push", "commit-msg", "post-checkout"];
// This marker lets us replace a hook without --force. The rest of the file need
// not match the template exactly. Keep the marker in generated hooks.
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

/// A complete executable hook awaiting replacement.
///
/// Failure or dropping this value leaves the old hook intact.
struct PreparedHook {
    destination: PathBuf,
    temporary: tempfile::NamedTempFile,
}

impl HookReplacement {
    fn new(directory: &std::path::Path, name: &'static str) -> Self {
        Self { path: directory.join(name), content: SHIM_TEMPLATE.replace("{}", name) }
    }

    fn prepare(self) -> Result<PreparedHook> {
        // Write a temporary file in the same directory, then rename it so
        // readers never see a partly written hook. The temporary file is also
        // cleaned up if writing it or making it executable fails.
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
        // Rename replaces the symlink itself without writing to its target. The
        // new file does not keep the old owner or ACLs. This also does not
        // ensure that the directory change survives a power loss.
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
    /// Inspects hook destinations without changing them.
    ///
    /// The installer checks every destination before creating directories or
    /// writing hooks. Ownership is based on the marker in each file; inspection
    /// does not lock out concurrent writers.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from inspecting entries or reading hooks. A dangling
    /// symlink is treated as an unowned entry. Unowned hooks are rejected
    /// unless `force` is true.
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
        // The hook set is fixed. Build each path and script from the checked
        // directory and hook name instead of storing another mutable list of
        // those same values in the plan.
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
        // An external hooks directory may affect other repositories, so require
        // --allow-global separately. --force allows replacing hooks; it does
        // not allow using an external directory.
        let layout = repo.layout();
        let root = layout.workdir.unwrap_or(layout.worktree_git_dir);
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

        // Resolve symlinks before comparing existing paths. Keep missing paths
        // as written. This check helps locate the directory; it neither grants
        // access to it nor prevents another process from changing it.
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
        let hooks = ["pre-push", "commit-msg", "post-checkout"];
        for existing in [false, true] {
            for fail_at in 0..hooks.len() {
                for fail_after_prepare in [false, true] {
                    let directory = tempfile::tempdir().unwrap();
                    let old = b"# gherrit-installer: managed\noriginal hook";
                    if existing {
                        for name in hooks {
                            fs::write(directory.path().join(name), old).unwrap();
                        }
                    }
                    let plan = InstallPlan::inspect(directory.path().to_owned(), false).unwrap();
                    // This executor test owns payload delivery; integration
                    // tests independently own complete shim encoding and mode.
                    let expected: Vec<_> =
                        hooks.iter().map(|name| SHIM_TEMPLATE.replace("{}", name)).collect();
                    let mut index = 0;
                    let result = plan.apply_using(|hook| {
                        let current = index;
                        assert_eq!(hook.path, directory.path().join(hooks[current]));
                        assert_eq!(hook.content, expected[current]);
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
                    assert_eq!(index, fail_at + 1, "stop at the first failed replacement");
                    for (index, name) in hooks.iter().enumerate() {
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
                    for (index, name) in hooks.iter().enumerate() {
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
