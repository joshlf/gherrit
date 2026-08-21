# Testing strategy

GHerrit turns one local commit stack into remote Git refs and GitHub pull
requests. The test suite must establish more than whether individual functions
return expected values. It must show that every durable prefix is safe, a fresh
attempt derives only remaining work, and protocol adapters faithfully carry
the pure model across real boundaries.

This document defines the testing contract, architecture, and criteria for
choosing a test layer.

## Goals

The suite provides confidence in domain behavior, adapter behavior,
user-visible behavior, and its own operational quality.

### Domain correctness

The suite establishes that GHerrit:

- derives the intended ordered local stack from the exact default branch;
- returns for an empty stack before GitHub authentication or requests;
- observes and validates only Git and GitHub state belonging to local change
  IDs;
- represents each revision as one inseparable head, literal-owned-base, and
  immutable-version tuple;
- records established pull request existence with a separate immutable marker;
- withholds final pull request projection until every required marker is
  exactly acknowledged;
- creates every pull request on its permanent owned-base key;
- converges roots to the exact default branch and nonroots to their own bases;
- emits no action when durable state already matches local intent;
- remains safe after interruption at every externally committed effect;
- remains safe when protocol-conforming publishers operate on disjoint or
  overlapping change IDs;
- fails closed when marked pull request existence is not visible;
- aggregates and rejects closed or merged local history when no OPEN pull
  request exists; and
- preserves every immutable version position through amendments, rebases,
  reorders, adjacent repeated revisions, and nonconsecutive revision reuse.

Tests cover protocol-conforming concurrent publishers: disjoint publications,
already-desired tuples and markers, identical tuple pushes whose planned
leases became stale, conflicting tuples, same-key creates, and different text
projections for one durable revision. They do not claim serializability
against writers which bypass the operating assumptions in [the pre-push
design](../design/pre-push.md). A test must not imply that an extra read closes
a time-of-check/time-of-use race.

### Adapter correctness

The suite establishes that:

- local Git history and branch state are derived accurately;
- remote Git observation requests only the exact default and local change
  namespaces;
- exact absence, immutable history, and marker state survive byte parsing;
- Git publication uses complete atomic tuples and exact leases;
- GitHub observes fully paginated all-state connections for the exact local
  IDs;
- GraphQL aliases, cursors, errors, nullable fields, and partial mutation
  outcomes are decoded conservatively;
- create and update receipts identify the exact planned pull requests;
- installed hooks are complete, executable, and forward exact arguments; and
- supported platforms agree on process, path, and filesystem behavior.

### User-visible behavior

The suite protects:

- exit status, standard output, and standard error;
- bounded, actionable diagnostics for rejected or ambiguous operations;
- the guarantee that a failed pre-push hook blocks the enclosing Git push;
- complete human-readable pull request bodies and navigation; and
- the absence of GitHub credential prompts or GitHub errors for an empty
  stack.

Pull request body snapshots also prove that no hidden `gherrit-meta` footer is
emitted. Commit-body tests accept that text as ordinary user content. No test
models the disabled auto-cascade Action as a production consumer.

### Operational quality

The test infrastructure is:

- **Hermetic:** ordinary tests inherit no credentials, user configuration,
  proxy settings, network endpoints, or unrelated environment variables.
- **Deterministic:** time, IDs, commit metadata, ordering, visibility, and
  injected failures are controlled explicitly.
- **Bounded:** subprocesses, servers, reads, writes, and teardown have finite
  deadlines.
- **Fast:** pure planning and recovery tests provide an interactive feedback
  loop without processes, sockets, or filesystems.
- **Strict:** unexpected fake operations, malformed requests, and unconsumed
  expectations fail the test.
- **Understandable:** a failure identifies the behavior and evidence layer
  which disagreed.
- **Extensible:** local rules add focused tables; new durable effects or
  visibility schedules extend one small recovery model; wire changes add
  adapter contracts.
- **Faithful:** tests use production values and the production executable at
  boundaries where those artifacts are the subject of the claim.

These properties are correctness requirements. A broad test which flakes,
silently accepts an unexpected request, or takes too long to run routinely is
weak evidence even when it covers many lines.

## Product risk model

Coverage is organized around product risks rather than source files.

