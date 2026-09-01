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

- captures the logical branch, checked management intent, and exact local tip
  before yielding to remote work;
- derives the intended ordered local stack from that retained tip and the exact
  default branch without resolving the live worktree branch again;
- observes an optional public branch together with the default branch;
- publishes the public projection for an empty public stack before returning,
  without GitHub authentication or requests;
- observes and validates only Git and GitHub state belonging to local change
  IDs;
- represents each revision as one inseparable head, literal-owned-base, and
  immutable-version tuple;
- represents a changed public branch as one real initial-ref effect, ordered
  after every tuple and retained in its complete atomic batch;
- records the canonical pull request number in a separate immutable annotated
  marker;
- withholds duplicate cleanup and final pull request projection until every
  required marker is exactly acknowledged;
- creates every pull request as a draft on its permanent owned-base key;
- uses the marker-bound OPEN row as canonical, closes every other visible OPEN
  row, and never promotes a duplicate merely because of number ordering;
- lets an unmarked lowest visible OPEN number contend deterministically for
  the marker lease without treating it as canonical first;
- converts a ready root to draft before any publication which will make it a
  nonroot, and never marks a pull request ready automatically;
- converges roots to the exact default branch and nonroots to their own bases;
- emits no action when durable state already matches local intent;
- remains safe after interruption at every externally committed effect;
- remains safe when protocol-conforming publishers operate on disjoint or
  overlapping change IDs;
- fails closed when a marker-bound pull request is not in the OPEN result; and
- preserves every immutable version position through amendments, rebases,
  reorders, adjacent repeated revisions, and nonconsecutive revision reuse.

Tests cover protocol-conforming concurrent publishers: disjoint publications,
already-desired tuples and markers, identical tuple pushes whose planned
leases became stale, conflicting tuples, delayed creates after a root
retarget, deterministic duplicate cleanup, and different text projections for
one durable revision. They do not claim serializability
against writers which bypass the operating assumptions in [the pre-push
design](../design/pre-push.md). A test must not imply that an extra read closes
a time-of-check/time-of-use race.

### Adapter correctness

The suite establishes that:

- local Git history and branch state are derived accurately;
- remote Git observation requests only the exact default and local change
  namespaces plus the configured public branch when public mode requires it;
- exact absence, immutable history, and annotated marker identity survive byte
  parsing and bounded object decoding;
- Git publication uses complete atomic tuples and exact leases;
- public-ref creation and advancement use exact absence/value leases and the
  same bounded initial-ref batching boundary;
- GitHub exhausts one OPEN-only connection for every exact local ID;
- GraphQL aliases, cursors, errors, nullable fields, and partial mutation
  outcomes are decoded conservatively;
- draft-conversion, create, and mixed final-projection receipts identify the
  exact planned pull requests and draft/lifecycle states;
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
| GitHub state is misclassified | Pure marker-join tables and OPEN-only GraphQL contracts |
| A create targets the wrong PR | Exact receipt tables and one scripted mutation contract |
| A retry repeats or loses work | Durable-prefix recovery model and focused lost-ack contracts |
| A stale read grants unsafe authority | Visibility schedules and marker-aware planner tables |
| Concurrent publishers overlap | Exact-lease, delayed-create, canonical-selection, and recovery cases |
| A hook does not enforce the workflow | Installed-hook success and blocking system tests |
| A protocol assumption changes | Scripted adapter contract or optional live smoke test |
| Platform behavior differs | Cross-platform adapter and process-boundary tests |

Line coverage is a backstop, not the organizing goal. High line coverage can
coexist with weak evidence for atomicity, authority transitions, or restart
convergence.

## Architecture

The domain planner and its one-use staged executor are surrounded by narrow
local-Git, remote-Git, GitHub, command-line, and hook adapters. One attempt
supplies one exact local evidence set and invokes the planner once.

