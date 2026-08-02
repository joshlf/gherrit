# Testing Strategy

GHerrit turns a local commit stack into Git refs and GitHub pull requests. Its
tests must establish more than whether individual functions return expected
values: they must show that repeated executions safely converge an external
system, including after interruption and concurrent changes.

This document defines the testing contract, the intended test architecture,
and the criteria for choosing a test layer. It is the target architecture for
an incremental migration; some existing tests still use a combined system-test
harness while that migration is in progress.

## Goals

The test suite exists to provide confidence in four areas.

### Domain Correctness

The suite must establish that GHerrit:

- discovers the intended local stack and preserves its topology;
- rejects unsupported or unsafe local and remote states before writing;
- publishes the exact managed branches and immutable version tags required by
  the local commits;
- projects each commit into the correct pull request title, body, head, base,
  and navigation links;
- performs no writes when the observed state already matches the desired
  state;
- converges after interruption at every externally committed write boundary;
- remains safe when observations become stale or another writer changes the
  remote state; and
- preserves absorbing external states, such as a merged pull request, rather
  than trying to recreate an earlier state.

### Adapter Correctness

The suite must establish that production adapters translate between the domain
model and external protocols correctly:

- local Git history, refs, configuration, and remote state are observed
  accurately;
- Git publication uses the intended atomic refspecs and lease conditions;
- GitHub GraphQL documents are escaped correctly and their complete,
  partial, malformed, and error responses are decoded correctly;
- installed hooks are complete, executable, and forward the exact arguments
  required by their subcommands; and
- platform-specific process and path behavior works on every supported
  platform.

### User-Visible Behavior

The suite must protect:

- command exit status, standard output, and standard error;
- actionable diagnostics for rejected and ambiguous operations;
- the guarantee that a failed pre-push hook blocks the enclosing Git push; and
- complete, human-readable pull request bodies and stack metadata.

### Operational Quality

The suite itself must be:

- **Hermetic:** ordinary tests inherit no credentials, user configuration,
  network endpoints, or unrelated environment variables.
- **Deterministic:** time, identities, commit metadata, ordering, and injected
  failures are controlled explicitly.
- **Bounded:** subprocesses, servers, and waits have deadlines and deterministic
  teardown.
- **Fast:** policy and reconciliation changes receive feedback without starting
  processes, repositories, or servers.
- **Fail-closed:** unsupported fake operations, unexpected requests, and
  unconsumed expectations fail the test.
- **Understandable:** a failure identifies the violated behavior and the layer
  responsible for it.
- **Extensible:** adding a policy, external operation, failure point, or race
  schedule does not require extending a large simulator.
- **Faithful:** tests use the production artifact and real external tools at the
  boundaries where their behavior is the subject of the test.

These properties are part of correctness. A broad test that flakes, silently
accepts unexpected calls, or is too expensive to run routinely provides weak
assurance even if it covers many lines.

## Product Risk Model

Organize coverage around product risks rather than source files.

| Risk | Required evidence |
| --- | --- |
| Local intent is misunderstood | Pure stack and policy cases; focused real-Git discovery contracts |
| The wrong refs are published | Pure publication decisions; real atomic-push and lease contracts |
| GitHub differs from the desired projection | Pure projection and minimal-update cases; GraphQL codec contracts |
| A retry repeats or loses work | Interruption tests at every committed effect; idempotence invariants |
| Two writers corrupt state | Deterministic stale-observation and interleaving schedules |
| A hook does not enforce the workflow | Installed-hook success and blocking system tests |
| A protocol assumption changes | Exact scripted adapter contracts; optional live service smoke test |
| Platform behavior differs | Cross-platform adapter and process-boundary tests |

Line coverage is a useful backstop, not the organizing goal. High line coverage
can coexist with weak evidence for convergence, atomicity, or recovery.

## Target Architecture

The target is a functional reconciliation core surrounded by narrow Git,
GitHub, command-line, and hook adapters.

```text
 local Git observation       remote Git observation       GitHub observation
          \                           |                           /
           +-------------------- ObservedWorld -----------------+
                                      |
                              reconcile(world)
                                      |
       Reject | Publish | CreatePullRequests | UpdatePullRequests | Done
                                      |
                              production adapters
```

`ObservedWorld` contains owned, deterministic domain values. It does not retain
repository handles, HTTP clients, locks, or syntax trees from an external
library.

