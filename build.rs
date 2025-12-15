fn main() {
    println!("cargo:rerun-if-env-changed=GHERRIT_TEST_BUILD");

    if std::env::var("GHERRIT_TEST_BUILD").is_ok() {
        println!(
            "cargo:warning=⚠️  DANGEROUS TEST MODE ENABLED (`GHERRIT_TEST_BUILD` set): This build contains unsafe test logic! Do not use with sensitive data or real authentication keys! ⚠️"
        );
    }
}
