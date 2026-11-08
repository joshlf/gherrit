//! Checks ownership and privacy contracts against the real private modules.
//!
//! This small test crate keeps compiler fixtures independent of GHerrit's
//! process and network dependencies. Positive fixtures establish that the
//! imported APIs are usable before negative fixtures test forbidden calls.

#[cfg(test)]
mod tests {
    #[test]
    fn private_api_contracts() {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/ui/batch-valid.rs");
        cases.pass("tests/ui/identity-valid.rs");
        cases.compile_fail("tests/ui/identity-forged-invalid.rs");
        cases.compile_fail("tests/ui/batch-overlap-invalid.rs");
        cases.compile_fail("tests/ui/batch-forged-invalid.rs");
        cases.compile_fail("tests/ui/batch-reuse-accept-invalid.rs");
        cases.compile_fail("tests/ui/batch-reuse-reject-invalid.rs");
    }
}