| Risk | Primary evidence |
| --- | --- |
| Local intent is misunderstood | Pure stack/policy cases and focused real-Git discovery |
| Unrelated repository state affects publication | Exact-request contracts and unrelated-state invariance |
| The wrong refs are published | Pure tuple decisions and real atomic-push/lease contracts |
| GitHub state is misclassified | Pure local outcome tables and all-state GraphQL contracts |
| A create targets the wrong PR | Exact receipt tables and one scripted mutation contract |
| A retry repeats or loses work | Durable-prefix recovery model and focused lost-ack contracts |
| A stale read grants unsafe authority | Visibility schedules and marker-aware planner tables |
| Concurrent publishers overlap | Exact-lease, same-key, and recovery cases |
| A hook does not enforce the workflow | Installed-hook success and blocking system tests |
| A protocol assumption changes | Scripted adapter contract or optional live smoke test |
| Platform behavior differs | Cross-platform adapter and process-boundary tests |

Line coverage is a backstop, not the organizing goal. High line coverage can
coexist with weak evidence for atomicity, authority transitions, or restart
convergence.

## Architecture

The pure planner is surrounded by narrow local-Git, remote-Git, GitHub,
command-line, and hook adapters. One attempt supplies one exact local evidence
set and invokes the planner once.

```text
remote symbolic HEAD -> local stack -> empty? -> return
                              |
                              +-> exact local refs, tags, and graph ----\
                              +-> exact local all-state PR connections --+
                                                                        |
                                                              publication plan
                                                                        |
atomic head/base/version tuples -> GraphQL creates -> marker batches
                                                   -> final GraphQL updates
```

"Exact" means that every requested Git name or namespace and every requested
GraphQL connection is covered. It does not mean that Git and GitHub form a
snapshot. Safety comes from validated immutable history, exact leases, stable
create keys, durable markers, and one-use acknowledgement gates.

The evidence set contains no repository-wide OPEN rows, nonlocal histories,
nonlocal graph roots, or separate terminal lookup table. Each local GraphQL
connection includes OPEN, CLOSED, and MERGED. Complete pagination produces one
OPEN value, one sealed absence proof, or a terminal-only rejection before the
planner runs.

Exact acknowledgement of all required tuple pushes releases pull request
creation. An initially observed valid OPEN pull request authorizes its marker;
a newly created pull request authorizes its marker only through an exact create
receipt. Exact acknowledgement of every marker push releases final projection.

An indeterminate write ends the attempt. The same attempt does not retry the
mutation, roll back, or reobserve. A fresh process reconstructs all authority
from durable Git and GitHub evidence.

### Domain values

"Typed" means a dedicated validated Rust struct or enum, not a boolean,
formatted command, raw JSON value, or detachable map entry.

The pure core distinguishes:

- default branch name and object ID;
- ordered local changes, each with change ID, head, literal first parent,
  title, and body;
- absent or nonempty local published histories;
- exact current heads, owned bases, immutable versions, and optional markers;
- one exact planner input: sealed `Absent` or validated `Open`;
- pull request identities as coupled number and GraphQL node ID values;
- desired complete projections and minimal update masks;
- whole tuple, create, marker, and update effects with stable change IDs; and
- one-use stage values which encode acknowledgement authority.

Invalid combinations should be impossible or private to a boundary decoder.
Tests may use concise builders, but those builders produce the same validated
values consumed by production planning.

Terminal failures on which production does not branch may use
`color_eyre::Report`. Their contract is bounded user-facing text. Add a
structured rejection enum only when production behavior needs to inspect it.

### External boundaries

Use typed ports around meaningful observations and effects. Do not mock every
call into `gix`, `Command`, Octocrab, or the HTTP client.

The Git boundary owns:

- symbolic remote default discovery;
- exact named default and local head/base observation;
- exact local version and marker namespace observation;
- bounded acquisition from advertised local version refs;
- atomic publication of complete three-ref tuples with exact leases; and
- separate create-only marker publication with absence leases.

The GitHub boundary owns:

- repository identity and default-branch observation;
- independently paginated all-state connections filtered by exact local head
  names;
- conversion of each exhausted connection into sealed `Absent`, validated
  `Open`, or an aggregated terminal-only rejection;
- stable owned-base create mutations and exact receipts; and
- minimal final update mutations and exact receipts.

