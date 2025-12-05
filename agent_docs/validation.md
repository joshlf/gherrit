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
cargo test
```

## Testing Strategy

- **Unit Tests:** Place unit tests in a `mod tests` module within the source
  file they test.
