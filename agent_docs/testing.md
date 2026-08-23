# Testing Strategy

GHerrit turns a local commit stack into Git refs and GitHub pull requests. Its
tests must establish more than whether individual functions return expected
values: they must show that repeated executions safely converge an external
system, including after interruption and concurrent changes.

This document defines the testing contract, the intended test architecture,
and the criteria for choosing a test layer.

## Goals

The test suite exists to provide confidence in four areas.

### Domain Correctness

The suite must establish that GHerrit:

- discovers the intended local stack and preserves its topology;
- rejects unsupported or unsafe local and remote states before writing;
- publishes each changed revision as an inseparable head, literal-owned-base,
  and immutable-version tuple;
- records established pull-request existence in a separate immutable marker
  whose acknowledged Git publication gates final GitHub projection;
- projects each commit into the correct pull request title, body, head, base,
  and navigation links, with a root on the default branch and each nonroot on
  its own base;
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
- **Fast:** focused pure tests provide an interactive feedback loop without
  running the exhaustive recovery proof or starting external boundaries.
- **Fail-closed:** unsupported fake operations, unexpected requests, and
  unconsumed expectations fail the test.
- **Understandable:** a failure identifies the violated behavior and the layer
  responsible for it.
- **Extensible:** ordinary policy, formatting, parsing, and adapter changes add
  focused tests. Only changes to publication actions, durable effects,
  visibility schedules, or restart reasoning extend the recovery oracle.
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

## Architecture

The publication planner is a pure core surrounded by narrow Git, GitHub,
command-line, and hook adapters. One attempt assembles a complete
attempt-scoped evidence set from independently timed reads and invokes that
planner once. "Complete" means that each required namespace, history, and
paginated connection was covered; it does not mean snapshot isolation across
Git, GitHub, or observation waves. Safety comes from validated histories and
markers, exact leases and acknowledgements, and fail-closed joins, not from
cross-system snapshot isolation.

```text
global heads -> local stack -> local histories and graph ---------\
complete repository-wide OPEN GraphQL observation -> correlation --+
nonlocal histories and graph + terminal evidence ------------------+
                                                                  |
                                                        publication plan
                                                                  |
atomic head/base/version tuples -> GraphQL creates -> marker batch
                                                    -> final GraphQL updates
```

The evidence and plan contain owned, deterministic domain values. They do not
retain HTTP JSON, GraphQL aliases, locks, or freely recombinable authorization
state. Destination-bound values keep Git writes, GitHub coordinates, and body
links tied to the same selected push destination.

Typed transitions enforce the two Git barriers. Exact acknowledgement of all
initial tuple pushes releases pull-request creation. Every marker request is
fully preflighted before the first write. Validated initial OPEN evidence
authorizes an existing pull request's marker immediately, while exact complete
create receipts authorize the already-preflighted marker requests for newly
created pull requests. Exact acknowledgement of all marker pushes releases the
final body and base updates. An indeterminate write ends the attempt; the same
attempt neither retries the write nor reobserves external state.

A later process reconstructs current evidence from Git and GitHub and derives
only the remaining work. It needs no transaction log or remembered prior plan.
Adapters batch independent observations and effects, but never split one
change's three-ref publication tuple. Batch boundaries and receipt mapping are
tested independently, while semantic tests reason about typed actions and
durable prefixes.

### Domain Values

Throughout this document, "typed" means a dedicated Rust struct or enum with
validated, structured fields, rather than a boolean, formatted string,
unvalidated JSON value, or transport-specific representation.

The core model should distinguish at least:

- local commits, with object ID, GHerrit ID, title, body, and stack position;
- stack visibility and base branch;
- destination-scoped observations which couple each active change to its
  managed head, owned base, and complete immutable version history;
- optional immutable pull-request markers whose targets belong to the
  validated published history;
- pull requests keyed by stable GHerrit ID, including lifecycle state;
- desired pull request specifications and minimal update patches;
- whole publication tuples and marker operations with explicit expected and
  desired object IDs; and
- outcomes which drive production branching, retry classification, or
  authority transitions, represented by dedicated Rust structs or enums with
  validated structured fields.

