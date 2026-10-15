//! Managed ref names shared by publication, observation and local persistence.
//! Parsing and admission stay with the caller; these functions only spell names.

pub(super) fn head(id: &str) -> String {
    format!("refs/heads/{id}")
}

pub(super) fn version_tag_prefix(id: &str) -> String {
    format!("refs/tags/gherrit/{id}/v")
}

pub(super) fn version_tag(id: &str, version: usize) -> String {
    format!("{}{version}", version_tag_prefix(id))
}
