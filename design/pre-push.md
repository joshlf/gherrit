# Pre-push publication

GHerrit publishes a local commit stack as Git refs and GitHub pull requests.
This document defines the representation, the evidence required before a
write, the order of durable effects, and the guarantees of the pre-push hook.

Every change owns its mutable head branch and its mutable pull request base
branch. Git records immutable patch history and durable pull request existence.
GitHub pull requests are projections of that Git state and the current local
stack.

This file is the canonical specification for pre-push publication. It is
written for both human readers and agents maintaining the implementation.

## Scope and assumptions

This design applies when GHerrit's pre-push hook handles a managed branch. It
covers:

- finding the local stack;
- observing the remote Git and GitHub state relevant to that stack;
- validating historical reachability and current repository state;
- publishing change-owned Git refs;
- creating and updating GitHub pull requests; and
- retrying after crashes, rejected writes, and lost acknowledgements.

One attempt publishes only changes in the local stack. It neither observes nor
validates the histories or pull requests of other GHerrit changes. This is safe
because a valid pull request uses only its own head and owned base, or the
stable default branch. Publishing one change therefore cannot move either ref
used by another valid change.

Multiple GHerrit publishers may run concurrently. The protocol assumes:

- every writer of managed heads, owned bases, version tags, pull request
  markers, and managed pull request projection follows this protocol;
- no independent change to a managed pull request's head repository, head
  name, or lifecycle while a publication attempt runs;
- no other pull request uses a change-owned head or base as its base;
- no concurrent movement of the repository default branch;
- the repository default branch name does not change while managed pull
  requests exist;
- complete local commit ancestry, subject to the explicit partial-clone
  acquisition path;
- exactly one configured push destination; and
- durable GitHub effects eventually becoming stably visible to later
  observations when convergence depends on them.

These assumptions are part of the guarantee. No sequence of client-side reads
can provide serializability against an actor which bypasses the protocol and
changes pull request lifecycle or topology between observation and mutation.

Protocol-conforming concurrent publishers remain safe. Publishers operating
on disjoint change IDs own disjoint mutable refs. When publishers overlap, a
requested ref is acceptable if it still has the exact observed value or
already has the exact desired value. Git acknowledges the latter as an
up-to-date no-op without requiring the now-stale old-value or absence lease.
Identical publishers can therefore both cross the tuple barrier after only one
changes the refs. A ref at any other value rejects, so exact leases serialize
conflicting tuple publication. A publisher which observes the desired tuple
or marker also treats it as already complete. Concurrent creates use the same
stable creation key. Even for the same durable revision, final projections may
differ because navigation reflects each publisher's complete local stack.
Those differences affect text only; root status and every allowed base follow
immutable ancestry. Competing projections are therefore safe
last-writer-wins updates. Once one complete local intent is stable, a fresh
attempt repairs any stale projection. If different revisions race, a
protecting Git lease may instead reject conflicting work before later stages.

Each top-level GraphQL mutation alias is also assumed to be one indivisible
pull request operation. A transmitted mutation may execute any subset of its
complete aliases, but an individual alias does not partially apply its title,
body, or base fields.

Retrying a create additionally relies on GitHub atomically refusing a second
same-repository OPEN pull request with the same creation key:

```text
(base repository, head repository, head ref, base ref)
```

The competing create must fail without creating another pull request.
`clientMutationId` correlates a response but does not supply idempotence. The
[public create endpoint][create-pr-api] documents a generic `422` response
rather than the required uniqueness and atomicity contract. This behavior is
therefore an explicit operating assumption, not a documented API guarantee.

Git and GitHub reads are not a cross-system snapshot. Safety derives from the
immutable history, stable create key, exact leases, acknowledgement barriers,
and fail-closed treatment of ambiguity. Freshness is a liveness concern unless
this document says otherwise.

## Change representation

### Change identity and local stack

Each managed commit has exactly one `gherrit-pr-id` trailer. Its value is a
nonempty ASCII alphanumeric string and is the change ID. The same grammar is
used in managed ref names.

The local stack is the ordered first-parent path after the remote default
branch and through `HEAD`. It is valid only when:

- no commit subject begins with Git's pending-autosquash prefixes `fixup!`,
  `squash!`, or `amend!`; this check runs before trailer validation;
