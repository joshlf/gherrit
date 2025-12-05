# Development Guidelines

This document covers guidelines for developing code changes.

## Build and Test

Use standard `cargo` commands to build and test the project.

- `cargo build`: Builds the project.
- `cargo test`: Runs the test suite.

### Dependencies

This project relies on the GitHub CLI (`gh`) for PR management. Ensure it is
installed and authenticated (`gh auth login`) when running integration tests or
using the tool.

## Rust Version

This project uses the stable Rust toolchain and the 2024 edition. Ensure your
code compiles and passes tests on the stable channel.
