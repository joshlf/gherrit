//! Facts about the trailer bytes supplied by native Git. Callers still choose
//! their own source region, normalization and admission policy.

pub(crate) fn gherrit_id_values(trailers: &[u8]) -> impl Iterator<Item = &[u8]> {
    trailers.split(|byte| *byte == b'\n').filter_map(|line| line.strip_prefix(b"gherrit-pr-id: "))
}
