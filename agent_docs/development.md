# Development Guidelines

This document covers guidelines for developing code changes.

## Build and Test

Use the commands in [validation.md](./validation.md) before submitting a
change. They match the checks run in CI, including the required test-build
configuration.

Use [testing.md](./testing.md) when choosing a test layer or extending the test
infrastructure. It defines the product-risk model and the implemented
architecture for pure, adapter-contract, semantic-recovery, and system tests.

- `cargo build`: Builds the project.

### Test Dependencies

The integration tests use local Git repositories and a local GitHub API stub.
They do not require GitHub credentials or an authenticated GitHub CLI.

## Rust Version

This project uses the stable Rust toolchain and the 2024 edition. Ensure your
code compiles and passes tests on the stable channel.

## Hook installation

The installer inspects all destination bytes and constructs the complete plan
before creating directories or replacing hooks. Binary hooks follow the same
sentinel ownership and `--force` rules as text hooks. Each replacement is
written and made executable in a temporary sibling, then renamed over its
destination. A failed preparation leaves that hook untouched. Replacement
replaces a symlink entry rather than modifying its target. Existing file
ownership, ACLs, and other metadata are not copied to the replacement.

The three hooks are replaced sequentially, not transactionally. Earlier
complete replacements can remain after a later failure; rerunning installation
repairs that prefix. This does not guarantee durability after power loss, lock
out another installer, or establish exclusive ownership of an external path.
Git uses common-directory default hooks in linked worktrees; explicit relative
hook paths are resolved from each worktree or the bare Git directory.