The reconciler derives one safe next action. After an action succeeds, the
caller observes or updates the world and invokes the reconciler again. This is
preferable to planning the entire run at once because external writes can
produce information needed by later actions. For example, GitHub assigns pull
request numbers during creation, and those numbers appear in projected pull
request bodies.

The one-action model also makes recovery explicit. A new process reconstructs
the same world from Git and GitHub and derives the next missing action. It does
not need a separate transaction log or an in-memory notion of what a previous
process intended to do.

Batching is an execution optimization. Batch boundaries and response mapping
must be tested independently, while semantic tests reason about typed actions.

### Domain Values

The core model should distinguish at least:

- local commits, with object ID, GHerrit ID, title, body, and stack position;
- stack visibility and base branch;
- observed managed branches and version tags;
- pull requests keyed by stable GHerrit ID, including lifecycle state;
- desired pull request specifications and minimal update patches;
- ref updates with explicit expected and desired object IDs; and
- typed rejection and ambiguity reasons.

Invalid combinations should be difficult to construct. Test builders may offer
concise scenario syntax, but they should produce the same validated domain
values used by production observation.

### External Boundaries

Use typed ports around meaningful observations and mutations. Do not mock every
call into `gix`, `Command`, Octocrab, or an HTTP client.

The Git boundary should support:

- observing the local stack and branch configuration;
- observing relevant managed branches and tags;
- publishing one atomic batch with explicit leases; and
- recording locally any state required after confirmed publication.

The GitHub boundary should support:

- observing pull requests for a set of GHerrit IDs;
- creating pull requests from complete specifications; and
- applying minimal pull request updates.

Production adapters use real Git and GitHub protocols. Application tests use a
small in-memory world that applies only GHerrit's domain semantics and records
typed effects.

## Test Layers

Choose the lowest layer that can faithfully prove the behavior. A higher layer
is justified only when the boundary itself is material to the claim.

### Pure Model Tests

Most behavior belongs here. These tests use no files, processes, threads,
locks, ports, environment variables, or sleeps.

They cover:

- stack topology and policy;
- pull request rendering and minimal updates;
- publication and version decisions;
- reconciliation ordering;
- batching boundaries;
- idempotence and convergence;
- partial success and restart;
- stale observations and deterministic writer interleavings; and
- exhaustive combinations over bounded small worlds.

Before adding a property-testing dependency, enumerate small state spaces
directly. Stacks of up to three commits combined with absent, matching, and
diverged refs and absent, open, closed, merged, and stale pull requests cover a
large set of meaningful cases while retaining reproducible names and failures.

Universal invariants include:

- rejection produces no mutations;
- GitHub writes never precede required Git publication;
- a converged world produces `Done`;
- applying a successful action makes measurable progress;
- retrying from every committed prefix eventually converges;
- version tags never move;
- pull request bases refer to published branches; and
- merged state is absorbing.

### Adapter Contract Tests

These tests prove translation at a real boundary while keeping domain policy
out of the fixture.

Git contracts use temporary repositories and a real bare remote. They cover
history discovery, ref parsing, atomic pushes, lease conflicts, tag
immutability, configuration, and hook filesystem behavior.

GitHub contracts use a small scripted HTTP transport. A script declares exact
expected requests and explicit responses. Unexpected requests, request-order
violations, and unused expected responses fail automatically. The scripted
transport does not implement pull request semantics; the in-memory model owns
those semantics.

Validate GraphQL documents against the checked-in schema in a dedicated
contract target. Do not make every application scenario parse and execute the
full schema.

### System Tests

System tests are reserved for claims about complete process composition:

- command-line parsing and complete user-visible output;
- hook installation, upgrading, permissions, and argument forwarding;
- one complete successful flow through an installed pre-push hook;
- one installed pre-push rejection that blocks the enclosing push; and
- platform-specific executable discovery and process behavior.

System fixtures should create managed commits and IDs explicitly when hook
behavior is not under test. Installing hooks as incidental fixture setup hides
dependencies and adds unnecessary work.

An optional or scheduled live GitHub smoke test may validate assumptions that
cannot be established locally. It must use an ephemeral repository, clean up
after itself, and never be part of ordinary hermetic test execution.

## Test Doubles and Faults

Use two independent test implementations:

1. `ModelWorld` provides synchronous, in-memory Git and GitHub domain semantics
   and a typed chronological effect trace.
2. `ScriptedHttp` verifies exact GitHub protocol translation.