- every stack commit has exactly one valid change ID;
- change IDs are unique in the stack and in the reachable ancestry relevant to
  validation;
- no change ID equals the default branch name;
- the default branch is neither `gherrit-bases` nor below
  `gherrit-bases/`;
- the default branch is an ancestor of `HEAD`;
- every required first parent is available; and
- no shallow, graft, replacement, or implicit-fetch behavior can change the
  graph being validated.

Stack order supplies parent and child relationships. A merge commit may occur
in the stack. Stack order follows first parents, while identity and
reachability validation inspect all parents reachable from the required roots.

### Refs owned by one change

For change ID `G`, let `H_G` be its desired commit and `P_G` be
`first_parent(H_G)`. The change owns:

- `refs/heads/G`, its mutable pull request head;
- `refs/heads/gherrit-bases/G`, its mutable owned base;
- `refs/tags/gherrit/G/vN`, its immutable version records; and
- `refs/tags/gherrit/G/pr`, its optional immutable pull request marker.

Publishing version `N` establishes one complete tuple:

```text
refs/heads/G                    -> H_G
refs/heads/gherrit-bases/G      -> P_G
refs/tags/gherrit/G/vN          -> H_G
```

The head and owned base move together when a later version is published. A
version tag never moves. The remote version sequence is the authoritative
patch history; local version tags are neither consulted nor created.

Versions are contiguous and one-based. A new version is created only when the
desired revision differs from the last published revision. Observation
nevertheless preserves every immutable tag position, including adjacent tags
which name the same commit. A later revision may also return to an older
literal commit. Histories such as `A, A` and `A, B, A` retain every position.

The current head and owned base must agree with the final version: the head is
the final version's commit and the base is that commit's literal first parent.
A disagreement is invalid state, not an independently repairable ref.

### Pull request marker

`refs/tags/gherrit/G/pr` is a lightweight immutable tag. It points to any head
in `G`'s validated published history and records only that GHerrit has either
observed or exactly acknowledged a valid same-repository OPEN pull request for
`G`.

The marker does not store the pull request number, GraphQL node ID, or current
version. It never moves when a new version is published. Marker absence is
authoritative only after a successful exact Git namespace observation.

The marker is deliberately separate from a version tuple. A version can be
published before GitHub creates the pull request. A pull request can be
created before its existence marker is acknowledged. Final GitHub projection
cannot occur until the marker acknowledgement barrier is crossed.

### Pull request head and base

Every create for `G`, including a desired root, uses the permanent key:

```text
head repository = the destination repository
headRefName      = G
base repository = the destination repository
baseRefName      = gherrit-bases/G
```

This key does not change across amendments, rebases, reorders, moves between
stacks, or changes in root status.

For example:

```text
main --- A --- B

refs/heads/A                   -> A
refs/heads/gherrit-bases/A     -> main

refs/heads/B                   -> B
refs/heads/gherrit-bases/B     -> A
```

The pull request for `B` compares `B` with `gherrit-bases/B`. Publishing a new
version of `A` does not move either ref in `B`'s comparison. Publishing a new
version of `B` moves both of `B`'s refs as one tuple.

A converged root pull request targets the exact repository default branch. A
new root is nevertheless created on its owned base. It stays on that safe base
until the marker is acknowledged, then the final update moves it to the
default branch.

A converged nonroot stays on its own owned base. Root-status changes alter only
the final pull request base:

- root to nonroot moves the base from the default branch to
  `gherrit-bases/G`; and
- nonroot to root moves the base from `gherrit-bases/G` to the default branch.

The stable creation key does not change.

### Pull request projection

The desired pull request title comes from the local commit subject. The body
is rendered from the local commit body, stack navigation, and immutable patch
history. The desired base is the default branch for the first stack change and
the change's own base for every later change.

Observed title and body text are projection values, not identity authority.
GHerrit identifies a pull request by the exact local change ID requested in the
GraphQL connection and by the returned same-repository head name. A stale or
manually edited body is repaired by the final projection rather than parsed to
discover ownership.

GHerrit does not append a hidden `gherrit-meta` identity or topology record.
Commit bodies do not reserve that prefix. The auto-cascade GitHub Action is
disabled and is not part of this publication protocol.

## One publication attempt

One attempt observes, validates, plans, and then moves through a fixed sequence
of durable effects. It performs no rollback and no same-attempt confirmation
read after a write.

