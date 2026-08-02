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
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --all-features \
  --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
ci/check_todo.sh
bash ci/test_extract_stack_child.sh
bash ci/test_rebase_stack_child.sh
```

The `test-driver` feature builds a separate, non-shipping process adapter for
system tests. The production `gherrit` binary never reads test endpoints,
generates deterministic IDs, or dispatches the Git interceptor, even when
Cargo unifies features across the package.

## Testing Strategy

See [testing.md](./testing.md) for the product-risk model, test layers, snapshot
policy, and performance goals. Place pure tests next to the source they test
when that keeps the behavior easy to discover. Use an integration target for
adapter contracts and complete process-boundary scenarios.

### Updating Snapshots

When tests fail due to snapshot mismatches (e.g., changed CLI output), you can
force update all snapshots to match the new output:

```bash
INSTA_UPDATE=always cargo test \
  --workspace --all-targets --all-features --locked
```

**Note:** This will update ALL snapshots for executed tests. You should use `git
diff` to review the changes to the `.snap` files to ensure they are correct
before committing.