Terminal validation failures on which production never branches may use a
`color_eyre::Report`. Their contract is bounded, actionable user-facing text,
owned by focused assertions or reviewed snapshots. Introduce a structured
rejection taxonomy only when a production consumer needs to inspect it.

Invalid combinations should be difficult to construct. Test builders may offer
concise scenario syntax, but they should produce the same validated domain
values used by production observation.

### External Boundaries

Use typed ports around meaningful observations and mutations. Do not mock every
call into `gix`, `Command`, Octocrab, or an HTTP client.

The Git boundary should support:

- observing the local stack and branch configuration;
- observing the default branch and every remote head in one global request;
- observing exact active tag namespaces in separate byte-bounded requests;
- publishing bounded atomic batches of inseparable head, literal-owned-base,
  and immutable-version tuples with exact leases; and
- publishing separate create-only pull-request marker batches after GitHub has
  established the corresponding pull requests.

Remote immutable tags are version authority. The boundary must not consult or
persist local version tags: tests must produce the same next version from a
fresh clone, a clone with stale local tags, and a split fetch/push remote.
Ordinary attempts use two Git reads. Observation work scales with all heads
plus active immutable histories, not every historical version tag.

The GitHub boundary should support:

- completely paginating the repository-wide OPEN connection and correlating
  managed identities only after local Git evidence is ready;
- exhaustively observing terminal history only for missing local identities;
- creating pull requests from complete specifications on the stable owned-base
  key; and
- applying the final minimal GraphQL projection, including default base for a
  root and the change's own base for a nonroot.

Production adapters use real Git and GitHub protocols. Pure planner tests use
focused domain fixtures and a dedicated finite-state semantic recovery oracle.
Process tests compose real repositories and Git subprocesses with a strict
schema-validating GitHub fake.

## Test Layers

Choose the lowest layer that can faithfully prove the behavior. A higher layer
is justified only when the boundary itself is material to the claim.

### Focused Pure Tests

Most local rules belong here. These tests use no files, processes, threads,
locks, ports, environment variables, or sleeps, and they run independently of
the exhaustive recovery proof.

They cover:

- stack topology and policy;
- pull request rendering and minimal updates;
- parsing and exact text equivalence;
- local owned-tuple, version, and marker decisions;
- batching boundaries;
- action masks; and
- bounded error formatting.

### Exhaustive Semantic Recovery Proof

The named test
`owned_base_and_marker_publication_exhaustively_survive_restarts` compares an
independent finite-state model with the production planner. It covers bounded
topologies, typed barrier ordering, durable effect prefixes, partial success,
visibility schedules, acknowledgements, restarts, and convergence. It is
required in CI and before publication, but it is deliberately separate from
the interactive focused-test loop.

Before adding a property-testing dependency, exhaustively enumerate bounded
small state spaces directly. Stacks of up to three commits combined with absent,
matching, and diverged refs and absent, open, closed, merged, and stale pull
requests cover a large set of meaningful cases while retaining reproducible
names and failures.

Universal invariants include:

- rejection produces no mutations;
- GitHub creates never precede exact acknowledgement of required tuple
  publication;
- final GraphQL updates never precede exact acknowledgement of required marker
  publication;
- a converged world produces no action;
- applying a successful action makes measurable progress;
- retrying from every committed prefix eventually converges;
- version tags never move;
- one change's head, owned base, and new version tag are never split;
- every created pull request uses its own published base;
- every converged root uses the exact default base and every converged nonroot
  uses its own base; and
- merged state is absorbing.

### Adapter Contract Tests

These tests prove translation at a real boundary while keeping domain policy
out of the fixture.

Git contracts use temporary repositories and a real bare remote. They cover
history discovery, ref parsing, atomic three-ref tuple pushes, exact head/base
leases, version and marker immutability, separately batched marker pushes,
lost or malformed porcelain acknowledgements, configuration, and hook
filesystem behavior. Assertions inspect the real bare remote's object IDs;
command snapshots alone are not evidence that tuple atomicity held.

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
- representative one-change, two-change, and mixed established/new
  publication through an installed pre-push hook;
- exact remote head, literal-owned-base, immutable-version, and optional marker
  object IDs after real Git pushes;