### Empty-stack boundary

GHerrit first resolves the configured Git push destination and observes the
remote symbolic `HEAD`. That establishes the candidate default branch name
and tip needed to derive the local stack.

If the stack after that default branch is empty, GHerrit returns successfully
at once. It does not read a GitHub token, construct an authenticated client, or
send a GitHub request. An empty managed branch has no local publication intent,
so unrelated GitHub availability and credentials are irrelevant.

GitHub authentication begins only after a nonempty stack supplies the exact
local change IDs to observe.

### Logical observation

For a nonempty stack, the complete logical observation contains:

- one resolved push destination and its GitHub repository coordinates;
- one default branch name and object ID agreed by remote Git, the corresponding
  local branch, and GitHub;
- the ordered local stack;
- the exact remote head, owned base, version history, and optional marker for
  every local change ID;
- one fully paginated all-state pull request connection for every local change
  ID;
- the commit graph rooted at the default tip, local proposals, and local
  published versions; and
- every exact external object needed to validate that graph.

No repository-wide head or pull request collection is part of this
observation. No nonlocal change ID, tag namespace, history, graph root, or pull
request enters the planner.

### Exact local Git evidence

The first remote Git read asks for symbolic `HEAD` and its object ID. Once the
local stack is known, later byte-bounded reads request only:

- the exact named default branch;
- `refs/heads/G` for each local ID;
- `refs/heads/gherrit-bases/G` for each local ID;
- `refs/tags/gherrit/G`; and
- `refs/tags/gherrit/G/*`.

The namespace root and descendant pattern together prove complete coverage of
each requested tag namespace. A successful response which omits
`refs/tags/gherrit/G/pr` proves marker absence for that observation. The parser
rejects a result outside the exact requested name or namespace even when Git's
pattern matching could return a name with a coincidental suffix.

Each logical request retains its requested IDs, destination capability, and
exact ref names. Raw maps cannot be relabelled or sliced into a new claim of
complete coverage.

The local graph contains the default tip, every local proposal, every local
published version, and their required ancestry. It contains no graph roots
discovered from unrelated pull requests.

If an advertised version object is missing, GHerrit may fetch only the exact
advertised version-tag refs for the affected local observation. The fetch:

- uses source-only refspecs;
- writes no local ref or `FETCH_HEAD`;
- disables tags, submodules, and automatic maintenance;
- does not fetch a raw object ID; and
- reloads the graph after the bounded acquisition attempt.

An existing promisor repository may receive one additional `--refetch`
attempt. There is no unbounded acquisition loop or incidental lazy fetch.

### Exact local GitHub evidence

For each local ID `G`, GHerrit queries one repository connection filtered by
the exact head name and all lifecycle states:

```graphql
pullRequests(
  headRefName: "G"
  states: [OPEN, CLOSED, MERGED]
  first: PAGE_SIZE
  after: CURSOR
) {
  nodes {
    number
    id
    state
    headRefName
    headRefOid
    baseRefName
    baseRefOid
    isCrossRepository
    title
    body
    autoMergeRequest { enabledAt }
    isInMergeQueue
  }
  pageInfo { hasNextPage endCursor }
}
```

Connections for several IDs are aliased in one document when request limits
permit. The request token retains each alias's ID and input cursor. A response
must contain exactly the requested aliases. Each alias advances only its own
cursor, a repeated cursor is invalid, and no observation is exposed until
every requested connection is exhausted.

The first request also obtains the repository node ID and GitHub default
branch. Later pagination requests contain only the repository connection
fields required for their exact pages.

Cross-repository rows do not describe the local branch and are ignored after
their response shape and requested head name are validated. Pagination still
continues past them. Same-repository rows are classified only after the exact
connection is exhausted:

- more than one OPEN row is ambiguous and rejects the observation;
- exactly one OPEN row yields `Open`, even when older terminal rows also
  exist;
- no OPEN row and one or more CLOSED or MERGED rows rejects before planning;
  and
- no same-repository row yields an opaque sealed `Absent` value.

All terminal identities are validated and counted. The rejection names a
bounded representative set and reports how many additional identities were
omitted, so an arbitrarily long history cannot produce unbounded output. They
do not enter the planner and GHerrit does not choose one historical pull
request. `Absent` means only that the exact all-state connection was exhausted
without a same-repository row; it does not assert snapshot isolation or
timeless absence.

