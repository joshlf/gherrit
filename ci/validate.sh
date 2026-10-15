#!/usr/bin/env bash
set -eo pipefail

# Use the caller's Rust toolchain and Cargo target directory. CI installs the
# declared MSRV first; local callers can set RUSTUP_TOOLCHAIN explicitly.
cd "$(dirname "$0")/.."

check() {
  case "$1" in
    production)
      check profiles
      cargo check --workspace --all-targets --locked
      ;;
    profiles)
      cargo check -p gherrit --lib --bin gherrit --no-default-features --locked
      cargo check -p gherrit --bin gherrit-test-driver --no-default-features --features test-driver --locked
      cargo check -p gherrit --bin gherrit-hosted-contracts --no-default-features --features hosted-contracts --locked
      ;;
    fmt) cargo +nightly fmt --all -- --check ;;
    lint) cargo clippy --workspace --all-targets --all-features --locked -- -D warnings ;;
    tests) cargo test --workspace --all-targets --all-features --locked ;;
    doc) cargo test --workspace --doc --all-features --locked ;;
    markers) ci/check_todo.sh ;;
    unit) cargo test --workspace --lib --all-features --locked ;;
    semantic) cargo test --lib --all-features --locked publication_attempt::semantic_oracle ;;
    adapters) cargo test --test pre_push --all-features --locked ;;
    hooks) cargo test --test hooks --all-features --locked ;;
    cli) cargo test --test cli --all-features --locked ;;
    all)
      for step in production fmt lint tests doc markers; do check "$step"; done
      ;;
    *)
      echo "Usage: ci/validate.sh [all|production|profiles|fmt|lint|tests|doc|markers|unit|semantic|adapters|hooks|cli]" >&2
      return 2
      ;;
  esac
}

check "${1:-all}"