The production adapters use real Git and GitHub protocols. Pure tests use
validated domain fixtures and a literal durable-state model. Process tests
compose real repositories and Git subprocesses with a strict schema-validating
GitHub fake.

## Test layers

Choose the lowest layer which can faithfully prove the claim. A higher layer
is justified only when composition or the boundary itself matters.

### Focused pure tests

Most rules belong here. These tests use no files, processes, ports, threads,
environment variables, locks, or sleeps.

They cover:

- stack topology, identity, and policy;
- version normalization and reachability;
- local pull request outcome tables;
- marker-aware creation authority;
- pull request rendering and complete text equivalence;
- minimal update masks;
- tuple and marker decisions;
- batching boundaries over typed effects; and
- bounded error formatting.

Use tables or direct bounded enumeration when they make the valid product
clear. A table row should describe a real semantic combination, not a freely
recombinable set of booleans which admits impossible states.

### Bounded semantic recovery model

The recovery model compares the production planner with an independent literal
world. It deliberately knows nothing about refspecs, `ls-remote`, GraphQL,
aliases, JSON, URLs, or HTTP.

Its durable world and process-local intent are separate:

```text
DurableWorld
  default tip
  published changes by stable ID
  literal pull request rows

PublishedChange
  ordered published revisions
  independently optional marker target

LocalIntent
  ordered local changes

LocalChange
  stable ID
  desired revision and literal first parent
  title and body
```

The marker and pull request are stored independently because they are durable
effects in different systems. Reachable recovery worlds enforce that a marker
was authorized by a durable OPEN identity; an observation may still hide that
row. Terminal-only exact connections are rejected during correlation, before
a planner value or recovery world exists. Focused correlation tests own those
external lifecycle combinations. A marker may target an older published
revision after an amendment. OPEN pull request rows retain identity, exact ref
and object values, and literal projection bytes. The model can therefore prove
that a complete body patch reaches an exact no-action retry without deriving
desired state inside the fake world.

The production planner exposes typed test effects:

```text
Git tuples | Creates | Markers | Updates | Done | Rejected
```

Every effect carries the stable change ID and its complete semantic payload.
The model compares exact effect order and content, applies an allowed durable
prefix or alias subset, discards all process-local authority, rebuilds a fresh
observation, and requires the next plan to describe exactly the remaining
work.

Concurrency cases derive two plans from independently chosen observations and
interleave their complete external effects. The durable model applies Git
effects only when every exact lease still matches, creates at most one OPEN
pull request for a stable creation key, and stops an attempt whose effect is
rejected or indeterminate. It then retries the selected stable intent from a
fresh observation. This explores concurrency without giving either planner
access to the other's process-local state.

Required bounded scenarios include:

1. A fresh root: tuple, create, marker, final base/body update, then `Done`.
2. Two missing pull requests: every meaningful subset of complete create
   aliases, then every marker and update prefix.
3. Amendment and reorder: an old marker target remains immutable while a new
   version and changed local position converge without recreating or
   remarking an established pull request.
4. Visibility: hiding an unmarked provisional OPEN row repeats only the stable
   create key; hiding a marked OPEN row rejects without a create.
5. Concurrent publishers: disjoint changes commute; a tuple or marker which
   another publisher applied is already complete on retry; identical tuple
   pushes from the same observation both receive usable acknowledgements even
   though only one changes the refs; pushes with different desired tuples from
   the same observation admit one exact-lease winner and stop the loser;
   simultaneous creates use one stable key; and different navigation
   projections for the same tuple are safe last-writer-wins updates which
   converge after one local intent stabilizes.

Universal invariants include:

- rejection exposes no mutation;
- GitHub creates never precede exact tuple acknowledgement;
- final updates never precede exact marker acknowledgement;
- a converged world produces `Done`;
- applying a successful required effect makes measurable progress;
- retrying every reachable durable prefix converges after intent stabilizes;
- every interleaving of protocol-conforming effects preserves safety;
- immutable tags never move;
- one tuple never splits its head, owned base, and version;
- each create uses its own published base;
- a final root uses the exact default and a nonroot uses its own base; and
- no model transition reads or mutates a nonlocal change.

Before adding a property-testing dependency, prefer direct exhaustive
enumeration of the small meaningful state space. Named deterministic failures
are easier to reproduce and review.