Pull request numbers and node IDs are validated and must be unique across the
relevant local observation. They are retained together as one identity.
Unrelated repository identities are neither downloaded nor placed in a global
collision registry.

Any GraphQL response with substantive errors is unusable, even when it also
contains partial data. A response which contains only a recognized resource
limit error and no data may cause the same logical page to be rebuilt with a
smaller alias batch or page size. Missing or extra aliases, malformed rows,
invalid object IDs, null required fields, and pagination contradictions fail
the attempt before a write.

Read-only transport failures may be retried with bounded delays. Mutation
requests are never retried in the same attempt.

### Parallel read work

After local derivation and GitHub authentication, exact local Git namespace
observation, graph loading, and exact local GitHub observation may proceed
concurrently. Their results meet once in the planner.

This concurrency shortens the read critical path but grants no snapshot
semantics. Every result remains bound to its own request and destination, and
all results must agree before an action becomes available.

### Derived local state

For each local change, validation derives:

- absent or nonempty published history;
- a coherent current head, owned base, and latest immutable version;
- an absent or validated immutable pull request marker;
- the local proposed head and literal first parent;
- whether a new version tuple is required;
- one sealed `Absent` or validated `Open` pull request observation, after
  terminal-only history has been rejected; and
- a bounded final-projection recipe. Existing pull requests can supply their
  numbers immediately; missing pull request numbers remain parameters until
  exact create receipts supply them.

The planner consumes the local stack, local Git histories, and local pull
request observations in the same exact order. A request-derived ID remains
attached to its observation, so a valid page cannot be reassigned to another
change.

## Validation and planning

No remote Git or GitHub write is exposed until the complete observation is
validated and every later action which can be preflighted is known to fit its
supported limits.

### Published history

For each local change, validation requires:

1. Version tags are lightweight, canonical, contiguous, and immutable.
2. Every version points to a commit carrying the expected change ID.
3. The current head equals the latest version head.
4. The current owned base equals that head's literal first parent.
5. The optional marker is lightweight and points to a published version head.
6. Every required commit and first parent is present in the validated graph.
7. Every published and proposed head is unreachable from every published and
   proposed first parent for the same change.

Every contiguous version position is preserved even when two adjacent tags
name the same literal revision. Publication does not create a new version when
the proposal already equals the current version, but validation does not infer
how an existing immutable tag was created or discard it.

The last rule covers old-head/new-base and new-head/old-base combinations that
GitHub might observe while mutable refs propagate. Checking only the current
pair is insufficient.

For a change which is or will become root, every published and proposed head
must also be unreachable from the exact agreed default tip.

The direct reachability checks remain explicit even though unique identity
ancestry normally implies them. They express the operational safety property
and defend against mistakes in history decoding.

### Pull request state

The supported local realities are:

| Published history | Marker | Pull request | Result |
| --- | --- | --- | --- |
| absent or present | absent | `Absent` | Stable-key create after tuple publication |
| present | absent | valid OPEN on owned base | Publish marker, then final projection |
| present | present | valid OPEN | Apply remaining final projection |
| any | present | `Absent` | Fail closed |
| any | any | terminal-only history | Reject before planning |
| absent | any | OPEN | Reject unexplained pull request state |

An OPEN pull request must:

- have the exact same-repository head name requested for the local ID;
- have a head object ID found in validated published history;
- use either its exact owned base or the exact agreed default branch;
- have an owned-base object ID found among validated published first parents;
- have the exact default object ID when based on the default branch; and
- remain on its owned base while it lacks a marker.

Native auto-merge and merge-queue state are invalid when the pull request is or
will become based on an owned base. GitHub could otherwise land a pull request
whose base is not the repository default branch.

An `Absent` observation plus exact marker absence creates a one-use
authorization for the stable create key. The authorization does not claim that
GitHub is current. If an unmarked provisional pull request was omitted, the
same-key uniqueness assumption makes the repeated create safe. An `Absent`
observation plus a marker never authorizes creation.

### Immutable plan

The final plan owns all decisions which can be made before writing. It contains
typed tuple operations, provisional create specifications, marker preflight,
and a bounded recipe for the final projection. Raw Git records, GraphQL JSON,
aliases, cursors, and freely recombinable booleans do not enter it.