```text
sealed branch + management + exact HEAD --\
                                          +-> local stack from retained HEAD
remote symbolic HEAD + exact public ref --/                 |
                                                            +-> empty public stack
                                                            |            |
                                                            +-> exact local refs, tags, graph --+
                                                            +-> exact local OPEN PRs ----------+
                                                                                               |
                                                                                     publication plan
                                                                                               |
                                      required draft conversions
                                                            |
                                      initial Git batches: tuples, then optional public effect
                                                            |
                                                            +-> GraphQL creates -> marker batches
                                                                                   -> final GraphQL projection
```

"Exact" means that every requested Git name or namespace and every requested
GraphQL connection is covered. It does not mean that Git and GitHub form a
snapshot. Safety comes from validated immutable history, exact leases, safe
draft owned-base creates, Git-authenticated canonical identity, and one-use
acknowledgement gates.

The evidence set contains no repository-wide pull request rows, nonlocal
histories, or nonlocal graph roots. Complete OPEN-only pagination yields sealed
absence or a validated same-repository row set. The independently observed
marker either selects one exact canonical number or is absent. Without a
marker, the lowest number is only a deterministic lease contender. With a
marker, every visible row except its exact number is a repairable duplicate;
if its number is absent, planning fails closed.

Exact acknowledgement of all required draft conversions releases initial Git
publication. Exact acknowledgement of all required initial Git batches—including the
optional public effect—releases pull request creation. An initially observed
validated contender supplies one marker template; a newly created pull request
supplies its marker identity only through an exact create receipt. Exact
acknowledgement of every marker push releases one final GraphQL projection
which closes noncanonical rows and updates canonical rows. If an earlier tuple batch
succeeds but the final public-containing batch fails atomically, the earlier
tuples remain durable and a fresh attempt plans only the remaining work.

An indeterminate write ends the attempt. The same attempt does not retry the
mutation, roll back, or reobserve. A fresh process reconstructs all authority
from durable Git and GitHub evidence.

### Domain values

"Typed" means a dedicated validated Rust struct or enum, not a boolean,
formatted command, raw JSON value, or detachable map entry.

The pure core distinguishes:

- default branch name and object ID;
- optional public branch intent, exact observed state, and create/advance
  transition;
- ordered local changes, each with change ID, head, literal first parent,
  title, and body;
- absent or nonempty local published histories;
- exact current heads, owned bases, immutable versions, and an optional marker
  carrying the canonical pull request number;
- one exact planner input: sealed `Absent` or a validated nonempty OPEN row set
  joined with that optional marker;
- pull request identities as coupled number and GraphQL node ID values;
- desired complete projections and minimal update masks;
- whole draft-conversion, tuple, public-branch, create, and marker effects with
  stable identities;
- minimal update effects addressed by sealed pull request identities; and
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
- exact optional public-branch presence or authoritative absence in the
  bounded initial observation;
- exact local version and marker namespace observation;
- bounded acquisition from advertised local version and marker refs;
- atomic publication of complete three-ref tuples with exact leases; and
- public-branch creation or advancement with an exact lease, ordered after all
  tuple units in the initial Git stage; and
- separate create-only annotated-marker publication with absence leases.

The GitHub boundary owns:

- repository identity and default-branch observation;
- independently paginated OPEN-only connections filtered by exact local head
  names;
- conversion of each exhausted connection into sealed `Absent` or a validated
  nonempty row set, followed by the pure marker-number join;
- exact draft-conversion mutations and receipts before initial Git publication;
- stable draft owned-base create mutations and exact receipts;
- exact duplicate-close mutations and CLOSED receipts; and
- minimal canonical update mutations and exact OPEN receipts.

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
- tuple, public-branch, and marker decisions;
- batching boundaries over typed effects; and
- bounded error formatting.

Use tables or direct bounded enumeration when they make the valid product
clear. A table row should describe a real semantic combination, not a freely
recombinable set of booleans which admits impossible states.

