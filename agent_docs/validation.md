# Validating Changes

This document covers the procedures and requirements for validating changes to
the project, including linting and testing.

## Toolchains

The project builds with the stable toolchain declared in `Cargo.toml`. Its
rustfmt configuration requires nightly, which can be installed with:

```bash
rustup toolchain install nightly --profile minimal --component rustfmt
```

## Validation

Run the same formatting, linting, test, and task-marker checks as CI:

```bash
cargo +nightly fmt --all -- --check
GHERRIT_TEST_BUILD=1 cargo clippy \
  --workspace --all-targets --locked -- -D warnings
GHERRIT_TEST_BUILD=1 cargo test --workspace --all-targets --locked
ci/check_todo.sh
bash ci/test_extract_stack_child.sh
bash ci/test_select_cascade_child.sh
```

`GHERRIT_TEST_BUILD=1` is **required** for Clippy and tests so the binary under
test includes the necessary test-only behavior. Do not use a binary built with
this setting on sensitive repositories or with real credentials.

## Testing Strategy

- **Unit Tests:** Place unit tests in a `mod tests` module within the source
  file they test.

### Updating Snapshots

When tests fail due to snapshot mismatches (e.g., changed CLI output), you can
force update all snapshots to match the new output:

```bash
GHERRIT_TEST_BUILD=1 INSTA_UPDATE=always cargo test \
  --workspace --all-targets --locked
```

**Note:** This will update ALL snapshots for executed tests. You should use `git
diff` to review the changes to the `.snap` files to ensure they are correct
before committing.
