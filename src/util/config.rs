use std::path::PathBuf;

use eyre::Result;
use gix::bstr::ByteSlice;

use super::Repo;
impl Repo {
    pub fn config_string(&self, key: &str) -> Result<Option<String>> {
        let Some(cow) = self.inner.config_snapshot().string(key) else {
            return Ok(None);
        };
        let s = std::str::from_utf8(cow.as_ref())?;
        Ok(Some(s.trim().to_string()))
    }
    pub fn config_path(&self, key: &str) -> Result<Option<PathBuf>> {
        let snapshot = self.inner.config_snapshot();
        let Some(path_val) = snapshot.path(key) else {
            return Ok(None);
        };

        let bstr: &gix::bstr::BStr = path_val.as_ref();
        let raw_path = bstr.to_path()?.to_path_buf();

        // Resolve relative paths against the workdir (or gitdir for bare repos)
        if raw_path.is_absolute() {
            Ok(Some(raw_path))
        } else {
            // Use `.canonicalize` to guarantee an absolute path.
            let root = self.workdir().unwrap_or(self.path()).canonicalize()?;
            Ok(Some(root.join(raw_path)))
        }
    }
}
