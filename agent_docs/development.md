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

## Management edits

Management classifies existing local ownership before constructing a complete
ordered edit plan. The final management marker follows every branch edit.
Separate Git config writes are not transactional: an interruption can leave
partial drift. An unforced invocation preserves that drift rather than adopting
it; an explicit forced invocation can repair it. Restoring pre-management user
values remains a separate persistence policy.

*Updated by an AI agent acting on joshlf's behalf.*

## Ambiguous commit cleanup

Git selects `core.commentChar=auto` before running prepare-commit-msg and the
editor, and does not pass the selected character to commit-msg hooks. Identical
hook inputs can therefore represent a valid `-F` subject or a message Git will
strip to nothing. GHerrit conservatively leaves a comment-only message alone
and warns when automatic selection makes the identity decision ambiguous. Use
a fixed comment character which does not begin the intended message when an
identity is required. The same conservative cancellation rule applies to
comment-only verbatim input; the hook does not receive cleanup flags.

## Object store locations

The gix 0.75 backend does not implement native Git's
`GIT_OBJECT_DIRECTORY` or `GIT_ALTERNATE_OBJECT_DIRECTORIES` lookup. GHerrit
refuses those overrides explicitly before opening its repository, rather than
silently reading or writing a different object store. On-disk
`objects/info/alternates` remains supported. Object location is independent of
replacement objects and grafts, which must not rewrite publication ancestry.


## Scissors cleanup

Git withholds the cleanup mode. When the cleaned prefix before scissors is
empty but its suffix has message text, GHerrit edits only that suffix. Scissors
cleanup still discards it and preserves cancellation; modes which retain the
text also retain its identity. Comment-only or otherwise ambiguous input keeps
the conservative cancellation rule and warning. The native cleanup matrix
checks both acceptance and cancellation, including explicit empty acceptance.