For a stack with missing pull requests, final bodies and minimal updates are
not yet values in the plan. The recipe is parameterized by the complete set of
pending pull request numbers. Exact create receipts consume that recipe once,
after validating every assigned number and identity, and produce only the
marker operations authorized by those creates plus the exact final updates.
Marker operations already authorized by observed OPEN pull requests remain a
separate preplanned set. Execution may combine the two authorized sets into one
atomic push; their authority does not become interchangeable. This is one
staged plan, not a second observation or a replan.

One-use values represent authority transitions. A caller cannot construct a
create without consuming exact absence and marker evidence, cannot construct a
new marker for a created pull request without consuming an exact create
receipt, and cannot access final updates before consuming exact marker
acknowledgement.

## Durable publication sequence

The state machine is:

```text
resolve destination
    -> observe remote HEAD and derive local stack
    -> if empty, return before GitHub authentication
    -> observe and validate exact local Git and GitHub state
    -> publish required Git tuples
    -> create missing pull requests
    -> publish required pull request markers
    -> apply final pull request updates
```

Each arrow after planning is an acknowledgement barrier. Work after a barrier
is unavailable until every required effect before it has an exact usable
acknowledgement. A stage with no work crosses its barrier immediately.

### Publish Git tuples

Each changed revision produces one tuple operation:

```text
head:       absent or expected old head -> desired head
owned base: absent or expected old base -> desired first parent
version:    absent                      -> desired head
```

The three refs are never split. Several tuples may share one bounded
`git push --atomic` batch. Command-size batching may create several atomic
batches; an earlier acknowledged batch and an indeterminate later batch form a
safe durable prefix.

Every mutable ref uses an exact force-with-lease expectation. Every new tag
uses an exact absence lease. A malformed, incomplete, or ambiguous porcelain
acknowledgement ends the attempt. Creation remains inaccessible unless every
required tuple batch is exactly acknowledged.

Git reports a requested ref which already has its desired object as an
acknowledged up-to-date no-op. This applies even when another identical
publisher made the planned old-value or absence lease stale. Such an
acknowledgement proves the same postcondition as performing the write. A ref at
a different object still rejects under the exact lease.

### Create missing pull requests

Creates run only after the tuple barrier, so every requested head and owned
base already exists. Each create includes the exact destination repository,
head repository, `G` head, `gherrit-bases/G` base, title, provisional body, and
client mutation ID.

The provisional body does not depend on pull request numbers which GitHub has
not assigned. The permanent owned base is safe for roots and nonroots.

Create aliases may be batched. GitHub may apply a subset before an error or
lost response makes the acknowledgement indeterminate. The same attempt stops
without publishing markers or final updates for that batch. A later invocation
reconstructs durable state through the all-state observation.

An exact create receipt requires:

- the expected echoed client mutation ID;
- a non-null pull request;
- a valid coupled number and node ID pair, unique among exact local identities
  and create receipts retained by this attempt;
- the exact same-repository head and owned-base names;
- OPEN lifecycle state; and
- head and base object IDs matching the acknowledged tuple.

The receipt proves the exact created object rather than relying on a
repository-wide registry of unrelated identities.

### Publish pull request markers

Every local OPEN pull request without a marker requires one marker operation.
An initially observed valid OPEN pull request authorizes its marker during
planning. A newly created pull request authorizes its preflighted marker only
after its exact receipt is consumed.

Marker publication is a separate bounded atomic Git push. Each operation
creates only `refs/tags/gherrit/G/pr`, uses an absence lease, and targets a
validated published head. It never moves a head, base, or version tag.

Final updates remain inaccessible until every required marker batch is exactly
acknowledged. An indeterminate marker result ends the attempt with each marker
either absent or durably present and every affected pull request still on a
validated safe base.

### Apply final pull request state

After the marker barrier, GHerrit sends only fields which differ from the
desired projection:

- title when needed;
- complete body when needed; and
- base name when root status or provisional creation requires it.

A root targets the exact default branch. A nonroot targets its own owned base.
Every update names the exact preplanned GraphQL node identity, and its receipt
must return that same number and node ID.

Update aliases may be applied in any subset. Each alias is assumed indivisible,
and every possible old or desired base has already passed reachability
validation. An indeterminate update ends the attempt; a later observation
derives only fields which still differ.

## Safety and recovery

