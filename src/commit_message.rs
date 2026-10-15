//! Facts about the trailer bytes supplied by native Git. Callers still choose
//! their own source region, normalization and admission policy.

pub(crate) fn gherrit_id_values(trailers: &[u8]) -> impl Iterator<Item = &[u8]> {
    trailers.split(|byte| *byte == b'\n').filter_map(|line| line.strip_prefix(b"gherrit-pr-id: "))
}

#[cfg(test)]
#[derive(serde::Deserialize)]
pub(crate) struct TrailerCase {
    pub name: String,
    pub trailers: String,
    pub hook_preserves: bool,
    pub publication: String,
}

#[cfg(test)]
pub(crate) fn trailer_cases() -> Vec<TrailerCase> {
    serde_json::from_str(include_str!("../tests/fixtures/trailer-classification.json"))
        .expect("literal trailer-classification corpus")
}
