use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;

#[test]
fn production_binary_rejects_the_test_driver_protocol() {
    Command::new(assert_cmd::cargo::cargo_bin!("gherrit"))
        .arg("__test-git")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand '__test-git'"));
}
