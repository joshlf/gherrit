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

The [resource-policy inventory](resource-policies.md) records existing limits,
their units and their owning operations. It does not change their values.

## Rust Version

This project uses the stable Rust toolchain and the 2024 edition. Ensure your
code compiles and passes tests on the stable channel.

## Management refusal

Explicit `manage` and `unmanage` requests return failure when configuration drift
blocks the requested change. Automatic post-checkout management remains
advisory: a known drift refusal warns and preserves configuration without making
Git report a failed checkout. Failed observations and writes still return errors.
The installed-hook regression covers tracking the default branch and contrasts
its successful checkout with a failing explicit management request.

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

## Stack action

`ci/rebase_stack_child.sh` uses the workflow's `GITHUB_REPOSITORY` for lookup,
checkout and PR editing. A nonempty child ID must identify exactly one open
same-repository PR with that exact head name. Missing, multiple, fork or
mismatched observations stop before checkout or mutation. The shell contracts
in `ci/test_rebase_stack_child.sh` cover those refusal paths.

*Updated by an AI agent acting on joshlf's behalf.*
