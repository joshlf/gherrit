//! Production spellings, separate from ref admission and independent models.

use crate::identity::GherritPrId;

pub(crate) fn owned_base_name(id: &GherritPrId) -> String {
    format!("gherrit-bases/{}", id.as_str())
}

pub(crate) fn head(id: &GherritPrId) -> String {
    format!("refs/heads/{}", id.as_str())
}

pub(crate) fn owned_base(id: &GherritPrId) -> String {
    format!("refs/heads/{}", owned_base_name(id))
}

pub(crate) fn tag_root(id: &GherritPrId) -> String {
    format!("refs/tags/gherrit/{}", id.as_str())
}

pub(crate) fn version(id: &GherritPrId, version: u64) -> String {
    format!("{}/v{version}", tag_root(id))
}

pub(crate) fn marker(id: &GherritPrId) -> String {
    format!("{}/pr", tag_root(id))
}