### Bounded semantic recovery model

The recovery model compares the production planner with an independent literal
world. It deliberately knows nothing about refspecs, `ls-remote`, GraphQL
documents or alias names, JSON, URLs, or HTTP. It preserves only semantic
operations and the request and alias boundaries which constrain durable
subsets.

Its durable world and process-local intent are separate:

```text
DurableWorld
  default tip
  public branch projections
  published changes by stable ID
  literal pull request rows

PublishedChange
  ordered published revisions
  zero or more pull request rows with OPEN, CLOSED, or MERGED lifecycle
  optional change-level marker carrying one pull request number

LocalIntent
  optional public branch and desired tip
  ordered local changes

LocalChange
  stable ID
  desired revision and literal first parent
  title and body
```

The marker belongs to the change and authenticates one exact pull request
number. The production stage machine cannot publish it until an observed OPEN
contender or exact create receipt supplies that number. The independent world
still permits a marker whose numbered row is CLOSED, MERGED, or omitted from
one observation; the planner must fail closed instead of allowing the model to
hide a mistaken identity rule. Without a marker, the lowest visible OPEN
number is only a deterministic contender. With a marker, its exact visible
OPEN row is canonical and every other visible OPEN row is noncanonical.
Closing a row changes its lifecycle to CLOSED rather than deleting its
identity.

A completed connection captures its complete returned OPEN row set, including
an empty result. Later lifecycle changes cannot alter a captured row, and
later creation cannot add one. Connections for different change IDs remain
independent rather than pretending to be one backend snapshot. The model can
therefore exercise omissions, cleanup residue, canonical closure or merge, and
later-created duplicates without downloading terminal history in the
production observation.

Every marker's tag object peels to the first published revision and retains its
number through later amendments. Durable Git history supplies current head and
owned-base object IDs. A stale query result separately retains the exact head
and base object IDs, draft state, landing state, and literal projection bytes
which that query returned; the immutable pull request identity remains coupled
to the durable row. The model can therefore prove that writes do not
retroactively refresh an observation and that a complete body patch reaches an
exact no-action retry without deriving desired state inside the fake world.

The production planner exposes typed test effects. Initial Git effects retain
their complete atomic grouping; a public effect is never removed from a trace
or inferred from branch mode:

```text
Draft conversions | Initial Git batches (Tuples + optional PublicBranch) |
Creates | Markers | Final pull request projections | Done | Rejected
```

Draft-conversion, tuple, create, and marker effects carry the stable change ID
and their complete semantic payload. Creates always add draft owned-base rows.
Conversions, closures, and updates are addressed by sealed pull request
identities; the model resolves each requested node ID to its durable pull
request row, then requires that row to remain OPEN before applying the
mutation. It records the resolved change ID only in its local trace, compares
exact effect order and content, applies an allowed durable prefix or alias
subset, discards all process-local authority, rebuilds a fresh observation,
and requires the next plan to describe exactly the remaining work.

Concurrency cases derive two plans from independently chosen observations and
run one complete competing plan while the primary plan is suspended at each
cross-stage authority barrier and between acknowledged serialized batches or
requests. Complete-alias subset enumeration separately covers indeterminate
execution within one GraphQL request. The durable model applies Git effects
only when every exact lease still matches and deliberately permits multiple
OPEN creates even for one base-sensitive request key. This proves safety
without relying on GitHub's current duplicate refusal and includes the weaker
case in which a later owned-base create lands after a root retarget. The model
also accepts a close for any exactly addressed OPEN row. A separate oracle
proves that the planner emits closes for exactly the noncanonical identities in
its observation, so the service model cannot hide a mistaken canonical choice. It
stops an attempt whose effect is rejected or indeterminate, then retries the
selected stable intent from a fresh observation. This explores concurrency
without giving either planner access to the other's process-local state or
inventing a second executor for extracted effects.

Required bounded scenarios include:

1. A fresh root: tuple, create, marker, final base/body update, then `Done`.
2. Two missing pull requests: every meaningful subset of complete create
   aliases, then every marker and canonical-update prefix.
3. Amendment and reorder: the marker remains the same annotated tag on `v1`
   while a new version and changed local position converge without recreating
   or remarking an established pull request; a ready root becoming nonroot
   crosses the draft-conversion barrier before any Git tuple changes.
4. Visibility: hiding an unmarked provisional OPEN row repeats only the
   owned-base create key; hiding a marked OPEN row rejects without a create;
   hiding the marker-bound canonical row cannot authorize closing or replacing
   it, and later complete visibility restores its projection regardless of
   duplicate number ordering.
5. Duplicate cleanup: every meaningful subset of mixed close and canonical
   update aliases, every serialized projection-request prefix, and a fresh
   retry which emits exactly the remaining work.
6. Concurrent publishers: disjoint changes independently converge regardless
   of execution order, while allocation order may change literal pull request
   identities and the bodies which embed them; a tuple or marker which another
   publisher applied is already complete on retry; identical tuple pushes from
   the same observation both receive usable acknowledgements even though only
   one changes the refs; pushes with different desired tuples from the same
   observation admit one exact-lease winner and stop the loser; a delayed
   same-key create and a delayed owned-base create after a root retarget can
   each become a noncanonical duplicate; a fresh attempt closes either without
   touching the marker-bound canonical identity; and
   different navigation projections for the same tuple are safe
   last-writer-wins updates which converge after one local intent stabilizes.
7. Public projection: absent, already-desired, and divergent public refs;
   public-only empty stacks; tuple batches followed by a final public effect;
   atomic failure of that final batch; and retry after earlier tuple batches
   have already become durable.

Universal invariants include:

- rejection exposes no mutation;
- GitHub creates never precede exact acknowledgement of every initial Git
  batch, including a planned public effect;
- final pull request projections never precede exact marker acknowledgement;
- a converged world produces `Done`;
- a successful required effect either makes durable progress or acknowledges
  the exact state established by another publisher;
- retrying every reachable durable prefix converges after intent stabilizes;
- every interleaving of protocol-conforming effects preserves safety;
- immutable tags never move;
- one tuple never splits its head, owned base, and version;
- a public effect remains visible in the complete atomic batch trace and is
  ordered after every tuple effect;
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
- exact local head, owned-base, version, and annotated-marker namespace
  parsing, including mandatory peel-to-`v1` framing;
- header-first marker kind and size bounds plus byte-exact canonical marker
  decoding;
- exact public branch presence and authoritative absence in the initial
  observation;
- authoritative absence for requested names;
- absence of unrelated head or tag requests;
- source-ref-only object acquisition for exact local version and marker refs
  without local ref effects;
- complete atomic tuple pushes and exact head/base leases;
- immutable version and marker absence leases, including same-identity no-op
  and different-identity marker races;
- identical competing tuple pushes where one changes the refs and both receive
  exact usable acknowledgements despite the second push's stale leases;
- conflicting competing tuple pushes where one wins and the other receives an
  exact-lease rejection;
- byte-bounded batches which never split a tuple;
- initial batches which place the optional public effect last without dropping
  it from an atomic batch;
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

- one OPEN-only alias per exact local head name;
- repository facts on the first request only;
- independent pagination where aliases advance at different rates;
- fork-only pages followed to exhaustion;
- OPEN pages followed independently to exhaustion;
- fork-only pages followed until sealed absence or same-repository evidence;
- multiple OPEN rows retained independently of pagination order so the pure
  join can select a marker-bound number or an unmarked deterministic
  contender;