Do not combine a semantic GitHub simulator, GraphQL interpreter, Git command
interceptor, remote repository, request recorder, and fault injector behind one
shared mutable state object. Such a fake duplicates external behavior, permits
impossible states, and makes policy tests depend on transport details.

Faults should name observable boundaries, for example:

- reject publication because a lease no longer matches;
- lose the response after a successful publication;
- fail observation with a transport error;
- create the first two pull requests, then return a partial response;
- omit a requested GraphQL alias; or
- interrupt immediately after an externally committed effect.

Every configured fault or scripted response must be consumed, or fixture
teardown must fail. Payload text and environment variables must never act as
hidden fault-control channels.

## Observations and Snapshots

Snapshots are preferred for complete text and state that a human intentionally
reviews. It is often difficult to know in advance which detail is load bearing;
a broad, stable snapshot makes all observable changes visible in one diff.

Snapshot meaningful behavior, not incidental fake representation. A canonical
scenario report should contain:

```text
result
  status
  stdout
  stderr

effects, in order
  observations
  published refs and leases
  created pull request specifications
  applied pull request patches

final Git state
  logical commits
  relevant branch configuration
  managed branches
  version tags

final GitHub state
  number, lifecycle state, head, base, title, and complete body
```

Map dynamic values to stable logical names such as `COMMIT_A`, `ID_A`, and
`PR_A`. Omit fabricated user profiles, server ports, temporary paths, and other
details that belong only to a test double.

Structural assertions complement snapshots when they:

- state a universal invariant over generated cases;
- prove that a fixture reached the intended precondition; or
- identify the exact semantic condition responsible for a failure.

They should not replace a complete human-reviewable result with a narrow subset
of fields merely to reduce snapshot churn.

## Hermeticity and Lifecycle Rules

Every spawned command starts from an empty environment and receives an explicit
allowlist. Add a variable only when a test needs it and document why its value
is deterministic and safe. In particular, ordinary tests must not inherit
credentials, proxy settings, Git configuration, locale-dependent behavior, or
live service endpoints.

Use deterministic commit identities and timestamps. Sort observations whose
external protocol does not define order. Never use a sleep to establish that an
event happened; use a typed event, barrier, deadline, or process completion.

Every process and server has a deadline. Teardown must terminate descendants,
join server threads, and report unconsumed expectations. A timeout is a test
failure with enough state to diagnose the stalled boundary.

Avoid shared mutable fixtures. Sharing an immutable parsed schema or executable
artifact can be appropriate after measurement demonstrates a meaningful cost,
but isolation is the default.

## Production Artifact

The production binary should be a thin composition root over a reusable
library. Runtime dependencies such as GitHub endpoints, clients, and ID entropy
should be constructed by the binary and passed explicitly.

If a system test needs deterministic entropy or a scripted endpoint, use a
non-shipping test driver that invokes the same library. Installed-hook tests can
place that driver under the `gherrit` name. The production executable must not
contain a hidden Git mode, deterministic-ID branch, or test-only endpoint
enabled by a build environment variable.

The migration is complete when ordinary `cargo test` and Clippy exercise the
normal production artifact without `GHERRIT_TEST_BUILD`.

## Performance Budget

Measure wall-clock time and critical-path tests, not only the sum of individual
test durations. Parallel system tests contend for process and filesystem
resources, so increasing test threads is not a general performance strategy.

At the start of this migration, a cached full suite takes roughly 48 seconds on
the development machine. The pre-push system target accounts for roughly 34
seconds, and one parameterized repository-URL test can determine the suite's
critical path at over 40 seconds under contention. The pure unit targets finish
in a fraction of a second.

After moving policy into production-used pure planners and deleting redundant
process matrices, the same full command takes 13.6 seconds, including 1.6
seconds of recompilation. Pure library tests take 0.02 seconds and the remaining
pre-push process target takes 3.0 seconds. These are point-in-time development
machine measurements, not looser replacements for the budgets below.

The intended steady-state budget is:

- pure model feedback in well under one second;
- all adapter contracts in a few seconds;
- the full required suite in under 15 seconds on a typical development
  machine; and
- no individual required test responsible for most of the suite critical
  path.

Treat regressions against these budgets as architectural signals. Optimize
after measuring; do not introduce shared mutable state or weaker boundaries for
speculative savings.

## Adding Coverage

When adding a behavior:

1. State the product risk and observable claim.
2. Put semantic combinations and invariants in a pure model test.
3. Add an adapter contract only if new protocol translation is involved.
4. Add a system scenario only if process or hook composition is the subject.
5. Include the new operation in the typed trace and canonical report.
6. Add explicit fault and restart cases for every new committed effect.
7. Review snapshots as behavioral diffs rather than updating them blindly.