### Adapter contract tests

Adapter tests prove translation at one real boundary while leaving domain
policy in pure tests.

#### Git contracts

Use temporary repositories and a real bare remote to cover:

- symbolic default discovery and exact target agreement;
- exact local head, owned-base, version, and marker namespace parsing;
- authoritative absence for requested names;
- absence of unrelated head or tag requests;
- source-ref-only object acquisition without local ref effects;
- complete atomic tuple pushes and exact head/base leases;
- immutable version and marker absence leases;
- identical competing tuple pushes where one changes the refs and both receive
  exact usable acknowledgements despite the second push's stale leases;
- conflicting competing tuple pushes where one wins and the other receives an
  exact-lease rejection;
- byte-bounded batches which never split a tuple;
- malformed or missing porcelain acknowledgements; and
- destination, environment, timeout, and hook filesystem behavior.

Assertions inspect the bare remote's literal object IDs. A command snapshot
alone does not prove that atomicity or leases held.

Remote versions are authoritative. Tests must derive the same next version
from a fresh clone, a clone with stale local tags, and split fetch/push remote
configuration.

#### GitHub contracts

Use a small scripted HTTP transport. Each script declares exact requests and
responses. Unexpected requests, wrong order, missing requests, and unconsumed
responses fail automatically.

Contracts cover:

- one all-state alias per exact local head name;
- repository facts on the first request only;
- independent pagination where aliases advance at different rates;
- fork-only pages followed to exhaustion;
- sealed absence plus OPEN, CLOSED, and MERGED decoding;
- one OPEN plus older terminal rows selecting OPEN;
- multiple terminal rows producing one aggregated rejection;
- multiple OPEN rows and repeated local identities rejecting ambiguity;
- wrong returned head names and object IDs;
- missing, null, duplicate, and extra aliases;
- repeated, empty, and missing continuation cursors;
- fatal partial data plus errors;
- resource-limit backoff without consuming a cursor;
- bounded transient query retry;
- stable owned-base create documents;
- simultaneous same-key creates where at most one OPEN pull request is
  created;
- exact create-key and object-ID receipts;
- receipt identity uniqueness scoped to the exact local rows and create
  aliases retained by one attempt;
- minimal update documents and exact identity receipts; and
- arbitrary subsets of complete nullable mutation aliases.

Validate documents against the checked-in GitHub schema in a dedicated
contract target. Ordinary application scenarios should not repeatedly parse
the full schema.

### System tests

System tests are reserved for complete process claims:

- command-line parsing and user-visible output;
- hook installation, upgrade, permissions, and argument forwarding;
- successful empty-stack push without token lookup or GitHub request;
- representative one-change, two-change, and mixed established/new stacks;
- literal remote head, owned-base, immutable-version, and marker object IDs;
- root/default and nonroot/self-owned final pull request bases;
- one installed-hook rejection which blocks the enclosing push before every
  external write;
- focused recovery after lost tuple, create, and marker acknowledgements;
- one overlapping two-publisher race where an exact-lease loser performs no
  later GitHub write; and
- platform-specific executable discovery and process behavior.

System fixtures create managed commits and IDs directly when hook behavior is
not the claim. Installing hooks as incidental setup hides dependencies and
adds work.

An optional scheduled live GitHub smoke test may validate an operating
assumption which cannot be proved locally, especially same-key duplicate
refusal. It uses an ephemeral repository, cleans up after itself, and is never
part of ordinary hermetic execution.

## Test doubles and fault injection

Keep three evidence roles separate:

1. The semantic model compares literal durable state with typed planner
   effects.
2. The strict GitHub fake validates schema-conforming requests, applies
   independent stored pull request effects, enforces the assumed complete
   same-repository creation key, and records ordered requests.
3. Real temporary repositories, a bare remote, and the bounded Git interceptor
   prove Git commands and ref effects.

The fake must not become an alternate planner. It stores literal fields and
implements only external service semantics needed by the contract. It does not
derive desired bases, bodies, versions, or marker work from local intent.

Faults name observable boundaries, for example:

- reject a tuple because an exact lease changed to a different object;
- apply a Git push but lose or corrupt its acknowledgement;
- fail one observation request;
- omit a requested alias;
- apply a chosen subset of complete create or update aliases;
- hide one known OPEN row for one observation; or
- interrupt immediately after one externally committed effect.

