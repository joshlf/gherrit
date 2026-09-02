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
  before remote work, then derives the stack from that retained evidence;
- admits only exact local Git and GitHub evidence which satisfies the provider
  or fixture boundary in [the pre-push design](../design/pre-push.md);
- represents revisions, public projection, and canonical pull request identity
  with the indivisible tuples, exact leases, and immutable markers defined by
  that design;
- validates every pull request before exposing an effect, creates only safe
  draft contenders, and crosses each acknowledgement barrier in order;
- preserves every immutable version position and projects the exact root,
  nonroot, title, body, and navigation state;
- emits no action when durable state already matches local intent;
- remains safe after every externally committed prefix and under the supported
  publisher interleavings; and
- converges from fresh evidence after intent stabilizes.

The detailed semantic inventory appears under
[Bounded semantic recovery model](#bounded-semantic-recovery-model). Tests do
not claim serializability against writers which bypass the operating
assumptions in the pre-push design, and must not imply that an extra read closes
a time-of-check/time-of-use race.

### Adapter correctness

The suite establishes that:

- local Git history, branch state, remote names, objects, and absence evidence
  survive translation without gaining authority at the adapter boundary;
- Git publication preserves complete atomic units, exact leases, batching, and
  acknowledgement semantics;
- GitHub pagination produces one complete OPEN-only local observation and
  conservatively rejects malformed queries, responses, and receipts;
- unrelated Git advertisement and cross-repository GitHub rows remain bounded
  transport data and never become planner evidence;
- installed hooks are complete, executable, and forward exact arguments; and
- supported platforms agree on process, path, and filesystem behavior.

The exact protocol cases appear under
[Adapter contract tests](#adapter-contract-tests).

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

- **Hermetic:** system fixtures and the product processes they exercise inherit
  no credentials, user configuration, proxy settings, network endpoints, or
  unrelated environment variables.
- **Deterministic:** time, IDs, commit metadata, ordering, visibility, and
  injected failures are controlled explicitly.
- **Bounded:** destination-bound production subprocesses, shared fixture
  commands, servers, network reads and writes, and teardown have finite
  deadlines.
- **Fast:** focused pure logic uses no external resources. The in-memory
  semantic world and typed effects provide recovery feedback without
  subprocesses, sockets, or network access; process recovery fixtures use real
  repository files only when that boundary is the claim.
- **Strict:** unexpected fake operations, malformed requests, and unconsumed
  expectations fail the test.
- **Understandable:** a failure identifies the behavior and evidence layer
  which disagreed.
- **Extensible:** local rules add focused tables; new durable effects or
  visibility schedules extend one bounded semantic model; wire changes add
  adapter contracts.
- **Faithful:** tests use production values and the production executable at
  boundaries where those artifacts are the subject of the claim.

These properties are correctness requirements. A broad test which flakes,
silently accepts an unexpected request, or takes too long to run routinely is
weak evidence even when it covers many lines.

## Product risk model

Coverage is organized around product risks rather than source files.

- **Local intent is misunderstood:** pure stack/policy cases and focused
  real-Git discovery.
- **Unrelated repository state affects publication:** exact-request contracts
  and unrelated-state invariance.
- **The wrong refs are published:** pure tuple decisions and real atomic-push
  and lease contracts.
- **GitHub state is misclassified:** pure marker-join tables and OPEN-only
  GraphQL contracts.
- **A create targets the wrong pull request:** exact receipt tables and one
  scripted mutation contract.
- **A retry repeats or loses work:** the durable-prefix recovery model and
  focused lost-ack contracts.
- **A stale read grants unsafe authority:** visibility schedules and
  marker-aware planner tables.
- **Concurrent publishers overlap:** semantic interleavings and one
  installed-hook overlap.
- **A hook does not enforce the workflow:** installed-hook success and
  blocking system tests.
- **A protocol assumption changes:** a scripted adapter contract or optional
  live smoke test.
- **Platform behavior differs:** cross-platform adapter and process-boundary
  tests.

Line coverage is a backstop, not the organizing goal. High line coverage can
coexist with weak evidence for atomicity, authority transitions, or restart
convergence.

## Architecture

The domain planner and its one-use staged executor are surrounded by narrow
local-Git, remote-Git, GitHub, command-line, and hook adapters. One attempt
supplies one exact local evidence set and invokes the planner once.

```text
sealed branch + management + exact HEAD ---+
                                           +--> local stack from retained HEAD
remote symbolic HEAD + exact public ref ---+

local stack
  +--> empty private/current public --> done
  +--> changed empty public
  |      +--> exact named default --> public effect
  +--> nonempty
         +--> exact named default + local refs/tags/graph
         +--> exact local OPEN pull request connections
                            |
                            v
                     publication plan
                            |
                            v
               required draft conversions
                            |
                            v
     initial Git batches: tuples, then optional public
                            |
                            v
              GraphQL creates --> marker batches
                            |
                            v
                 final projection stage
```

The initial symbolic-`HEAD` and optional-public observation supplies only the
candidate default name and tip needed to derive the stack. The first exact-local
query for a nonempty stack repeats the exact named default. An empty public
stack whose public ref is absent or divergent performs that exact named-default
observation before its one plan and possible write; empty private and
already-current public paths add no read.

"Exact" means that every requested Git name or namespace and every requested
GraphQL connection is covered in the logical evidence. Git may advertise
bounded unrelated tail matches, and an exact-head GitHub connection may return
cross-repository rows; boundary validation discards them before planning. Exact
does not mean that Git and GitHub form a snapshot. Safety comes from validated
immutable history, exact leases, safe draft owned-base creates,
Git-authenticated canonical identity, and one-use acknowledgement gates.

The planner evidence contains no repository-wide pull request rows, nonlocal
histories, or nonlocal graph roots. Complete OPEN-only pagination yields sealed
absence or a validated same-repository row set. CLOSED and MERGED rows exist
only in the independent durable oracle; no terminal row enters an accepted
production observation or planner value. The independently observed marker
either selects one exact canonical number or is absent. Without a marker, the
lowest validated same-repository number is only a deterministic lease
contender. With a marker, every other validated same-repository OPEN row is a
repairable duplicate; if the marker's number is absent, planning fails closed
without distinguishing a terminal row from a temporary omission.

Exact acknowledgement of all required draft conversions releases initial Git
publication. Exact acknowledgement of all required initial Git
batches—including the optional public effect—releases pull request creation.
An initially observed validated contender supplies one marker template; a newly
created pull request supplies its marker identity only through an exact create
receipt. When any create is required, every marker template remains in one
preplanned stage until all create receipts and the receipt-dependent projection
pass preflight. Exact acknowledgement of every marker push releases one final
projection stage. It sends zero or more bounded GraphQL requests which close
noncanonical rows and update canonical rows. If an earlier tuple batch succeeds
but the final public-containing batch fails atomically, the earlier tuples
remain durable, and a fresh attempt plans only the remaining work.

Read-only transport retry or resource backoff may resend an unaccepted page
while assembling the one logical observation. Once planning begins, no write is
followed by a same-attempt query, confirmation read, or planner invocation.
Create receipts consume the preplanned parameterized recipe; they do not
trigger observation or replanning. An indeterminate write ends the attempt.
That attempt does not retry the mutation or roll back. A semantic restart
discards its attempt-local authority and plan, makes a fresh observation, and
plans again. It does not require a new OS process. Tests described as
fresh-process recovery start a new invocation explicitly.

### Domain values

"Typed" means a dedicated validated Rust struct or enum, not a boolean,
formatted command, raw JSON value, or detachable map entry.

The pure core distinguishes:

- default branch name and object ID;
- optional public branch intent, provider-authorized observed state, and
  create/advance transition;
- ordered local changes, each with change ID, head, literal first parent,
  title, and body;
- absent or nonempty local published histories, with zero-version histories
  requiring an absent current head, owned base, and marker;
- exact current heads, owned bases, immutable versions, and optional markers
  carrying canonical pull request numbers for nonempty histories;
- one exact planner input: sealed `Absent` or a validated nonempty
  same-repository OPEN row set joined with that optional marker;
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

- production endpoint/destination compatibility before remote I/O, accepting
  only HTTPS or SSH to GitHub.com, plus explicit custom test destinations whose
  fixtures contain only absent or direct requested heads and tags;
- symbolic remote default discovery which supplies a candidate name and tip;
- exact named-default observation in the first exact-local query for a
  nonempty stack and before an absent or divergent empty-public write, with no
  second read for empty private or already-current public state;
- exact local head/base observation;
- exact optional public-branch presence or authoritative absence under the
  provider or fixture contract in the bounded initial observation;
- exact local version and marker namespace observation;
- bounded acquisition from advertised local version and marker refs;
- atomic publication of complete three-ref tuples with exact leases;
- public-branch creation or advancement with an exact lease, ordered after all
  tuple units in the initial Git stage; and
- separate create-only annotated-marker publication with absence leases.

The GitHub boundary owns:

- repository identity and default-branch observation;
- independently paginated OPEN-only connections filtered by exact local head
  names, with cross-repository rows discarded before planner evidence;
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

Most rules belong here. The pure production logic under test performs no
filesystem, process, or network work. Expected rendered text may reside in
snapshot files.

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

Its independent durable oracle, production-shaped observation, and
process-local intent are separate:

```text
IndependentDurableOracle
  default tip
  public branch projections
  published changes by stable ID
  literal pull request rows in OPEN, CLOSED, or MERGED state

PublishedChange
  ordered published revisions
  zero or more literal pull request rows
  optional change-level marker carrying one pull request number

ProductionOpenObservation
  one exhausted OPEN-only connection per local change ID
  visible same-repository OPEN rows only
  independently observed Git marker

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
contender or exact create receipt supplies that number. CLOSED and MERGED rows
remain only in the independent durable oracle. Before invoking the production
planner, the model projects exactly the visible same-repository OPEN rows. A
marker whose oracle row is CLOSED, MERGED, or hidden therefore produces the
same planner input—marker present with its numbered OPEN row absent—and
fails closed. A visible duplicate cannot replace it. A terminal duplicate
needs no closure, while an unmarked terminal row has no authority and does not
defeat sealed OPEN absence. Production neither downloads terminal history nor
diagnoses CLOSED versus MERGED. Without a marker, the lowest validated
same-repository OPEN number is only a deterministic contender. With a marker,
its exact visible OPEN row is canonical and every other validated
same-repository OPEN row is noncanonical. Closing a row changes the oracle's
lifecycle to CLOSED rather than deleting its identity; a later fresh production
observation omits it.

A completed connection captures its complete returned OPEN row set, including
an empty result. Later lifecycle changes cannot alter a captured row, and
later creation cannot add one. Connections for different change IDs remain
independent rather than pretending to be one backend snapshot. The model can
therefore exercise omissions, cleanup residue, canonical closure or merge, and
later-created duplicates without downloading terminal history in the
production observation.

Every marker's tag object peels to the first published revision and retains its
number through later amendments. Durable Git history supplies current head and
owned-base object IDs. A stale same-repository OPEN row may independently report
any published head slot and any published owned-base first-parent slot. Tests
enumerate their complete Cartesian product, including mismatched revision
slots; a proposal-only or otherwise unpublished object ID is rejected. The
separate historical-safety matrix includes every published revision plus the
proposal and proves for every pair `r, s` that `H(r)` is not reachable from
`P(s)`. Default-base rows require the exact default tip.

A stale query result retains those exact head and base object IDs, draft state,
landing state, and literal projection bytes even when durable writes occur; the
immutable pull request identity remains coupled to the durable row. The model
can therefore prove that writes do not retroactively refresh an observation and
that a complete body patch reaches an exact no-action result on a fresh retry
without deriving desired state inside the fake world.

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
subset, ends the modeled attempt, discards its attempt-local authority and
plan, observes durable state afresh, and requires the next plan to describe
exactly the remaining work.

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
its observation, so the service model cannot hide a mistaken canonical choice.
It stops an attempt whose effect is rejected or indeterminate, then retries the
selected stable intent from a fresh observation. This explores concurrency
without giving either planner access to the other's process-local state or
inventing a second executor for extracted effects.

Required bounded scenarios include:

1. A fresh root: tuple, create, marker, final base/body update, then `Done`.
2. Two missing pull requests: every meaningful subset of complete create
   aliases, then every marker and canonical-update prefix.
3. Amendment and reorder: the marker remains the same annotated tag on `v1`
   while a new version and changed local position converge without recreating
   the pull request or creating another marker, and every published-head ×
   published-first-parent stale OPEN pairing remains safe. For a ready
   marker-bound root becoming a nonroot, either landing flag rejects before any
   effect. Otherwise, exact draft conversion precedes every Git tuple change. A
   stale second conversion is indeterminate rather than an already-draft
   acknowledgement.
4. Visibility: hiding an unmarked provisional OPEN row repeats only the
   owned-base create key; hiding a marked OPEN row rejects without a create;
   hiding the marker-bound canonical row cannot authorize closing or replacing
   it, and a later attempt with a fresh complete observation restores its
   projection regardless of duplicate number ordering.
5. Duplicate cleanup: every meaningful subset of mixed close and canonical
   update aliases, every serialized projection-request prefix, and a retry from
   a fresh observation which emits exactly the remaining work.
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
   public-only empty stacks where absent or divergent state completes exact
   named-default observation before planning or writing while already-current
   state adds no read; tuple batches followed by a final public effect; atomic
   failure of that final batch; and retry after earlier tuple batches have
   already become durable.

Universal invariants include:

- a validation or planning rejection exposes no effect, while a rejected
  external operation contributes no transition of its own but may follow an
  already-acknowledged safe prefix;
- GitHub creates never precede exact acknowledgement of every initial Git
  batch, including a planned public effect;
- final pull request projections never precede exact marker acknowledgement;
- a converged world produces `Done`;
- a successful required effect either makes durable progress or acknowledges
  the exact state established by another publisher;
- retrying every reachable durable prefix converges after intent stabilizes;
- every modeled interleaving of protocol-conforming effects preserves safety;
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

- the endpoint/destination matrix: Production admits supported GitHub HTTPS
  and SSH destinations but rejects filesystem, insecure, and custom-helper
  destinations before remote I/O; Custom admits its explicit local/helper
  fixtures; Disabled admits local plans only within its no-GitHub limit;
- symbolic default discovery which supplies a candidate name and tip, followed
  by exact named-default agreement in the first exact-local query for every
  nonempty stack;
- absent and divergent empty-public paths which complete exact named-default
  observation before planning or writing, plus empty-private and
  already-current empty-public paths which issue no second read;
- exact local head, owned-base, version, and annotated-marker namespace
  parsing, including mandatory peel-to-`v1` framing;
- header-first marker kind and size bounds plus byte-exact canonical marker
  decoding;
- exact public branch presence and authoritative absence in the initial
  observation, with every custom fixture constructed from absent or direct
  requested refs and visible symbolic records rejected defensively;
- authoritative absence for requested owned head and tag names under the same
  provider or fixture contract;
- absence of unrelated logical head or tag requests, together with bounded
  validation and rejection or filtering of unrelated tail-matching records
  which Git may nevertheless advertise;
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
- locale-preserving porcelain acknowledgement parsing which treats translated
  header and footer text as opaque nonempty control-free framing, prevents
  composite-hook prefix output—including non-UTF-8 bytes or forged
  complete-looking blocks—from supplying the receipt, and rejects malformed or
  missing final statuses, extra status-shaped records, or output after the
  footer; and
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

- exact `states: [OPEN]`, `first: 1` documents with one alias for each requested
  local-ID and input-cursor page;
- repository facts on every transmission of the still-unaccepted first page
  batch, including transient retries and successively smaller resource-backoff
  requests until one batch is accepted, and no repository facts on later page
  requests;
- independent pagination where aliases advance at different rates and every
  connection is exhausted through fork-only, same-repository, and mixed pages,
  including pages after the first same-repository row;
- sealed absence or a complete same-repository OPEN set becoming available
  only after cursor exhaustion;
- exact page, alias-grouping, and physical-request counts for each scripted
  cursor schedule, recognizing that one request can alias several pages while
  one connection can span several requests;
- the per-ID first-row allowance plus exactly 99 shared additional raw rows:
  for `N` IDs at most `N + 99` rows and `2N + 99` pages, including one possible
  final empty page per connection, with no donation of an unused allowance;
- cross-repository rows consuming that budget before filtering, validating
  their number, node ID, OPEN state, exact requested head, and selected wire
  shape, but not interpreting their projection fields or object IDs and not
  registering their identity components as local evidence;
- terminal rows neither appearing nor paginating because the connection is
  OPEN-only;
- multiple same-repository OPEN rows retained independently of pagination order
  so the pure join can select a marker-bound number or an unmarked deterministic
  contender;
- repeated same-repository identity components rejecting ambiguity;
- wrong returned head names for every row and wrong returned object IDs for
  same-repository rows;
- missing, null, duplicate, and extra aliases;
- repeated, empty, and missing continuation cursors;
- fatal partial data plus errors;
- resource-limit backoff without consuming a cursor and exact extra request
  counts for prescribed backoff;
- bounded transient query retry while assembling the initial observation, with
  no same-attempt observation after planning;
- every mutation response requiring the exact alias set, a non-null operation
  and pull request, and the expected echoed client mutation ID;
- exact draft-conversion documents whose receipts echo the client mutation ID
  and return the same coupled identity, still OPEN and now draft, with unchanged
  head and observed default-base names and object IDs; landing-automation fields
  are planning preconditions and are not repeated in the receipt;
- stable draft owned-base create documents;
- create receipts with the expected echoed client mutation ID, a non-null pull
  request, a coupled number and node ID new among retained same-repository rows
  and every create receipt in the attempt, exact head and base repository IDs,
  exact `G` and `gherrit-bases/G` names, OPEN and draft state, and head/base
  object IDs matching the acknowledged tuple;
- exact close documents and bounded batches whose receipts echo the client
  mutation ID and return the same coupled identity in CLOSED state;
- minimal update documents whose receipts echo the client mutation ID and
  return the same coupled identity in OPEN state without returning or requiring
  draft state;
- mixed close-before-update projection documents whose alias-count and byte
  limits preserve every operation exactly once across batches; and
- null or incomplete mutation aliases ending the attempt as indeterminate,
  with each mutation request transmitted exactly once and never replayed in
  the same attempt.

Focused production tests snapshot exact generated document text. The shared
process fake validates received documents against the checked-in GitHub schema,
parses that immutable schema once, and reuses it across ordinary application
scenarios.

### System tests

System tests are reserved for complete process claims:

- command-line parsing and user-visible output;
- hook installation, upgrade, permissions, and argument forwarding;
- successful empty-stack push without token lookup or GitHub request, including
  the exact named-default read only when an absent or divergent empty public
  projection may be written;
- representative one-change, two-change, and mixed established/new stacks;
- literal remote head, owned-base, immutable-version, and marker object IDs;
- root/default and nonroot/self-owned final pull request bases;
- a ready default-base canonical which will become nonroot, both with inert
  landing state and with auto-merge or merge-queue state which must reject
  before draft conversion or any other write;
- one installed-hook rejection which blocks the enclosing push before every
  external write;
- representative fresh-process recovery after lost tuple and marker Git
  receipts; create, update, and mixed close/update apply-then-disconnect
  responses; malformed create and update receipts; and a concurrent canonical
  close which a fresh observation rejects;
- one deterministic process case which proves that the exact local Git and
  GitHub observations begin concurrently;
- one converged-process comparison which proves that 512 unrequested OPEN head
  names do not alter GraphQL documents, physical GitHub or Git operations,
  pushes, output, or relevant pull request and ref state;
- one deterministic installed-hook case which composes overlapping
  publication attempts; and
- platform-specific executable discovery and process behavior.

For fixtures deliberately kept below alias and byte limits and run without
injected read retries or resource backoff, assert these exact physical GraphQL
request traces:

- an empty stack sends no request;
- a fresh root sends `Query`, `Create`, then `Update`;
- two visible same-repository rows requiring mixed repair send `Query`,
  `Query`, then one `Close + Update` request;
- a departing ready root plus a new root sends `Query`, `Draft`, `Create`, then
  one `Update` request, with no intervening query;
- unmarked terminal-only history plus no OPEN row has the same `Query`,
  `Create`, `Update` trace as ordinary absence; and
- a converged established stack sends only `Query`.

These named traces complement, rather than replace, adapter assertions over the
exact documents, cursors, aliases, and request counts.

System fixtures create managed commits and IDs directly when hook behavior is
not the claim. Installing hooks as incidental setup hides dependencies and
adds work.

## Test doubles and fault injection

Keep three evidence roles separate:

1. The semantic model compares literal durable state with typed planner
   effects.
2. The strict GitHub process fake validates schema-conforming requests, applies
   effects to its own stored pull request rows, permits duplicate OPEN creates,
   closes any exact OPEN identity, and records ordered requests. Its store may
   retain terminal rows, while its production query surface returns and
   paginates only OPEN rows. This process fixture is separate from the semantic
   model's `DurableWorld` and is deliberately weaker than GitHub's current
   base-sensitive refusal.
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

Comprehensive snapshots are human-reviewed behavior evidence when observable
values and order can be normalized without weakening the claim. It is often
impossible to know in advance which detail is load bearing. A broad stable
snapshot exposes every observable change in one diff that a reviewer can
accept or reject.

Snapshot meaningful behavior rather than incidental fake representation.
Existing system snapshots own stable command results, final pull request
projections, and selected GraphQL operation-label traces. Lower-layer
snapshots own stable complete text, protocol documents, and semantic traces.
When several of these observables form one stable scenario, prefer one broad
reviewable snapshot over disconnected narrow snapshots.

A scenario snapshot may contain:

```text
result
  status
  stdout
  stderr

stable text or trace
  pull request body and navigation
  exact protocol document
  normalized semantic effect sequence
  GraphQL operation-label sequence

final GitHub state
  local PR number, lifecycle, draft state, head, base, title, and complete body
```

Map dynamic values to stable logical names such as `COMMIT_A`, `ID_A`, and
`PR_A`. Omit fabricated user profiles, ports, temporary paths, and values which
exist only because of the fake.

Structural assertions own exact refs, physical request boundaries and counts,
atomicity, leases, and causal barriers. They may also be the primary evidence
for concurrent process composition when backend-assigned values or scheduling
order are intentionally nondeterministic. In those cases they cover every
relevant identity, ref, invariant, and barrier. They also:

- state a universal invariant over enumerated cases;
- prove that a fixture reached the intended precondition; or
- identify the semantic reason for a failure.

They do not replace a complete human-reviewable result with a narrow field
subset merely to reduce snapshot churn.

## Hermeticity and lifecycle rules

Every command created by the shared system-fixture environment starts from an
empty environment, receives an explicit allowlist, and has a deadline.
The fixture resolves the absolute system Git path in-process before isolated
commands run. The raw descendant in `TestCommand`'s timeout test is not a
top-level command; its bounded parent owns its deadline and cleanup. Add a
variable only when a test requires it and document why its value is
deterministic and safe. Commands under that shared fixture environment never
inherit:

- GitHub or other credentials;
- Git, proxy, or credential-helper configuration;
- locale-dependent behavior;
- live network endpoints; or
- repository-redirection and object-database controls.

Use deterministic commit identities, timestamps, and IDs. Sort only where the
external protocol does not define order.

Never sleep to prove that an event happened. Use a typed event, rendezvous,
deadline, or process completion. Servers also have deadlines. Teardown requests
descendant termination and server shutdown under a deadline, waits for
completion evidence, and fails the test when completion cannot be proved. A
pathological server may be detached after that failure so teardown itself
remains bounded. Fixtures also report unconsumed expectations.

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
proves that it rejects the driver protocol. Local and custom Git destinations
used by that driver are trusted test fixtures: every requested head and tag is
constructed absent or direct because Git transport output cannot validate that
precondition for a generic server.

## Performance budget

Measure wall-clock critical path, not only the sum of test durations. Parallel
process tests contend for CPU, filesystems, and server resources; increasing
test threads is not a general optimization.

The current feedback targets are:

- pure planner and recovery feedback well under one second;
- adapter contracts in a few seconds;
- the warm complete required suite under one minute on the reference
  development machine; and
- no individual required test responsible for most of the suite critical
  path.

A warm full run on the reference development machine measured about 53 seconds
on 2026-09-02. Fifteen seconds remains an optimization objective, not a
property the present suite satisfies. Track point-in-time measurements when
changing architecture. A regression against the current baseline or a failure
to approach the objective is an architectural signal. Optimize after measuring
and do not weaken isolation or strictness for speculative savings.

## Adding coverage

When adding behavior:

1. State the product risk and observable claim.
2. Put a semantic rule in the focused pure layer.
3. Update the recovery model only when a durable effect, authority barrier,
   visibility schedule, or restart rule changes.
4. Add an adapter contract only when protocol translation changes.
5. Add a system scenario only when process or hook composition is the claim.
6. Include every new durable operation in the typed trace.
7. Add semantic restart cases for every new committed effect.
8. Review snapshots as behavioral diffs rather than updating them blindly.

For a defect, first add the lowest-layer regression which expresses the
general rule. Retain a higher-level regression only when it protects a distinct
boundary.

## Evidence ownership

Each claim has one primary owner. Higher layers prove composition without
repeating the primary owner's full matrix.

- **Branch-management transitions:** pure transition tables, with focused
  Git-config adapter scenarios for composition.
- **Stack topology and policy:** the pure model, with one real-Git discovery
  contract for composition.
- **GHerrit ID syntax:** a pure function, with one installed commit hook for
  composition.
- **Empty stack avoids GitHub:** an orchestration unit, with one installed-hook
  scenario for composition.
- **Exact local Git names:** the Git adapter, with one complete publication
  trace for composition.
- **Version and marker normalization:** the pure planner, with bare-remote ref
  assertions for composition.
- **Pull request marker join and draft state:** pure tables, with GraphQL
  pagination contracts for composition.
- **Pull request text and navigation:** the pure renderer, with one projection
  snapshot for composition.
- **Minimal update masks:** the pure planner, with a GraphQL encoding contract
  for composition.
- **Tuple, public, and marker decisions:** the pure planner, with real
  atomic-push contracts for composition.
- **Public and mixed-projection restart schedules:** the semantic oracle owns
  exhaustive coverage; the representative process cases are listed under
  [System tests](#system-tests).
- **Draft-conversion interruption:** the semantic world and transport contract;
  no separate composition evidence.
- **Fresh-process recovery:** the process-boundary cases listed under
  [System tests](#system-tests), with exhaustive restart schedules remaining in
  the semantic oracle.
- **Publisher concurrency:** semantic interleavings, with one deterministic
  installed-hook overlap for composition.
- **GraphQL wire shape:** the codec contract, with one complete system flow for
  composition.
- **Hook forwarding and blocking:** installed-hook system tests; no separate
  composition evidence.

This ownership list is also a deletion rule. A new process test needs a
boundary claim not already owned below it.

Branch-management evidence exhaustively classifies the exact legacy-public
configuration tuple and every near miss before deriving an edit. Focused Git
adapter cases then cover malformed or non-unique legacy destinations, forced
drift repair, idempotent migration, refusal to adopt a legacy-shaped tuple in
another ownership state, and transitions from legacy public state to private
or unmanaged state. No migration path may overwrite configuration which the
pure classifier has not proved GHerrit-owned.