- repeated local identity components rejecting ambiguity;
- wrong returned head names and object IDs;
- missing, null, duplicate, and extra aliases;
- repeated, empty, and missing continuation cursors;
- fatal partial data plus errors;
- resource-limit backoff without consuming a cursor;
- bounded transient query retry;
- exact draft-conversion documents and OPEN/draft identity receipts;
- stable draft owned-base create documents;
- exact create-key, object-ID, and draft-state receipts;
- receipt identity uniqueness scoped to the exact local rows and create
  aliases retained by one attempt;
- exact close documents, bounded batches, and CLOSED identity receipts;
- minimal update documents and exact OPEN identity/draft-state receipts;
- mixed close-before-update projection documents whose alias-count and byte
  limits preserve every operation exactly once across batches; and
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
- focused recovery after lost draft-conversion, tuple, create, marker, and
  final-projection acknowledgements;
- one delayed-create race where a root has already left its owned base and the
  next attempt closes only the noncanonical duplicate;
- one overlapping two-publisher race where an exact-lease loser performs no
  later GitHub write; and
- platform-specific executable discovery and process behavior.

System fixtures create managed commits and IDs directly when hook behavior is
not the claim. Installing hooks as incidental setup hides dependencies and
adds work.

## Test doubles and fault injection

Keep three evidence roles separate:

1. The semantic model compares literal durable state with typed planner
   effects.
2. The strict GitHub fake validates schema-conforming requests, applies
   independent stored pull request effects, permits duplicate OPEN creates,
   closes any exact OPEN identity, and records ordered requests. This service
   model is deliberately weaker than GitHub's current base-sensitive refusal.
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
- apply a chosen subset of complete create or final-projection aliases;
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
| Branch-management transitions | Pure transition tables | Focused Git-config adapter scenarios |
| Stack topology and policy | Pure model | One real-Git discovery contract |
| GHerrit ID syntax | Pure function | One installed commit hook |
| Empty stack avoids GitHub | Orchestration unit | One installed-hook scenario |
| Exact local Git names | Git adapter | One complete publication trace |
| Version and marker normalization | Pure planner | Bare-remote ref assertions |
| Pull request marker join and draft state | Pure table | GraphQL pagination contracts |
| Pull request text and navigation | Pure renderer | One projection snapshot |
| Minimal update masks | Pure planner | GraphQL encoding contract |
| Tuple, public, and marker decisions | Pure planner | Real atomic-push contracts |
| Restart convergence | Semantic world | Focused lost-ack composition |
| Publisher concurrency | Semantic interleavings | Lease and duplicate-cleanup contracts |
| GraphQL wire shape | Codec contract | One complete system flow |
| Hook forwarding and blocking | Installed-hook system test | None |

This table is also a deletion rule. A new process test needs a boundary claim
not already owned below it.

Branch-management evidence exhaustively classifies the exact legacy-public
configuration tuple and every near miss before deriving an edit. Focused Git
adapter cases then cover malformed or non-unique legacy destinations, forced
drift repair, idempotent migration, refusal to adopt a legacy-shaped tuple in
another ownership state, and transitions from legacy public state to private
or unmanaged state. No migration path may overwrite configuration which the
pure classifier has not proved GHerrit-owned.

## Required publication evidence

### Pure semantic evidence

The recovery model represents every published revision as:

```text
refs/heads/G                 -> H
refs/heads/gherrit-bases/G   -> first_parent(H)
refs/tags/gherrit/G/vN       -> H
```

The tuple is one indivisible effect. An optional public-branch transition is a
separate, visible effect at the end of the initial Git stage. The independently
optional `refs/tags/gherrit/G/pr` marker is a later create-only annotated tag
which peels to `v1` and records the canonical pull request number. Required
draft conversions form an earlier GraphQL barrier. Pure tests cover every
meaningful durable prefix and exact complete-alias subset before rebuilding the
planner from observation.

Focused planner tables own the complete relation among published history,
marker identity, sealed absence or a nonempty OPEN set, canonical identity,
duplicate identities, root status, draft state, landing automation, and
desired projection. In particular the tests prove:

- absent plus marker absence is the only create authority;
- absent plus marker presence fails closed;
- every OPEN row passes history, base, and landing-state validation before any
  duplicate can be closed;
- every unmarked contender and noncanonical duplicate must be draft on its
  owned base and reject landing automation;
- a marker selects its exact OPEN number regardless of response order or
  duplicate number ordering;
- without a marker, the lowest OPEN number is only a deterministic contender;
- a marker whose exact number is absent from the OPEN set fails closed;
- canonical draft-owned, draft-default, and ready-default states are accepted,
  while ready-owned is unrepresentable;
- only a ready-default canonical row whose desired base is owned requires the
  draft-conversion barrier;
- an unexplained OPEN head is rejected;
- every create uses `G` and `gherrit-bases/G` with `draft: true`;
- every noncanonical OPEN identity becomes an exact closure after the marker
  barrier;
  and
- final root and nonroot bases apply only to the canonical identity.

### Real Git-boundary evidence

Git fixtures seed real commits and establish published tuples in one ref
transaction. They accept literal base object IDs and independently optional
marker identities. They never infer a base from another change's current head
or a marker number from pull request state.

After publication, tests inspect the bare remote and assert all tuple, public,
and marker refs, peeled targets, strict tag bytes, and object IDs. Lost-ack tests perform a real successful atomic
push and replace or corrupt only its acknowledgement. They prove that each
batch leaves a complete atomic-unit prefix, that earlier tuple batches may
remain after a later public-containing batch fails, and that no GitHub action
crosses an unacknowledged Git barrier.

Malformed-state tests construct incomplete tuples with low-level helpers, then
assert failure and exact absence of further Git or GitHub writes.

### GitHub protocol evidence

The scripted transport proves exact local OPEN-only queries, complete
independent pagination, repository/default agreement, fork filtering, exact
draft conversion, stable draft owned-base creates, complete alias receipts,
and mixed exact duplicate-closure and minimal canonical-update projections.

A one-shot visibility expectation hides a known OPEN pull request for exactly
one local-ID observation without removing it from fake durable state. It proves
these recovery cases:

- an unmarked omission repeats only the stable owned-base key;
- a marked omission produces no create and fails closed; and
- omission of the marker-bound canonical row never authorizes its closure or
  replacement, while later complete visibility restores it and closes every
  visible noncanonical row.

Large pull request populations with other head names must leave the GraphQL
documents, local evidence set, planner result, and mutation trace unchanged.
Cross-repository rows which reuse a requested local head name are followed and
discarded only within the shared raw-row budget.

### Complete-process composition

Retained process scenarios prove:

1. Empty private local intent exits before token access or any GitHub request;
   empty public local intent projects only its public branch.
2. One-change, two-change, and mixed established/new stacks publish literal
   complete tuples and correct final bases.
3. A lifecycle trace orders required draft conversions before complete initial
   batches, retains the optional public effect after all tuples, then orders
   create, marker publication, and one final projection stage whose close
   aliases precede canonical-update aliases in each request.
4. Incomplete tuples, unsafe bases, and owned-base landing automation reject
   before every write.
5. Lost draft-conversion, tuple, public, create, marker, and final-projection acknowledgements
   leave safe durable prefixes; fresh invocations converge without
   confirmation reads or mutation retries.
6. Exact local Git and GitHub reads overlap after stack derivation, and no
   repository-wide or nonlocal observation occurs.
7. Two overlapping publishers prove that identical publication receives an
   acknowledged no-op while a publisher with a conflicting desired tuple loses
   its exact lease and stops before every later GitHub stage.
8. A delayed stale create after a root retarget produces a noncanonical OPEN
   row; the next fresh invocation closes only that duplicate and retains the
   marker-bound canonical identity.

Complete readable snapshots show results, traces, refs, and pull requests.
Structural assertions remain mandatory for literal object IDs, tuple
atomicity, barrier order, exact request scopes, and absence of writes.