### Historical reachability

For change `G`, let `R_G` contain every published revision and the local
proposal. For revision `r`, let `H(r)` be its head and `P(r)` its literal first
parent. Let `X <= Y` mean that `X` is reachable from `Y`, including equality.

Validation requires:

```text
for every r and s in R_G: not (H(r) <= P(s))
```

An owned-base pull request can therefore observe any historical or proposed
head/base combination without its head becoming reachable from its base.

For a root, validation separately requires:

```text
for every r in R_G: not (H(r) <= default_tip)
```

The default branch is stable by assumption during the attempt.

### Mutation footprint

Only local changes produce writes. A valid nonlocal pull request for `X` uses
head `X` and either `gherrit-bases/X` or the stable default branch. A local
publication for `G != X` moves neither of those refs and cannot alter `X`'s
projection.

A legacy, manually retargeted, or otherwise corrupt pull request could use a
local change's head or owned base as its own base. This protocol does not scan
the repository to discover that unsupported state. The assumption that
change-owned branches are reserved and not used by other pull requests is
therefore necessary. Supporting migration or arbitrary manual cross-base use
requires a separate exact base-consumer guard or migration protocol.

### Safe visible prefixes

Every externally visible prefix is safe:

1. Read-only observation changes no managed remote state.
2. An atomic tuple batch leaves every included change wholly old or wholly
   new.
3. Earlier acknowledged tuple batches contain only validated combinations.
4. A created pull request starts on its permanent safe owned base.
5. A marker push changes no pull request comparison ref.
6. Final updates become available only after durable existence markers.
7. A complete update alias leaves the pull request on an old validated base,
   its owned base, or its final validated base.

Title and body fields do not affect reachability.

### Failure prefixes

| Failure point | Durable result | Retry behavior |
| --- | --- | --- |
| Before tuple publication | No external write | Reobserve and replan |
| Tuple acknowledgement lost | Batch is wholly old or wholly new | Exact refs reveal the result |
| Some tuple batches acknowledged | Safe complete tuple prefix | Publish remaining tuples |
| Create acknowledgement lost | A provisional PR may exist | Repeat stable key or observe PR |
| Some create aliases applied | Some provisional PRs exist | Observe exact local IDs |
| Marker acknowledgement lost | Marker is absent or immutable | Reobserve exact tag namespace |
| Some marker batches acknowledged | Safe marker subset | Publish remaining markers |
| Some update aliases applied | Some projections are final | Compare and update remaining fields |

Nothing is rolled back. Rollback would add writes and could restore an older
combination which no longer belongs to the validated plan.

### Stale GitHub visibility

The all-state query is exhaustive over every cursor returned for each local ID,
but it is not a snapshot and GitHub may temporarily omit a durable row or
return older field values.

If an unmarked provisional OPEN pull request is omitted, marker absence permits
the same stable-key create. The duplicate-key assumption prevents a second
OPEN pull request. The failed or indeterminate create supplies no identity and
therefore cannot release a marker or update.

If a marked pull request is omitted, creation is forbidden. Terminal-only
history rejects before planning; otherwise the attempt fails closed until the
OPEN row becomes stably visible.

Older head or base object IDs are accepted only when validated published
history explains them. A resulting redundant update is safe. A value outside
validated history rejects the attempt.

### Convergence

Convergence is a liveness property after intent stabilizes, not while
publishers continually express incompatible stacks. Fix one complete local
intent and the default branch, and assume that after some point no publisher
writes a conflicting tuple or projection. Other publishers may still perform
effects required by the fixed intent. Measure its remaining durable work
lexicographically by:

1. missing Git tuples;
2. missing pull requests;
3. missing pull request markers; and
4. stale final projection fields.

After stabilization, an acknowledged required effect reduces the earliest
affected component. Another publisher may perform the same Git effect first,
in which case the pending push is acknowledged as an up-to-date no-op or a
fresh attempt observes the desired state. Either path makes the same
reduction. An indeterminate effect either reduces the measure or leaves it
unchanged. Exact Git advertisements expose durable Git progress, and eventual
stable GitHub visibility removes already-applied GitHub work from later plans.
If still-required operations eventually receive usable acknowledgements,
fresh attempts reach a plan with no action.

