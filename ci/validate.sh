#!/usr/bin/env bash
set -eo pipefail

# Use the caller's Rust toolchain and Cargo target directory. CI installs the
# declared MSRV first; local callers can set RUSTUP_TOOLCHAIN explicitly.
cd "$(dirname "$0")/.."

# Cargo succeeds when a focused filter selects nothing. Check its test listing
# first; pipefail also propagates compilation or listing failures.
selected_tests() {
  cargo test "$@" -- --list | awk '
    /: test$/ { found = 1 }
    END {
      if (!found) {
        print "Test selector matched no tests" > "/dev/stderr"
        exit 1
      }
    }'
  cargo test "$@"
}

check() {
  case "$1" in
    production) cargo check --workspace --all-targets --locked ;;
    profiles)
      cargo check --package gherrit --lib --bin gherrit --no-default-features --locked
      cargo check --package gherrit --bin gherrit-test-driver --no-default-features --features test-driver --locked
      ;;
    fmt) cargo +nightly fmt --all -- --check ;;
    lint) cargo clippy --workspace --all-targets --all-features --locked -- -D warnings ;;
    tests) cargo test --workspace --all-targets --all-features --locked ;;
    doc) cargo test --workspace --doc --all-features --locked ;;
    markers) ci/check_todo.sh ;;
    action) bash ci/test_extract_stack_child.sh; bash ci/test_rebase_stack_child.sh ;;
    navigation) python3 ci/check_navigation.py ;;
    unit) selected_tests --workspace --lib --all-features --locked ;;
    adapters) selected_tests --test pre_push --all-features --locked ;;
    hooks) selected_tests --test hooks --all-features --locked ;;
    cli) selected_tests --test cli --all-features --locked ;;
    all)
      for step in production profiles fmt lint tests doc markers navigation action; do check "$step"; done
      ;;
    *)
      echo "Usage: ci/validate.sh [all|production|profiles|fmt|lint|tests|doc|markers|navigation|unit|adapters|hooks|cli|action]" >&2
      return 2
      ;;
  esac
}

check "${1:-all}"