- root/default and nonroot/self-owned pull-request bases after the GraphQL
  projection;
- one installed pre-push rejection that blocks the enclosing push without a
  Git or GitHub write;
- focused recovery across lost tuple, create, and marker acknowledgements; and
- platform-specific executable discovery and process behavior.

System fixtures should create managed commits and IDs explicitly when hook
behavior is not under test. Installing hooks as incidental fixture setup hides
dependencies and adds unnecessary work.

An optional or scheduled live GitHub smoke test may validate assumptions that
cannot be established locally. It must use an ephemeral repository, clean up
after itself, and never be part of ordinary hermetic test execution.

## Test Doubles and Faults

Keep three evidence roles distinct:

1. The planner's synchronous semantic world applies typed Git and GitHub
   effects and records their chronological trace. It owns exhaustive policy
   and recovery combinations.
2. The strict GitHub fake validates GraphQL documents and schemas, projects
   independent stored pull-request fields, enforces duplicate creation by the
   complete same-repository head/base key, and records ordered requests.
3. Temporary repositories, a real bare remote, and the bounded Git interceptor
   prove command and ref effects at the actual Git boundary.

The GitHub fake must not become an alternate planner, and the semantic world
must not parse GraphQL, Git commands, URLs, or HTTP payloads. A request log can
prove ordering and wire shape; only real remote inspection proves Git ref
effects.

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
  published tuple refs and leases
  created pull request specifications
  published pull request markers and leases
  applied pull request patches

final Git state
  logical commits
  relevant branch configuration
  managed head and owned-base branches
  immutable version tags and pull request markers

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

The feature-gated test driver is a separate Cargo binary target. The normal
production binary remains on the same all-feature test graph, and a dedicated
regression proves that it rejects the driver protocol. No build environment
variable changes production control flow.

## Performance Tiers and Baselines

Measure wall-clock time and critical-path tests, not only the sum of individual
test durations. Parallel system tests contend for process and filesystem
resources, so increasing test threads is not a general performance strategy.

Measure focused feedback, the exhaustive semantic proof, adapter/process
groups, and the full gate separately. A slow proof must not silently become
part of every interactive command, and a fast focused subset is not evidence
that the recovery proof ran.

On the 2026-08-23 macOS reference host, with warm artifacts, one Cargo build
job, and one libtest thread, the exact activation tip measured the 36-test
planner group including the oracle at 30.98 seconds, the complete pre-push
process target at about 199 seconds, and the complete workspace at about 256
seconds. Repeated warm oracle runs took 32.19–36.40 seconds with 10 Rayon
workers and about 186–198 CPU seconds. These are dated same-host baselines, not
portable pass/fail thresholds.

Comparable validation receipts must record the toolchain, Cargo job count,
test and Rayon worker counts, warm or cold state, selected layer, and wall
time. A material regression requires measurement and an explanation. Optimize
without weakening the state space, hermeticity, or boundary fidelity.

## Adding Coverage

When adding a behavior:

1. State the product risk and observable claim.
2. Put a local semantic rule in a focused pure test. Update the exhaustive
   oracle only when the publication state machine, a durable effect, a
   visibility schedule, or the recovery proof changes.
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
| Minimal pull request patches | Pure planner | GraphQL encoding contracts |
| Owned-tuple, marker, and version decisions | Pure planner | Bare-remote ref-state contracts |
| GraphQL request and response shape | Codec contract | One complete system flow |
| Batching boundaries and aliases | Pure batching | One multi-item codec contract |
| Terminal validation and CLI diagnostics | Focused validation/formatting tests | Focused command snapshots |
| Hook argument forwarding and blocking | Installed-hook system test | None |
| Retry and concurrent-writer behavior | Semantic world | Focused lease contract |

This table is also a deletion rule. Once the primary owner and the stated
composition evidence exist, another process test needs a distinct boundary
claim to justify its cost.

## Required Publication Evidence

The suite treats the owned publication representation as the sole active
representation. Ordinary fixtures contain no compatibility representation,
and no process matrix exists only to repeat pure planner combinations.

### Pure Semantic Evidence

The dedicated recovery oracle owns exhaustive combinations of bounded
histories and durable effect prefixes. It models each published revision as
exactly:

```text
refs/heads/G                 -> H
refs/heads/gherrit-bases/G   -> first_parent(H)
refs/tags/gherrit/G/vN       -> H
```

The tuple is one indivisible action. The independently optional
`refs/tags/gherrit/G/pr` marker is a later create-only action, never a tuple
member. Pure tests enumerate acknowledged tuple and marker batches, every
subset of complete aliased GraphQL operations, restart from each durable
prefix, and prove that no partial title/body/base application exists within one
alias.

Focused pure tests separately own exact body text, rendering, parser syntax,
wire encoding, and local batching boundaries. Those details do not multiply
the recovery oracle's state space unless they change a durable action or
visibility schedule.

Planner coverage owns the full root/nonroot, lifecycle, automation, ordering,
and recovery relation matrices. In particular, it proves that every create
uses the permanent same-repository `G`/`gherrit-bases/G` key; a final root
uses the exact default branch; a final nonroot uses its own base; and a marker
barrier is required before either final projection becomes available.

### Real Git-Boundary Evidence

Git-boundary fixtures seed real commit objects first, then establish a
published tuple in one ref transaction. The fixture accepts the literal base
object ID and the independently optional marker target. It must not infer a
base from another change's current head or infer a marker from pull-request
state.

After publication, tests inspect the real bare remote and assert all three
tuple refs and their exact object IDs. For a changed revision, they also assert
that one atomic push contains the complete tuple and that no bounded batch
splits it. Marker tests inspect the separate create-only ref and its absence
lease, including multiple markers in one bounded batch.

Lost-acknowledgement tests use a real successful atomic push followed by
replaced or malformed porcelain. They prove that the remote contains either
the complete tuple or no tuple, never a partial tuple, and that no GitHub write
crosses an unacknowledged Git barrier. A fresh invocation supplies convergence
evidence. An intercepted command line or a fake request log is useful trace
evidence, but it is not a substitute for inspecting the actual remote refs.

Malformed-state tests deliberately use low-level ref helpers to construct an
incomplete head/base/version representation. They assert fail-closed behavior
and exact absence of Git and GitHub writes.

### GitHub Protocol Evidence

The scripted GraphQL transport proves complete OPEN pagination, exact
repository coordinates, terminal lookup only for missing local identities,
stable owned-base creates, complete alias receipts, and minimal final updates.
Stored fake pull requests keep base name and base object ID independent and
model auto-merge and merge-queue state independently.

A one-shot visibility expectation may hide a known OPEN pull request from
exactly one complete scan without removing it from fake state or duplicate-key
enforcement. This proves that a lost create acknowledgement repeats the stable
owned-base key without creating a duplicate, and that a present durable marker
suppresses creation even when OPEN omits the row.

### Complete-Process Composition

Retained process scenarios prove that the production artifact composes the
boundaries:

1. One-change, two-change, and a small mixed established/new stack publish
   complete tuples. Direct ref assertions verify every literal first parent.
   GitHub state verifies the root/default and nonroot/self-owned final bases.
2. A representative lifecycle trace shows tuple publication, GraphQL creation,
   marker publication, and final GraphQL projection in that order.
   The shared fake records schema-validated GraphQL request receipt and Git
   push completion callbacks under one lock. This is a chronological fake-side
   observation, not proof that the client consumed either response; universal
   barrier ordering comes from the one-use production types.
3. Incomplete head/base/version state and owned-base automation violations
   reject before every write and preserve all existing refs and pull requests.
4. Lost tuple, create, and marker acknowledgements leave safe durable prefixes;
   fresh invocations converge without a same-attempt confirmation read or
   mutation retry.
5. Cancellation-safe gates prove that global heads and the first OPEN page
   start concurrently, local history work can overlap held OPEN pagination,
   and an empty local stack drops its held OPEN read. The nonlocal-after-OPEN
   relation is structural: nonlocal IDs exist only after consuming correlated
   OPEN evidence, and the additional history future borrows exactly those IDs.

Comprehensive readable snapshots show complete results, traces, refs, and pull
requests. Exact structural assertions remain mandatory for object IDs,
whole-tuple atomicity, barrier order, and absence of writes.
