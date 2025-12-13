# Validating Changes

This document covers the procedures and requirements for validating changes to
the project, including linting and testing.

## Linting

Run Clippy to catch common mistakes and improve code quality.

```bash
cargo clippy --tests
```

## Validating Changes

Ensure the project builds and passes checks.

```bash
cargo check --tests
GHERRIT_TEST_BUILD=1 cargo test
```

`GHERRIT_TEST_BUILD=1` is **required** in order to enable test-only behavior. If
it is not set when building the binary under test, some tests will fail.

## Testing Strategy

- **Unit Tests:** Place unit tests in a `mod tests` module within the source
  file they test.