Before stabilization, a tuple push whose desired object conflicts with the
current object must satisfy exact leases, and a rejected push stops before its
later stages. An attempt which had already observed its tuple as desired may
emit a later stale projection after another publisher changes the tuple, but
every allowed projection is safe. Conflicting effects may increase the
remaining work for either intent, so the protocol promises safety rather than
progress until one complete intent stabilizes. It never rolls back to
manufacture progress.

### Concurrent mutation limit

No observation design can close a time-of-check/time-of-use race against a
writer which bypasses this protocol. For example, another actor can close an
unmarked provisional pull request after observation but before a retry create.
Once it is closed, OPEN-key uniqueness need not prevent a new pull request.

Repository-wide observation, an extra re-observation, or combining OPEN and
terminal queries differently merely moves this race. Supporting such writers
requires a backend idempotency or locking primitive, or a different durable
protocol. This design supports concurrent protocol publishers but states the
boundary against independent lifecycle and topology writers plainly.

## Adapter contracts

### Push destination

All Git reads, acquisitions, and writes use one destination resolved from the
configured remote according to Git's ordinary URL and push-URL rules. GitHub
repository coordinates and body links derive from that same resolved
destination.

The implementation uses a command-scoped internal remote rather than exposing
the destination literal throughout child argument lists or writing it to
repository configuration. It sanitizes inherited Git configuration,
repository-redirection, credential, tracing, object-database, replacement,
graft, shallow, proxy, and prompt-control environment variables before every
child command. HTTP redirects are disabled.

Every network command is bounded by output, execution, and cleanup deadlines.
Failures include bounded terminal-safe diagnostics without printing the private
destination or credentials.

### Git observation and acquisition

Remote observation is byte-oriented and validates exact ref names. It rejects:

- malformed object IDs or ref records;
- symbolic `HEAD` without a usable target;
- disagreement between `HEAD` and its exact target ref;
- an unexpected namespace root;
- noncanonical or noncontiguous versions;
- annotated managed tags; and
- any response which cannot prove complete coverage of the requested local
  names.

Exact local queries, source-ref acquisitions, tuple pushes, and marker pushes
use a conservative variable-argument budget. Batching never splits a tuple or
marker operation.

Observation and acquisition do not update local branches, tags,
remote-tracking refs, `FETCH_HEAD`, or Git configuration.

### Git publication acknowledgements

Git publication uses `--atomic`, porcelain output, and exact leases. The
adapter compares the acknowledgement with the complete requested ref set. A
missing, duplicate, extra, rejected, or malformed status makes the result
indeterminate or failed and withholds the next stage.

An exact up-to-date status is a usable acknowledgement: it proves that the ref
already has the requested object even if its planned lease is stale. The
adapter does not confuse that successful no-op with a lease rejection caused
by a different current object.

Each internal publication command sets an internal environment marker to its
generated remote name. The GHerrit hook returns immediately only when Git
supplies that same remote name and a nonempty remote location. A composite
pre-push hook therefore continues to run its other checks for internal tuple
and marker pushes. A wrapper which invokes GHerrit must preserve the marker.
The marker prevents cooperative recursion; it is not an authentication or
security boundary.

### GraphQL queries

Read-only GraphQL requests have bounded serialized documents, response bodies,
connection and total-attempt timeouts, and a finite transient retry schedule.
Recognized resource-limit failures may reduce query dimensions without
advancing an input cursor.

The all-state local-ID accumulator owns completeness. It accepts a page only
when the response alias, requested ID, input cursor, returned head name, and
next cursor agree. It exposes one exact ordered local observation only after
all connections are exhausted.

### GraphQL mutations

Create and update documents use JSON string escaping for every external value.
Requests are limited by both alias count and serialized byte size. A single
operation which exceeds the byte limit rejects before transmission.

A mutation is sent exactly once. Transport failure, timeout, non-success HTTP
status, GraphQL errors, missing or extra aliases, a null operation, malformed
payload, or an invalid receipt is indeterminate. The current attempt performs
no mutation retry and crosses no later barrier.

## Code model

The implementation uses dedicated values for evidence and authority. The
essential shape is:

```text
PushDestination
    -> RemoteDefault
    -> LocalStack

LocalStack + PushDestination
    -> ExactLocalGitObservation
    -> ExactLocalPullRequestObservation
    -> CommitGraphEvidence

ExactLocalPullRequestObservation[G]
    = SealedAbsent | Open(ValidatedIdentityAndProjection)

terminal-only exact history
    -> aggregated pre-planning rejection

validated local evidence
    -> PublicationPlan
    -> TupleStage
    -> CreateStage
    -> MarkerStage
    -> FinalProjection
```