Every configured fault and scripted response must be consumed. Payload text,
environment variables, and timing sleeps must not act as hidden fault-control
channels.

## Observations and snapshots

Snapshots are preferred for complete text and state which humans intentionally
review. It is often impossible to know in advance which detail is load bearing.
A broad stable snapshot exposes every observable change in one diff that a
reviewer can accept or reject.

Snapshot meaningful behavior rather than incidental fake representation. A
canonical scenario report contains:

```text
result
  status
  stdout
  stderr

effects in order
  exact observations
  tuple refs and leases
  create specifications
  marker refs and leases
  update patches

final Git state
  logical commits
  relevant branch configuration
  local managed heads and owned bases
  immutable versions and markers

final GitHub state
  local PR number, lifecycle, head, base, title, and complete body
```

Map dynamic values to stable logical names such as `COMMIT_A`, `ID_A`, and
`PR_A`. Omit fabricated user profiles, ports, temporary paths, and values which
exist only because of the fake.

Structural assertions complement snapshots when they:

- state a universal invariant over enumerated cases;
- prove that a fixture reached the intended precondition; or
- identify the semantic reason for a failure.

They do not replace a complete human-reviewable result with a narrow field
subset merely to reduce snapshot churn.

## Hermeticity and lifecycle rules

Every spawned test command starts from an empty environment and receives an
explicit allowlist. Add a variable only when a test requires it and document
why its value is deterministic and safe. Ordinary tests never inherit:

- GitHub or other credentials;
- Git, proxy, or credential-helper configuration;
- locale-dependent behavior;
- live network endpoints; or
- repository-redirection and object-database controls.

Use deterministic commit identities, timestamps, and IDs. Sort only where the
external protocol does not define order.

Never sleep to prove that an event happened. Use a typed event, barrier,
deadline, or process completion. Every command and server has a deadline.
Teardown terminates descendants, joins server tasks or threads, and reports
unconsumed expectations.

Avoid shared mutable fixtures. An immutable parsed schema or executable may be
shared only after measurement shows a meaningful cost and its lifetime remains
outside mutable test state.

## Production artifact

The production binary is a thin composition root over a reusable library.
Runtime dependencies such as GitHub clients, endpoints, and ID entropy are
constructed at the boundary and passed explicitly.

System tests which require deterministic entropy or a scripted endpoint use a
non-shipping test driver which invokes the same library. Installed-hook tests
may place that driver under the `gherrit` name. The production executable does
not contain a hidden Git mode, deterministic-ID path, or environment-selected
test endpoint.

The feature-gated driver is a separate Cargo binary target. The normal
production binary remains in the all-feature build graph, and a regression
proves that it rejects the driver protocol.

## Performance budget

Measure wall-clock critical path, not only the sum of test durations. Parallel
process tests contend for CPU, filesystems, and server resources; increasing
test threads is not a general optimization.

The steady-state targets are:

- pure planner and recovery feedback well under one second;
- adapter contracts in a few seconds;
- the complete required suite under 15 seconds on a typical development
  machine; and
- no individual required test responsible for most of the suite critical
  path.

Track point-in-time measurements when changing architecture, but do not turn
one machine's result into a looser budget. A regression against these targets
is an architectural signal. Optimize after measuring and do not weaken
isolation or strictness for speculative savings.

## Adding coverage

When adding behavior:

1. State the product risk and observable claim.
2. Put a semantic rule in the focused pure layer.
3. Update the recovery model only when a durable effect, authority barrier,
   visibility schedule, or restart rule changes.
4. Add an adapter contract only when protocol translation changes.
5. Add a system scenario only when process or hook composition is the claim.
6. Include every new durable operation in the typed trace.
7. Add restart cases for every new committed effect.
8. Review snapshots as behavioral diffs rather than updating them blindly.

For a defect, first add the lowest-layer regression which expresses the
general rule. Retain a higher-level regression only when it protects a distinct
boundary.

## Evidence ownership

Each claim has one primary owner. Higher layers prove composition without
repeating the primary owner's full matrix.

