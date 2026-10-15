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
ci/validate.sh
```

The helper uses the caller's stable toolchain and runs the same checks as the
Ubuntu CI job. Set `RUSTUP_TOOLCHAIN=1.89.0` to reproduce its declared version.
The formatter still uses nightly. The Windows job retains its native test
invocation.

For focused feedback, select `production`, `profiles`, `fmt`, `lint`, `unit`, `semantic`,
`adapters`, `hooks`, `cli`, `doc`, `markers`, or `navigation`. `unit` includes all library
tests; `semantic` selects the durable recovery model; `adapters` selects the
pre-push integration target; `hooks` and `cli` select complete process tests.
Focused test selectors list tests first and reject an empty selection.
`navigation` requires Python 3 and checks local Markdown destinations, explicit
source paths and source-function references in the agent/design maps. These
checks establish navigation, not coverage or correctness.

These commands do not replace the full validation required for submission.


The `profiles` check compiles shipping lib/binary, test-driver-only binary and
hosted-contracts-only binary independently. `production` includes these checks
before the existing workspace/all-target check. Runtime tests still use all
features; this does not multiply the runtime suite across profiles.

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

Select the failing scenario and check its owning state or effect oracle
before accepting new output. For example:

```bash
INSTA_UPDATE=always cargo test --all-features --locked \
  --test cli test_commit_msg_hook
git diff -- tests/cli/snapshots
```

Review every changed snapshot. Confirm that exact refs, object IDs, lifecycle,
and effect assertions still support the new output. Avoid regenerating the
entire suite as an ordinary repair step: unrelated snapshots can hide a
regression. Run the full validation helper after reviewing the focused update.

*Updated by an AI agent acting on joshlf's behalf.*