Transport types retain aliases, cursors, raw strings, and response JSON only
at the adapter boundary. Domain types retain validated IDs, object IDs,
histories, identities, base kinds, desired projections, and one-use stage
authority.

The planner cannot represent:

- a nonlocal change;
- a pull request observation detached from its requested local ID;
- a marker target outside validated history;
- a split head/base/version publication action;
- final updates without marker acknowledgement; or
- a create receipt for an unplanned change.

Invalid external combinations become errors before a plan exists rather than
additional planner states.

## Performance

Network latency and backend query execution dominate ordinary publication.
Work therefore scales with the local stack rather than repository size.

An empty stack performs only the Git work needed to identify its boundary. It
does not authenticate to GitHub or send a GraphQL request.

A normal nonempty attempt performs:

- one small symbolic remote `HEAD` observation;
- byte-bounded exact Git reads for the default and local change namespaces;
- one logical GraphQL observation containing aliased all-state connections for
  the local IDs; and
- only the mutation and push stages which have actual work.

There is no repository-wide OPEN scan, terminal second wave, nonlocal tag
observation, nonlocal object acquisition, marker confirmation query, rollback,
or same-attempt re-observation.

Independent local connections and effects are batched to reduce round trips.
Batch and page sizes back off only when request, response, or backend resource
limits require it. Expensive graph work is shared across local histories, and
duplicate object IDs are loaded once without collapsing distinct version
positions.

## Testing obligations

[The testing strategy](../agent_docs/testing.md) assigns each claim to its
lowest faithful layer. At minimum, coverage proves:

- exact local Git ref and tag observation, including authoritative absence;
- empty-stack completion before GitHub token access or requests;
- pending-autosquash rejection before trailer validation or remote
  observation;
- complete independently paginated all-state queries for exact local IDs;
- sealed absence, one OPEN, historical terminal plus OPEN, terminal-only,
  duplicate OPEN, fork, and malformed response outcomes;
- no repository-wide or nonlocal observation;
- historical reachability across all published and proposed pairings;
- complete tuple atomicity and leases at a real Git remote;
- exact create and update receipt validation;
- internal publication recursion suppression without bypassing other
  pre-push checks;
- tuple, create, marker, and update interruption prefixes;
- protocol-conforming publisher interleavings and exact-lease conflicts;
- stale visibility before and after marker publication; and
- deterministic convergence to no action once intent stabilizes.

Semantic recovery tests apply typed effects to an independent literal durable
world, discard all attempt-local authority, and plan again. They do not parse
Git commands, GraphQL documents, JSON, or HTTP. Adapter tests own those
encodings, and a small system suite proves complete hook and process
composition.

## Guarantee boundary

Under the assumptions in this document, GHerrit guarantees:

- every local version is one exactly leased head/base/version tuple;
- every created pull request starts on its permanent safe owned base;
- no final pull request update precedes durable marker acknowledgement;
- every acknowledged and indeterminate prefix remains safe;
- protocol-conforming publishers remain safe when their local stacks are
  disjoint, overlap, or temporarily express different stacks or revisions;
- a marked identity is never recreated because GitHub omitted it;
- crashes and lost acknowledgements require no rollback or transaction log;
- fresh attempts converge after one complete local intent stabilizes, durable
  effects become stably visible, and required operations eventually receive
  usable acknowledgements; and
- observation and validation cost depends on local stack size, not all open
  pull requests or managed histories in the repository.

The protocol does not guarantee:

- a deterministic winner or freedom from starvation while publishers
  continually race with conflicting local intents;
- serializability against ref or pull request writers which bypass the
  protocol;
- safety against concurrent default-branch movement;
- safety against manual mutation of managed refs, tags, markers, lifecycle, or
  topology;
- discovery of legacy or corrupt pull requests based on another change's
  owned branches;
- preservation of concurrent manual title or body edits;
- use of native auto-merge or merge queues for owned-base pull requests; or
- automatic rebasing after a root pull request merges.

[create-pr-api]: https://docs.github.com/en/rest/pulls/pulls#create-a-pull-request
