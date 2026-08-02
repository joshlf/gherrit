# Development Guidelines

This document covers guidelines for developing code changes.

## Build and Test

Use the commands in [validation.md](./validation.md) before submitting a
change. They match the checks run in CI, including the required test-build
configuration.

Use [testing.md](./testing.md) when choosing a test layer or extending the test
infrastructure. It defines the product-risk model and the target architecture
for the ongoing migration away from broad system fixtures.

- `cargo build`: Builds the project.

### Test Dependencies

The integration tests use local Git repositories and a local GitHub API stub.
They do not require GitHub credentials or an authenticated GitHub CLI.

## Rust Version

This project uses the stable Rust toolchain and the 2024 edition. Ensure your
code compiles and passes tests on the stable channel.