When fixing a defect, first add the lowest-layer regression that expresses its
general rule. Retain a higher-level regression only when it protects a distinct
boundary.

## Evidence Ownership

Each behavioral claim has one primary owner. Higher layers may prove that the
parts compose, but they should not repeat the owner's full input matrix.

| Claim | Primary owner | Higher-layer evidence |
| --- | --- | --- |
| Stack topology and policy | Pure model | One real-Git discovery contract |
| GHerrit ID syntax and derivation | Pure function | One installed commit hook |
| Branch-management transitions | Pure function | One hook per process boundary |
| Pull request text and navigation | Pure renderer | One complete lifecycle snapshot |
| Minimal pull request patches | Pure reconciliation | GraphQL encoding contracts |
| Version and publication decisions | Pure reconciliation | Bare-remote ref-state contracts |
| GraphQL request and response shape | Codec contract | One complete system flow |
| Batching boundaries and aliases | Pure batching | One multi-item codec contract |
| CLI diagnostics | Pure typed errors | Focused command snapshots |
| Hook argument forwarding and blocking | Installed-hook system test | None |
| Retry and concurrent-writer behavior | Pure state machine | Focused lease contract |

This table is also a deletion rule. Once the primary owner and the stated
composition evidence exist, another process test needs a distinct boundary
claim to justify its cost.

## Incremental Migration Sequence

The migration should remain reviewable and bisectable. Each increment must
leave the full required suite green, carry its own deletion of superseded
coverage where possible, and avoid creating a second permanent architecture.

### 1. Make the Existing Suite Trustworthy

1. Clear inherited command environments and explicitly allow only deterministic
   variables needed by each process.
2. Replace fixture presets with explicit capabilities such as a repository, a
   bare remote, a GitHub transport, or a Git interceptor.
3. Give every subprocess and server a deadline, deterministic shutdown, and
   descendant cleanup.
4. Replace string- and environment-controlled failures with ordered typed fault
   expectations that fail teardown when unused.
5. Make the GitHub boundary strict: reject unsupported operations, wrong
   repository identities, malformed variables, ambiguous results, and unused
   responses.
6. Build the test driver as an ordinary Cargo artifact and prove that the
   production binary cannot enter its protocol.
7. Add real installed-hook tests for file installation, argument forwarding,
   and failure propagation.

These steps improve evidence without changing the product model and create a
reliable base for larger deletions.

### 2. Move Stable Decisions into Pure Production Code

8. Extract branch-management transitions, checked-out-branch classification,
   GHerrit ID derivation, stack linking, PR-body rendering, autosquash policy,
   batching, and publication refspec construction.
9. Give each extraction exhaustive table tests over its small state space.
10. Retain one process test for every distinct OS, Git, or hook boundary and
    remove process permutations that only repeated the extracted decision.
11. Make repository and PR lookup ambiguity explicit typed errors rather than
    selecting an arbitrary candidate.
12. Make observation failures fail closed; an unavailable remote must never be
    interpreted as an empty remote.

This phase should reduce the process target to a few seconds before introducing
new abstractions.

### 3. Define the Reconciliation Vocabulary

13. Introduce owned domain identifiers and observations for local commits,
    managed refs, immutable versions, pull requests, and stack configuration.
14. Introduce typed outcomes for rejection, publication, PR creation, PR
    update, and convergence.
15. Represent ref publication with explicit old-value expectations and desired
    object IDs. Represent PR updates as minimal patches whose absent fields mean
    no write.
16. Convert adapter-specific values into validated domain observations at the
    edge. The reconciler must not accept HTTP JSON, GraphQL aliases, Git command
    output, or repository handles.
17. Express lifecycle rules, including the absorbing merged state, in the
    domain rather than in transport or orchestration code.

These changes may initially wrap the current control flow. The vocabulary is
complete when every external write can be named without mentioning its
transport.

### 4. Introduce the One-Action Reconciler

18. Implement `reconcile(ObservedWorld) -> Outcome` as a pure function that
    returns either a typed rejection, one next action, or `Done`.
19. Order actions so required Git publication precedes PR creation and PR
    updates. Document every dependency in types or constructors rather than in
    test setup.
20. Apply one confirmed action to an in-memory world and reconcile again.
21. Enumerate bounded worlds of up to three commits and check universal
    invariants: rejection has no effects, successful actions make progress,
    tags never move, merged state is absorbing, and convergence is idempotent.