| Claim | Primary owner | Composition evidence |
| --- | --- | --- |
| Stack topology and policy | Pure model | One real-Git discovery contract |
| GHerrit ID syntax | Pure function | One installed commit hook |
| Empty stack avoids GitHub | Orchestration unit | One installed-hook scenario |
| Exact local Git names | Git adapter | One complete publication trace |
| Version and marker normalization | Pure planner | Bare-remote ref assertions |
| Pull request history classification | Pure table | GraphQL pagination contracts |
| Pull request text and navigation | Pure renderer | One lifecycle snapshot |
| Minimal update masks | Pure planner | GraphQL encoding contract |
| Tuple and marker decisions | Pure planner | Real atomic-push contracts |
| Restart convergence | Semantic world | Focused lost-ack composition |
| Publisher concurrency | Semantic interleavings | Lease and create-key contracts |
| GraphQL wire shape | Codec contract | One complete system flow |
| Hook forwarding and blocking | Installed-hook system test | None |

This table is also a deletion rule. A new process test needs a boundary claim
not already owned below it.

## Required publication evidence

### Pure semantic evidence

The recovery model represents every published revision as:

```text
refs/heads/G                 -> H
refs/heads/gherrit-bases/G   -> first_parent(H)
refs/tags/gherrit/G/vN       -> H
```

The tuple is one indivisible effect. The independently optional
`refs/tags/gherrit/G/pr` marker is a later create-only effect. Pure tests cover
every meaningful durable prefix and exact complete-alias subset before
rebuilding the planner from observation.

Focused planner tables own the complete relation among published history,
marker state, sealed absence or OPEN observation, root status, landing
automation, and desired projection. Correlation tables separately own terminal
history rejection. In particular the tests prove:

- absent plus marker absence is the only create authority;
- absent plus marker presence fails closed;
- OPEN without a marker remains on the owned base and authorizes a marker;
- terminal-only history never reaches the planner;
- an unexplained OPEN head is rejected;
- every create uses `G` and `gherrit-bases/G`; and
- final root and nonroot bases are exact.

### Real Git-boundary evidence

Git fixtures seed real commits and establish published tuples in one ref
transaction. They accept literal base object IDs and independently optional
marker targets. They never infer a base from another change's current head or
a marker from pull request state.

After publication, tests inspect the bare remote and assert all tuple refs and
object IDs. Lost-ack tests perform a real successful atomic push and replace or
corrupt only its acknowledgement. They prove that each batch leaves a complete
tuple prefix and that no GitHub action crosses an unacknowledged Git barrier.

Malformed-state tests construct incomplete tuples with low-level helpers, then
assert failure and exact absence of further Git or GitHub writes.

### GitHub protocol evidence

The scripted transport proves exact local all-state queries, complete
independent pagination, repository/default agreement, fork filtering,
terminal-only rejection, stable owned-base creates, complete alias receipts,
and minimal final updates.

A one-shot visibility expectation hides a known OPEN pull request for exactly
one local-ID observation without removing it from fake durable state or
duplicate-key enforcement. It proves both recovery cases:

- an unmarked omission repeats only the stable owned-base key; and
- a marked omission produces no create and fails closed.

Large pull request populations with other head names must leave the GraphQL
documents, local evidence set, planner result, and mutation trace unchanged.
Cross-repository rows which reuse a requested local head name are followed and
discarded only within the shared raw-row budget.

### Complete-process composition

Retained process scenarios prove:

1. Empty local intent exits before token access or any GitHub request.
2. One-change, two-change, and mixed established/new stacks publish literal
   complete tuples and correct final bases.
3. A lifecycle trace orders tuple publication, create, marker publication, and
   final projection.
4. Incomplete tuples, unsafe bases, and owned-base landing automation reject
   before every write.
5. Lost tuple, create, and marker acknowledgements leave safe durable prefixes;
   fresh invocations converge without confirmation reads or mutation retries.
6. Exact local Git and GitHub reads overlap after stack derivation, and no
   repository-wide or nonlocal observation occurs.
7. Two overlapping publishers prove that identical publication receives an
   acknowledged no-op while a publisher with a conflicting desired tuple loses
   its exact lease and stops before every later GitHub stage.

Complete readable snapshots show results, traces, refs, and pull requests.
Structural assertions remain mandatory for literal object IDs, tuple
atomicity, barrier order, exact request scopes, and absence of writes.