22. For every action prefix, restart from observation alone and prove eventual
    convergence.
23. Add deterministic stale-observation schedules and two-writer interleavings;
    require either safe progress or a typed conflict followed by reobservation.

At the end of this phase, policy and recovery regressions should require no
filesystem, HTTP server, async runtime, or subprocess.

### 5. Separate Semantic and Protocol Doubles

24. Implement `ModelWorld`, a synchronous semantic oracle that stores domain
    state, applies typed actions, and records a chronological typed trace.
25. Give `ModelWorld` only GHerrit's rules. It must not parse command lines,
    refspec syntax, GraphQL, URLs, or HTTP payloads.
26. Implement `ScriptedHttp` as an ordered sequence of exact request matchers
    and explicit responses. It must not infer pull request semantics.
27. Split any temporary combined fake into focused Git interception, scripted
    HTTP, bare-remote inspection, and process-lifecycle helpers.
28. Remove the in-process GraphQL interpreter once all semantic scenarios use
    `ModelWorld` and all wire-shape cases use codec contracts.

The key deletion gate is independence: changing a domain rule should not
require a transport snapshot update, and changing GraphQL syntax should not
change semantic scenario setup.

### 6. Establish Narrow Adapter Contracts

29. Test local Git observation against temporary repositories, including root,
    merge, reordered, autosquashed, missing-ID, and malformed-ID histories.
30. Test remote observation parsing from complete, empty, malformed, duplicate,
    and failed `ls-remote` results.
31. Test atomic publication against a real bare remote: new refs, branch
    replacement, immutable tag creation, lease conflict, partial rejection, and
    retry after lost acknowledgement.
32. Test every GraphQL operation's exact document, variables, escaping, result
    decoding, partial data, missing aliases, ambiguous nodes, unknown enum
    values, and error envelopes.
33. Test batching separately at zero, one, limit, limit-plus-one, and multiple
    batch boundaries, including response-to-operation mapping.
34. Keep schema validation in a dedicated fast contract target instead of
    reparsing the complete schema for every scenario.

Adapter contracts are complete when an adapter can be replaced without
rewriting domain tests and protocol defects fail without running a full CLI
scenario.

### 7. Reduce System Tests to Composition Evidence

35. Keep one full successful private-stack lifecycle that verifies user output,
    real remote refs, and complete semantic PR state.
36. Keep one drifted retry lifecycle proving that already-published Git state
    still leads to PR repair.
37. Keep one installed pre-push rejection proving that a failed hook blocks the
    enclosing `git push` and leaves the remote unchanged.
38. Keep focused installed-hook tests for commit-message arguments and
    post-checkout argument forwarding.
39. Keep process snapshots only for diagnostics or output whose exact rendering
    is the claim.
40. Delete URL, lifecycle-state, batching-size, projection-field, and fixture
    permutations whose primary evidence now belongs to lower layers.

Every retained system test should state the boundary that prevents it from
moving lower.

### 8. Make the CI Shape Match the Architecture

41. Run formatting, linting, pure model tests, codec tests, adapter contracts,
    and system tests as visibly separate targets.
42. Put the fastest deterministic signal first while preserving required status
    checks for every supported platform.
43. Run ordinary tests without credentials or live endpoints. Put a live GitHub
    canary in an explicitly scheduled or manually invoked workflow with an
    ephemeral repository and guaranteed cleanup.
44. Measure cached and clean wall-clock duration per layer and publish enough
    timing data to identify critical-path regressions.
45. Enforce bounded execution at the outer CI job as well as inside each
    process helper.

### 9. Finish by Deleting Migration Scaffolding

46. Remove compatibility builders, fake state fields, snapshots, feature flags,
    and helper APIs with no remaining callers.
47. Search for tests that install hooks incidentally, inspect fake internals,
    control faults through strings, inherit the host environment, or sleep for
    coordination; each remaining occurrence needs an explicit boundary reason.
48. Re-run the evidence table as an audit: every product risk must have a clear
    primary owner and every expensive test must protect a unique claim.
49. Record final layer timings and update the performance budget from measured
    CI and development-machine results.
50. Treat future additions that bypass the action model or enlarge a semantic
    protocol fake as architecture changes requiring explicit justification.

The target is not the fewest tests. It is the smallest set of independent,
high-information tests that makes unsafe behavior difficult to introduce and
failures cheap to understand.
