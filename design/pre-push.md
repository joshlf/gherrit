# Pre-push publication

GHerrit publishes a local commit stack as Git refs and GitHub pull requests.
This document defines the representation, the evidence required before a
write, the order of durable effects, and the guarantees of the pre-push hook.

Every change owns its mutable head branch and its mutable pull request base
branch. A public managed branch additionally owns one force-updated projection
of the local stack tip. Git records immutable patch history and durable pull
request existence. GitHub pull requests are projections of that Git state and
the current local stack.

This file is the canonical specification for pre-push publication. It is
written for both human readers and agents maintaining the implementation.

## Scope and assumptions

This design applies when GHerrit's pre-push hook handles a managed branch. It
covers:

- finding the local stack;
- observing the remote Git and GitHub state relevant to that stack;
- validating historical reachability and current repository state;
- publishing change-owned Git refs and an optional public branch projection;
- creating and updating GitHub pull requests; and
- retrying after crashes, rejected writes, and lost acknowledgements.

One attempt publishes only changes in the local stack and, in public mode, the
single public branch named by the checked-out local branch. It neither observes
nor validates the histories or pull requests of other GHerrit changes. This is
safe because a valid pull request uses only its own head and owned base, or the
stable default branch. Publishing one change therefore cannot move either ref
used by another valid change.

Multiple GHerrit publishers may run concurrently. The protocol assumes:

- every writer of managed heads, owned bases, version tags, pull request
  markers, public branch projections, and managed pull request projection
  follows this protocol;
- no independent change to a managed pull request's head repository, head
  name, or lifecycle while a publication attempt runs;
- no other pull request uses a change-owned head or base as its base;
- no concurrent movement of the repository default branch;
- the repository default branch name does not change while any GHerrit-managed
  OPEN pull request exists; GHerrit does not infer a renamed default or
  retarget a pull request from the former name;
- complete local commit ancestry, subject to the explicit partial-clone
  acquisition path;
- exactly one configured push destination;
- pull request numbers are immutable, repository-unique, and increase with
  creation, so a later create cannot take a lower number than an existing row;
  and
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
or marker also treats it as already complete. Concurrent or retried creates
may leave several valid same-repository OPEN pull requests for one head after
a root moves away from its creation base. GHerrit treats that as a repairable
durable prefix: the lowest-numbered visible OPEN row is canonical, every
higher visible row is closed after the marker barrier, and only the canonical
row receives the final projection. Even for the same durable revision, final
projections may differ because navigation reflects each publisher's complete
local stack. Those differences affect text only; root status and every allowed
base follow immutable ancestry. Competing projections are therefore safe
last-writer-wins updates. Once one complete local intent is stable, a fresh
attempt repairs any stale projection and closes remaining duplicates. If
different revisions race, a protecting Git lease may instead reject
conflicting work before later stages.
The same exact-lease rule applies when a public branch needs a transition. If
it was observed already at the desired stack tip, the plan emits no redundant
operation or lease. A later competing move can therefore make a public link
temporarily stale, just as a later tuple move can make an already-planned pull
request projection stale. This remains safe and a fresh stable attempt repairs
it.

Each top-level GraphQL mutation alias is also assumed to be one indivisible
pull request operation. A transmitted mutation may execute any subset of its
complete aliases, but an individual alias does not partially apply its title,
body, or base fields.

Every create still uses one permanent request key:

```text
(base repository, head repository, head ref, base ref)
```

GitHub currently refuses a second OPEN pull request with that exact key, but
correctness does not depend on the refusal. A root's canonical pull request is
eventually retargeted to the default branch, which frees the owned-base key for
a delayed stale create. GitHub also permits one head to have OPEN pull requests
against different bases. Any accepted duplicate has a later pull request
number than the row which already existed, so minimum-number selection never
authorizes a stale observation to close an unseen older row. A later complete
observation either preserves the same canonical identity or discovers a lower
one and closes the previously selected higher row. `clientMutationId`
correlates a response but does not supply idempotence.

Git and GitHub reads are not a cross-system snapshot. Safety derives from the
immutable history, safe creation base, deterministic canonical selection,
exact leases, acknowledgement barriers, and fail-closed treatment of
ambiguity. Freshness is a liveness concern unless this document says
otherwise.

## Change representation

### Change identity and local stack

Each managed commit has exactly one `gherrit-pr-id` trailer in the final
commit-message paragraph. GHerrit reads that paragraph from the immutable raw
commit-message bytes. The key is case-insensitive, but its separator is exactly
a colon followed by one ASCII space; Git's normalized trailer output is not
identity evidence. Another occurrence of the same key with a different
separator is malformed rather than silently ignored. The value is a nonempty
ASCII alphanumeric string of at most 128 bytes and is the change ID. A
continuation is part of the value and therefore makes a change ID invalid.

The final paragraph is a trailer block only when each non-continuation line
has a nonempty key without whitespace or control bytes, followed by `:` or
`=`, and each line beginning with an ASCII space or tab continues a preceding
entry. Other keys use this broad shape only to delimit the block; they carry no
GHerrit authority. The same change-ID value grammar is used in managed ref
names. The length bound leaves ample room for GHerrit's 33-byte generated IDs
while keeping every owned ref component and its lock-file name within the
limits of the supported destination.

The checked-out branch name must be valid UTF-8 before GHerrit uses it to look
up management state. The local stack is the ordered first-parent path after
the remote default branch and through `HEAD`. It is valid only when:

- no commit subject begins with Git's pending-autosquash prefixes `fixup!`,
  `squash!`, or `amend!`; this check runs before trailer validation;
- every commit subject is nonempty and contains at most 256 Unicode scalar
  values, so it is a valid GitHub pull request title;
- every local stack commit's message body, including its trailer paragraph,
  is valid UTF-8; GHerrit neither decodes it lossily nor applies Unicode
  normalization;
- every stack commit has exactly one valid change ID;
- change IDs are unique in the stack and in the reachable ancestry relevant to
  validation;
- no change ID equals or is a ref-path ancestor of the default branch name;
- the default branch is neither `gherrit-bases` nor below
  `gherrit-bases/`;
- the default branch is an ancestor of `HEAD`;
- every required first parent is available; and
- no shallow, graft, replacement, or implicit-fetch behavior can change the
  graph being validated.

The UTF-8 requirement applies to local messages which GHerrit projects into
pull requests. Commit messages encountered only while validating external
published history remain opaque bytes and are never rendered.

This list defines the local graph and text which may enter planning. Before
any remote write, planning also renders the widest pull request body each
change can require. It rejects the attempt if any body exceeds 131,072 UTF-8
bytes or if any single serialized mutation cannot fit its one-MiB request
limit. An arbitrarily large UTF-8 commit message is therefore not publishable.

Stack order supplies parent and child relationships. A merge commit may occur
in the stack. Stack order follows first parents, while identity and
reachability validation inspect all parents reachable from the required roots.

### Optional public branch projection

Every managed branch uses a loopback enclosing push. In public mode GHerrit
itself projects the local stack tip to `refs/heads/B` at the configured push
destination, where `B` is the exact UTF-8 checked-out local branch name. This
ref is not part of change identity or patch history. Its only pull request
effect is the public branch link rendered in each body.

The first path component of `B` must contain a byte other than an ASCII letter
or digit. `B` must be neither `gherrit-bases` nor below `gherrit-bases/`. It
conflicts with the observed default branch `D` exactly when `B == D`, `B`
starts with `D + "/"`, or `D` starts with `B + "/"`. This grammar makes `B`
disjoint from every possible change-owned head, the complete owned-base
namespace, and the default branch. For example, `feature-/work` and
`release-candidate` are valid, while `feature`, `feature/work`, and
`Gchange/backup` are not.

The public branch link has two independent renderings of `B`. The Markdown
label backslash-escapes every ASCII punctuation character, which CommonMark
treats as a valid backslash escape. The GitHub tree URL percent-encodes every
UTF-8 byte except RFC 3986 unreserved bytes and `/`; slash remains a path
separator so branch hierarchy is preserved. A byte which is meaningful to
Markdown or a URL therefore remains literal branch data rather than changing
the generated link's structure.

The public ref is a GHerrit-owned, force-updated projection, not a second
bidirectional source of truth. An exact lease protects a value which changes
after observation, but an already-observed value different from the local tip
is deliberately replaced. Other actors may read the branch for backup or
sharing but must not update it independently. Changing the local branch to
private mode or renaming it does not delete an earlier public ref because
GHerrit cannot prove that deletion is still authorized.

If the initial observation already names the desired tip, GHerrit has no public
transition to lease. Even when a transition occurs, its acknowledgement proves
the public target only at that barrier. A later writer can therefore make the
target stale before pull request projection. This does not alter any managed
pull request comparison ref: the public branch contributes only a link. A
fresh attempt repairs the target after concurrent intent stabilizes.

GHerrit does not scan unrelated ordinary branches for directory/file
conflicts. If an ordinary remote branch is a ref-path ancestor or descendant
of `B`, the remote rejects creation of `B`. Because the public operation is
last, its containing atomic batch rolls back, any earlier complete tuple
batches remain a safe prefix, and no GitHub mutation occurs. Retry cannot
succeed until the conflicting ordinary ref is removed or the branch is made
private or renamed.

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

Every create request continues to use the owned-base key after the canonical
pull request moves. The request key is stable even though a root's current
GitHub base is not.

### Pull request projection

The desired pull request title comes from the local commit subject. The body
is rendered from the local commit body, stack navigation, and immutable patch
history. The desired base is the default branch for the first stack change and
the change's own base for every later change.

Every patch-history `Base` link compares that version tag against the
version's literal first-parent object ID. The link never interpolates a
default, public, or owned branch name. The lowercase hexadecimal object ID is
already URL-safe and preserves the historical comparison when branch names or
refs later move.

Observed title and body text are projection values, not identity authority.
GHerrit identifies a pull request by the exact local change ID requested in the
GraphQL connection and by the returned same-repository head name. A stale or
manually edited body is repaired by the final projection when it differs under
the following exact comparison rather than parsed to discover ownership.
GHerrit replaces every CRLF pair with LF in both the observed and generated
body and then compares the remaining text exactly. It does not trim leading,
trailing, or line-ending whitespace, normalize a lone carriage return, or
apply Unicode normalization. Bodies which differ only by CRLF versus LF are
therefore already converged and are not rewritten. Titles are compared
exactly. Observation accepts a base name only when it is the exact default
branch or `gherrit-bases/G` for that pull request's change ID; final projection
then compares which of those two validated base kinds is desired.

GHerrit does not append a hidden `gherrit-meta` identity or topology record.
Commit bodies do not reserve that prefix. The auto-cascade GitHub Action is
disabled and is not part of this publication protocol.

## One publication attempt

One attempt observes, validates, plans, and then moves through a fixed sequence
of durable effects. It performs no rollback and no same-attempt confirmation
read after a write.

### Empty-stack boundary

Before remote work, GHerrit captures the logical local branch, its checked
management intent, and its exact `HEAD` object. An unmanaged branch returns
without consuming hook input or resolving a destination. For a managed
branch, direct invocation has no enclosing Git effect; an invocation from Git
must be the empty local loopback installed by `gherrit manage`, so no enclosing
ref update can occur after publication finishes.

GHerrit then resolves the configured Git push destination and observes the
remote symbolic `HEAD`. In public mode the same request also observes the exact
public branch or proves its absence. The default branch establishes the name
and tip needed to derive the local stack from the captured `HEAD`.

If the stack after that default branch is empty, private mode returns
successfully at once. Public mode first creates or advances its public branch
to the stack tip, which in this case is the default tip, using the same exact
lease and acknowledgement rules as the initial Git stage. Neither mode reads a
GitHub token, constructs an authenticated client, or sends a GitHub request.
Unrelated GitHub availability and credentials are therefore irrelevant.

GitHub authentication begins only after a nonempty stack supplies the exact
local change IDs to observe.

### Logical observation

For a nonempty stack, the complete logical observation contains:

- one resolved push destination and its GitHub repository coordinates;
- the exact initial value or authoritative absence of the optional public
  branch;
- one default branch name and object ID agreed by remote Git, the corresponding
  local branch, and GitHub;
- the ordered local stack;
- the exact remote head, owned base, version history, and optional marker for
  every local change ID;
- one fully paginated OPEN pull request connection for every local change ID,
  followed by a terminal-state absence probe only for IDs with no visible
  same-repository OPEN row;
- the commit graph rooted only at distinct local published-version heads which
  differ from that change's sealed local proposal, plus their ancestry; and
- every exact external object needed to validate that graph.

No repository-wide head or pull request collection enters this logical
observation. No nonlocal change ID, tag namespace, history, graph root, or pull
request enters the planner. The Git wire may advertise broader ref categories
before the client constructs this exact evidence, as described below.

### Exact local Git evidence

The first remote Git read asks for symbolic `HEAD`, its object ID, and, in
public mode, the exact public branch. This adds a ref pattern but no network
round trip. Once the local stack is known, later byte-bounded reads request
only:

- the exact named default branch;
- `refs/heads/G` for each local ID;
- `refs/heads/gherrit-bases/G` for each local ID;
- `refs/tags/gherrit/G`; and
- `refs/tags/gherrit/G/*`.

Git's positional `ls-remote` patterns are slash-delimited tail filters, not
server-side exact prefixes. The later reads also pass `--heads --tags`, which
lets a protocol-v2 server restrict its advertisement to those two broad
categories, but Git can still receive every head and tag before filtering.
The initial symbolic-`HEAD` read and older protocol versions may receive a
broader advertisement. This transport boundary is deliberately delegated to
Git so credentials, proxies, helpers, and every supported transport keep
Git's behavior instead of a second protocol implementation.

Every remote Git command has a 120-second execution deadline and a 64-MiB
stdout limit. Validated unrelated refs which tail-match a requested pattern
consume that stdout budget even though the parser later ignores them. A
timeout or the first byte beyond the limit stops and cleans up the command
before parsing, planning, or writing. These bounds provide resource safety,
not progress: stable unrelated refs can prevent publication if they keep a
required advertisement outside either bound.

The namespace root and descendant pattern together prove complete coverage of
each requested tag namespace. A successful response which omits
`refs/tags/gherrit/G/pr` proves marker absence for that observation. The parser
validates every returned record before classifying its complete ref name.
Records in an owned requested namespace populate evidence. A validated ref
whose complete name is outside every owned namespace is ignored even when
Git's pattern matching returned it because of a coincidental suffix; it cannot
prove presence or defeat authoritative absence. This includes a peeled record
for an unrelated annotated tag. Peeled records in an owned namespace,
malformed records, and records in an owned but unrequested namespace reject.

Each logical request retains the exact sealed local stack which authorized it,
the repository and destination capabilities used to execute it, and its exact
ref names. Raw maps cannot be relabelled, sliced into a new claim of complete
coverage, or validated against a different stack with coincidentally equal IDs.

Before loading an object, GHerrit structurally preflights every requested local
history as one set and requires its exact default branch name and tip to equal
the path origin retained by the sealed local stack. Graph roots then preserve
semantic first-occurrence order: change order from the local stack, then
version order within each change. A published slot equal to its own sealed
proposal uses the local revision directly and is not a graph root. The same OID
equal to another change's proposal remains external to the first change. Thus
the local graph is rooted only at distinct external published heads and
contains all of their declared-parent ancestry. It contains no default-tip,
local-proposal, or unrelated pull-request root.

Every distinct external graph root retains the nonempty list of exact version
tag refs which advertised it, in local-change and version order. Graph loading
checks roots in that same order and breadth-first ancestry preserves the first
root which caused each missing object to be required. Only that deterministic
causal root can authorize acquisition.

A missing advertised root permits one ordinary negotiated fetch. A missing
ancestor permits no network request in an ordinary repository, where it is an
integrity failure. In a repository already configured with a promisor object
source, using Git 2.45 or newer, a missing ancestor instead permits one direct
`--refetch`. Git 2.45 is required so every graph-inspection subprocess can also
disable implicit lazy fetching. Either request contains all and only the causal
root's retained version-tag aliases; it never includes another root merely
because that root is also missing. The single fetch process:

- reads source-only refspecs, one per line, from a fully prepared anonymous
  regular file on standard input;
- writes no local ref or `FETCH_HEAD`;
- disables progress, shallow-file updates, filters, configured bundle-URI
  acquisition, tags, submodules, and automatic maintenance;
- uses an explicitly empty ref map so configured fetch mappings cannot create
  a destination ref;
- does not fetch a raw object ID; and
- gives the top-level Git process no destination literal or source ref in its
  argument vector. Git may pass the configured destination to a trusted
  transport helper as described under
  [Push destination](#push-destination).

Before the child starts, GHerrit has bounded and finished every input byte,
verified and reopened the same regular file read-only, rewound it, removed its
name, and closed every GHerrit-owned writable handle. This removes the input
producer and pipe capacity from the command lifetime and gives the child only a
read-only standard-input handle. It is not kernel-enforced inode immutability
or a sandbox boundary against the child itself. Git and its local helpers run
with the user's authority and are trusted extensions, as described under
[Push destination](#push-destination).

Complete or otherwise invalid initial graph evidence performs no acquisition.
After one successful fetch, GHerrit performs exactly one authoritative graph
reload. A failed fetch performs no reload, and a remaining or different hole
on the final reload is returned without another request. Transport may bring
objects adjacent to the named refs and thereby fill more than the first hole;
if it does not, a fresh hook invocation can reobserve and select its own first
remaining causal root. There is no ordinary-fetch-then-refetch wave, unbounded
acquisition loop, or incidental lazy fetch.

### Exact local GitHub evidence

For each local ID `G`, GHerrit first queries one repository connection filtered
by the exact head name and OPEN lifecycle state:

```graphql
pullRequests(
  headRefName: "G"
  states: [OPEN]
  first: 1
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

Every OPEN connection is exhausted so all visible same-repository candidates
participate in canonical selection. For each ID whose OPEN connection contains
no same-repository row, GHerrit then probes terminal history:

```graphql
pullRequests(
  headRefName: "G"
  states: [CLOSED, MERGED]
  first: 1
  after: CURSOR
) {
  nodes { number id state headRefName isCrossRepository }
  pageInfo { hasNextPage endCursor }
}
```

The terminal probe continues past cross-repository rows. The first
same-repository CLOSED or MERGED row rejects the attempt; exhausting the
connection without one seals `Absent`. Terminal history is therefore read
only on the missing or invalid path. In particular, CLOSED rows left by
duplicate cleanup never enter the established OPEN path.

Connections for several IDs in the same wave are aliased in one document when
request limits permit. The request token retains each alias's phase, ID, and
input cursor. A response must contain exactly the requested aliases. Duplicate
JSON object members are invalid at every depth and are rejected before they
can collapse into one value. Each alias advances only its own cursor, a
repeated cursor is invalid, and an advancing page must contain its one
requested row. No successful observation is exposed until every required
connection in both waves is exhausted.

The page size is always one. Each connection has a budget for its first
returned row, and the entire two-wave observation has a shared budget for
exactly 99 additional rows. An unused first-row budget belongs only to its
connection, so a large local stack cannot donate rows to one pathological
head. For `N` local IDs and `K <= N` terminal probes, the observation accepts
at most `N + K + 99` rows and `2N + 2K + 99` pages, including one possible
final empty page per connection. The established path has `K = 0`. This
deliberately spends more round trips on unusually long same-name OPEN or fork
histories in exchange for bounded backend work and a simple resource proof.
Normal connections still fit in one page, and aliases batch independent
connections into the same request.

The budget counts every returned row before repository-identity filtering.
GitHub's repository pull-request connection can filter by head name but not by
head repository, so cross-repository pull requests which reuse a local ID still
consume backend work. Counting them keeps that work bounded. If colliding fork
history exhausts the budget, the observation fails before every write rather
than downloading an unbounded history. This is an explicit resource limit, not
a claim that unrelated forks can never impede progress.

The first OPEN request also obtains the repository node ID and GitHub default
branch. Later OPEN pages and terminal probes contain only the repository
connection fields required for their exact phase.

Cross-repository rows do not describe the local branch and are ignored after
their response shape and requested head name are validated. Pagination still
continues past them while the shared budget remains. Same-repository OPEN
identities are registered and folded as their pages arrive. Exhaustion yields
either a nonempty ordered OPEN set, whose lowest number is canonical and whose
higher identities are repairable duplicates, or authority to begin the
terminal probe. A same-repository terminal row rejects before planning;
terminal identities do not enter the planner. Exhausting that probe yields an
opaque sealed `Absent` value. `Absent` means only that these two exact
connections found neither a same-repository OPEN nor terminal row; it does not
assert snapshot isolation or timeless absence.

Every OPEN row validates its coupled pull request number and node ID. For every
same-repository OPEN row, both component namespaces are then retained
independently across the relevant local observation, so a later create receipt
cannot reuse either component. Every OPEN row additionally retains the fields
needed to validate its head, base, and landing state. Only the canonical row
retains title and body as projection input; duplicates retain only their exact
identities after validation. Terminal and cross-repository rows discard their
validated payload. No repository-wide or nonlocal identity collection is
downloaded or placed in a collision registry.

Any GraphQL response with substantive errors is unusable, even when it also
contains partial data. A response which contains only a recognized resource
limit error and no data may cause the same logical pages to be rebuilt with a
smaller alias batch. Page size is fixed and never backs off. Duplicate JSON
members, missing or extra aliases, malformed rows, invalid object IDs, null
required fields, and pagination contradictions fail the attempt before a
write.

Read-only transport failures may be retried with bounded delays. Mutation
requests are never retried in the same attempt.

### Parallel read work

After local derivation and GitHub authentication, exact local Git namespace
observation and exact local GitHub observation may proceed concurrently. Graph
loading starts after exact Git evidence supplies its published-version roots,
and may overlap any GitHub pagination which is still in progress. Their results
meet once in the planner.

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
- one sealed `Absent` or a nonempty validated OPEN set with its canonical row
  and higher-numbered duplicates, after terminal-only history has been
  rejected; and
- a bounded final-projection recipe. Existing pull requests can supply their
  numbers immediately; missing pull request numbers remain parameters until
  exact create receipts supply them.

For a public stack, the initial observation separately derives exactly one of:

- create `refs/heads/B` from authoritative absence to the stack tip;
- force-advance `refs/heads/B` from its exact observed object to the stack tip;
  or
- no operation because the ref already names the stack tip.

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

1. With no version tags, the head, owned base, and marker are all absent. Any
   other zero-version combination is invalid.
2. Version tags are lightweight, canonical, contiguous, and immutable.
3. Local-stack sealing has already proved the proposal's identity, literal
   first parent, default descent, and unique occurrence in complete proposal
   ancestry. Every distinct external published head carries exactly one
   expected change ID.
4. The current head equals the latest version head.
5. The current owned base equals that head's literal first parent.
6. The optional marker is lightweight and points to a published version head.
7. Every required revision is present in either the sealed local evidence or
   the complete external graph, including each literal first parent.
8. The expected change ID occurs nowhere in the union of the proper ancestry
   of every distinct external published head, following all parents.

Every contiguous version position is preserved even when two adjacent tags
name the same literal revision. Publication does not create a new version when
the proposal already equals the current version, but validation does not infer
how an existing immutable tag was created or discard it.

The local-stack invariant and last rule together guarantee the operational
owned-base and default-base safety properties. If an external published head
were reachable from the proposal's first parent or agreed default tip, it would
be another occurrence of the active ID in complete proposal ancestry, which
local-stack sealing rejects. If any managed head were reachable from an
external published first parent, it would be an expected-ID proper ancestor of
that external head, which the union walk rejects. A published slot equal to the
proposal shares its already-sealed local revision. Separate proposal, base, and
default graph walks cannot reject an additional sealed input, so history
validation loads external published evidence and performs one union
proper-ancestry walk over it.

### Pull request state

The supported local realities are:

| Published history | Marker | Pull requests | Result |
| --- | --- | --- | --- |
| absent or present | absent | `Absent` | Create on the owned base after the complete initial-ref barrier |
| present | absent | one or more valid OPEN rows on the owned base | Publish marker, then project the pull request state |
| present | present | one or more valid OPEN rows | Project the pull request state |
| any | present | `Absent` | Fail closed |
| any | any | terminal-only history | Reject before planning |
| absent | any | any OPEN row | Reject unexplained pull request state |

Every OPEN pull request must:

- have the exact same-repository head name requested for the local ID;
- have a head object ID found in validated published history;
- use either its exact owned base or the exact agreed default branch;
- have an owned-base object ID found among validated published first parents;
- have the exact default object ID when based on the default branch; and
- remain on its owned base while it lacks a marker.

Native auto-merge and merge-queue state are invalid when the pull request is or
will become based on an owned base. GitHub could otherwise land a pull request
whose base is not the repository default branch. They are also invalid on
every noncanonical duplicate, because that row is destined for closure rather
than landing.

After all OPEN rows pass those checks, the lowest pull request number is the
canonical identity. This choice is independent of pagination order. Every
higher OPEN row is scheduled for exact closure, and only the canonical row's
title, body, and base participate in final projection. A stale observation may
miss a lower row and temporarily select a higher one, but it can never close
an unseen lower identity. Because GitHub numbers later creations above earlier
ones, a later complete observation can only keep the selected canonical row or
replace it with a lower row and close the former selection.

An `Absent` observation plus exact marker absence creates a one-use
authorization for the owned-base request key. The authorization does not claim
that GitHub is current. If an unmarked provisional pull request was omitted,
the repeated create may be refused or may establish a higher-numbered
duplicate. Refusal ends the attempt without later authority; acceptance
returns the only identity this attempt may use, and a later complete
observation repairs the duplicate set. An `Absent` observation plus a marker
never authorizes creation.

### Immutable plan

The final plan owns all decisions which can be made before writing. It contains
typed tuple operations, the optional public branch transition, provisional
create specifications, marker preflight, and a bounded recipe for the final
pull request projection. That projection closes exact duplicate identities and
updates canonical identities. Raw Git records, GraphQL JSON, aliases, cursors,
and freely recombinable booleans do not enter it.

For a stack with missing pull requests, final bodies and minimal updates are
not yet values in the plan. The recipe is parameterized by the complete set of
pending pull request numbers. Exact create receipts consume that recipe once,
after validating every assigned number and identity, and produce only the
marker operations authorized by those creates plus the exact final
projection.
Marker operations already authorized by observed OPEN pull requests remain a
separate preplanned set. Execution may combine the two authorized sets into one
atomic push; their authority does not become interchangeable. This is one
staged plan, not a second observation or a replan.

One-use values represent authority transitions. A caller cannot construct a
create without consuming exact absence and marker evidence, cannot construct a
new marker for a created pull request without consuming an exact create
receipt, and cannot access the final projection before consuming exact marker
acknowledgement.

## Durable publication sequence

The state machine is:

```text
capture local branch, management intent, and HEAD
    -> if unmanaged, return without remote work
    -> prove that a managed enclosing push is a no-op
    -> resolve destination
    -> observe remote HEAD and optional public branch; derive local stack
    -> if empty, reconcile the optional public branch
       and return before GitHub authentication
    -> observe and validate exact local Git and GitHub state
    -> publish required initial Git refs: tuples, then optional public branch
    -> create missing pull requests
    -> publish required pull request markers
    -> project pull request state: close higher OPEN duplicates
       and update canonical pull requests
```

Each arrow after planning is an acknowledgement barrier. Work after a barrier
is unavailable until every required effect before it has an exact usable
acknowledgement. A stage with no work crosses its barrier immediately.

### Publish initial Git refs

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

An optional public branch operation is ordered after every tuple operation. It
may share the final bounded atomic batch or occupy its own batch. It creates the
ref under an exact absence lease or force-advances it under a lease for the
exact initially observed object. The desired object is the local stack tip. If
the ref was already desired, the plan contains no public operation. Ordering it
last ensures that the public branch cannot advance before an earlier tuple
batch, while the complete initial-ref barrier ensures that no GitHub mutation
can precede it.

Every mutable ref uses an exact force-with-lease expectation. Every new tag or
branch uses an exact absence lease. A malformed, incomplete, or ambiguous
porcelain acknowledgement ends the attempt. Creation remains inaccessible
unless every required initial-ref batch is exactly acknowledged.

Git reports a requested ref which already has its desired object as an
acknowledged up-to-date no-op. This applies even when another identical
publisher made the planned old-value or absence lease stale. Such an
acknowledgement proves the same postcondition as performing the write. A ref at
a different object still rejects under the exact lease.

### Create missing pull requests

Creates run only after the initial-ref barrier, so every requested head and
owned base already exists and an optional public branch is at the desired tip.
Each create includes the exact destination repository,
head repository, `G` head, `gherrit-bases/G` base, title, provisional body, and
client mutation ID.

The provisional body does not depend on pull request numbers which GitHub has
not assigned. The permanent owned base is safe for roots and nonroots.

Create aliases may be batched. GitHub may apply a subset before an error or
lost response makes the acknowledgement indeterminate. The same attempt stops
without publishing markers or final updates for that batch. A later invocation
reconstructs durable state through the OPEN-first observation.

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

Every local change whose validated nonempty OPEN set lacks a marker requires
one marker operation. The set authorizes its marker during planning. A newly
created pull request authorizes its preflighted marker only after its exact
receipt is consumed.

Marker publication is a separate bounded atomic Git push. Each operation
creates only `refs/tags/gherrit/G/pr`, uses an absence lease, and targets a
validated published head. It never moves a head, base, or version tag.

The final pull request projection remains inaccessible until every required
marker batch is exactly acknowledged. An indeterminate marker result ends the
attempt with each marker either absent or durably present and every affected
pull request still on a validated safe base.

### Close duplicate pull requests

For each local change, every observed valid OPEN identity above the canonical
minimum is closed after the marker barrier. Closures use exact preplanned node
identities. A receipt must return the same pull request number and node ID in
CLOSED state.

Closing a duplicate changes no comparison ref and cannot invalidate the
canonical update. Close and update operations therefore share the same bounded
GraphQL projection requests rather than imposing an acknowledgement barrier
between independent effects. Close aliases precede update aliases for a
deterministic document, but any subset of aliases may land safely. An
indeterminate response ends the attempt, and a later observation schedules
only the closures and updates which remain. Ordinary converged publication has
no closure operation.

A delayed publisher may create or update a higher duplicate after another
attempt's cleanup. This does not make a lower canonical row unsafe or permit
the stale publisher to close it. Once competing publishers stop, a fresh
attempt observes and closes the remaining higher rows.

### Apply final pull request state

After the marker barrier, GHerrit sends only fields which differ from the
desired projection:

- title when needed;
- complete body when needed; and
- base name when root status or provisional creation requires it.

A root targets the exact default branch. A nonroot targets its own owned base.
Every update names the canonical exact preplanned GraphQL node identity, and
its receipt must return that same number and node ID in OPEN state.

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

For a root, the same sealed-stack and external-history proof guarantees:

```text
for every r in R_G: not (H(r) <= default_tip)
```

This is not a separate graph walk: the proposal strictly descends the agreed
default tip, so any managed head reachable from that tip would also lie in
proposal ancestry. A distinct external head would duplicate the active ID
there, while the proposal itself cannot be reachable from its proper ancestor
in an acyclic commit graph. The default branch is stable by assumption during
the attempt.

### Mutation footprint

Only local changes and the optional checked public branch produce writes. A
valid nonlocal pull request for `X` uses head `X` and either
`gherrit-bases/X` or the stable default branch. A local publication for
`G != X` moves neither of those refs and cannot alter `X`'s projection.

The enclosing Git push is a proved local no-op for both public and private
stacks. GHerrit writes the public projection inside its acknowledged initial
Git stage. Its checked grammar is disjoint from all change-owned heads and
bases and from the default branch. Public presentation links use that same
checked name and the repository identity bound to the publication destination.
As with any force-updated feature branch, consumers must not treat the public
projection as an independently writable or stable base.

A legacy, manually retargeted, or otherwise corrupt pull request could use a
local change's head or owned base as its own base. This protocol does not scan
the repository to discover that unsupported state. The assumption that
change-owned branches are reserved and not used by other pull requests is
therefore necessary. Supporting migration or arbitrary manual cross-base use
requires a separate exact base-consumer guard or migration protocol.

### Safe visible prefixes

Every externally visible prefix is safe:

1. The enclosing push has no ref update after the hook returns.
2. Read-only observation changes no managed remote state.
3. An atomic initial-ref batch leaves every included change wholly old or
   wholly new and the optional public ref either old or at the desired tip.
4. Earlier acknowledged initial-ref batches contain only validated tuples;
   because the public operation is last, they cannot expose it prematurely.
5. Every created pull request starts on its permanent safe owned base, even
   when it is a higher-numbered duplicate.
6. A marker push changes no pull request comparison ref.
7. A duplicate closure changes no comparison ref and never targets the
   lowest-numbered visible OPEN row.
8. Duplicate closures and canonical updates become available only after
   durable existence markers.
9. A complete update alias leaves the canonical pull request on an old
   validated base, its owned base, or its final validated base.

Title and body fields do not affect reachability.

### Failure prefixes

| Failure point | Durable result | Retry behavior |
| --- | --- | --- |
| Before initial-ref publication | No external write | Reobserve and replan |
| Initial-ref acknowledgement lost | Batch is wholly old or wholly new | Exact refs reveal the result |
| Some initial-ref batches acknowledged | Safe complete tuple prefix; public ref remains old until its final unit | Publish remaining initial refs |
| Concurrent public ref write | Exact public lease rejects the containing batch | The rejected attempt preserves it; a fresh attempt reobserves and may deliberately project the desired tip |
| Unrelated ordinary public-ref D/F conflict | Remote rejects the containing batch | Remove the conflicting ref, rename the public branch, or use private mode |
| Create acknowledgement lost | A provisional PR may exist | Repeat the owned-base request or observe exact local rows |
| Some create aliases applied | Some provisional PRs exist | Observe exact local IDs |
| Marker acknowledgement lost | Marker is absent or immutable | Reobserve exact tag namespace |
| Some marker batches acknowledged | Safe marker subset | Publish remaining markers |
| Final-projection acknowledgement lost | Each close or update alias may or may not have landed | Reobserve exact local IDs |
| Some projection aliases applied | Canonical remains OPEN; some duplicates are CLOSED and some fields are final | Project only the remaining work |

Nothing is rolled back. Rollback would add writes and could restore an older
combination which no longer belongs to the validated plan.

### Stale GitHub visibility

The OPEN wave and every required terminal probe are exhaustive over their
returned cursors, but they are not a snapshot and GitHub may temporarily omit
a durable row or return older field values.

If an unmarked provisional OPEN pull request is omitted, marker absence permits
the same owned-base create. GitHub may refuse the request or accept a
higher-numbered duplicate; retargeting an earlier root away from the owned base
is one known reason acceptance can occur. A failed or indeterminate create
supplies no identity and therefore cannot release a marker or final projection
in that attempt. Later complete visibility selects the lowest OPEN number and
closes every visible higher row.

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
writes a conflicting tuple, public branch, or pull request projection. Other
publishers may still perform effects required by the fixed intent. Measure its
remaining durable work lexicographically by:

1. missing Git tuples;
2. a missing or stale public branch projection;
3. missing pull requests;
4. missing pull request markers;
5. higher-numbered OPEN duplicate pull requests; and
6. stale canonical projection fields.

After stabilization, an acknowledged required effect reduces the earliest
affected component. Another publisher may perform the same Git effect first,
in which case the pending push is acknowledged as an up-to-date no-op or a
fresh attempt observes the desired state. Either path makes the same
reduction. An indeterminate effect either reduces the measure or leaves it
unchanged. Exact Git advertisements expose durable Git progress, and eventual
stable GitHub visibility removes already-applied GitHub work from later plans.
If still-required operations eventually receive usable acknowledgements,
fresh attempts reach a plan with no action. A delayed create can temporarily
increase the duplicate component, but after publishers stop every new pull
request number is fixed and each acknowledged closure reduces that finite set.

Before stabilization, an initial-ref push whose desired object conflicts with
the current object must satisfy exact leases, and a rejected push stops before
its later stages. This includes the public ref. An attempt which had already
observed its tuple as desired may emit a later stale projection after another
publisher changes the tuple, but every allowed projection is safe. Conflicting
effects may increase the remaining work for either intent, so the protocol
promises safety rather than progress until one complete intent stabilizes. It
never rolls back to manufacture progress.

### Concurrent mutation limit

No observation design can close a time-of-check/time-of-use race against a
writer which bypasses this protocol. For example, another actor can close the
canonical pull request after observation but before a final update, while a
stale publisher may create a higher row from older authority.

Repository-wide observation, an extra re-observation, or combining OPEN and
terminal queries differently merely moves this race. Supporting such writers
requires a backend idempotency or locking primitive, or a different durable
protocol. This design supports concurrent protocol publishers but states the
boundary against independent lifecycle and topology writers plainly.

## Adapter contracts

### Enclosing hook boundary

Git invokes `pre-push` with a remote name and location and writes every planned
ref update to the hook's standard input. Every managed branch is configured
with `pushRemote = .`, `remote = .`, and its own local merge ref. A managed Git
invocation is accepted only when both hook arguments are exactly `.` and
standard input reaches EOF without one byte of update data. This proves that
the enclosing Git process has no ref mutation after GHerrit's internal
protocol is acknowledged. It does not prove that the enclosing process will
succeed: another hook or a later Git failure may reject it after GHerrit has
durably published, and a fresh invocation then reobserves that state.

`gherrit manage` also recognizes one exact compatibility form for an existing
public branch: public management state, `pushRemote` equal to the one valid
`gherrit.remote` value, or `origin` when that setting is absent, selected by the
prior configuration model, `remote = .`, and the branch's own local merge ref.
It consults that legacy remote only while matching this complete tuple. Only
the complete public-state form is treated as GHerrit-owned and rewritten to the
loopback form without `--force`. Any near miss remains drift, and the exception
does not apply to private, unmanaged, or unconfigured branches. The rewritten
current form is ordinarily idempotent without consulting destination
configuration again.

Plain `git push` has the accepted shape. An external destination or a refspec
which would produce an update is rejected. Explicit `git push .` and an
already-up-to-date refspec are observationally identical to the plain form and
may be accepted.

The hidden direct `gherrit hook pre-push` entry point has no enclosing Git
effect and therefore uses zero arguments and requires no input. One supplied
argument is invalid; two form the Git-hook invocation. An unmanaged Git
invocation returns before reading standard input, so a composite hook can pass
the complete stream to later checks. Internal GHerrit pushes return through the
recursion guard before management or input handling. Installed and composite
wrappers must forward both arguments unchanged and leave standard input
connected to GHerrit.

Git supplies no hook input which distinguishes `git push --dry-run` from a real
push. The dry-run form therefore performs GHerrit's real internal publication;
only the already-empty enclosing loopback push is dry-run. This is a documented
limitation of the pre-push interface.

### Push destination

All Git reads, acquisitions, and writes use one destination resolved from the
configured remote according to Git's ordinary URL and push-URL rules. GitHub
repository coordinates and body links derive from that same resolved
destination.

Production GitHub URI destinations use Git's built-in `http`, `https`, `git`,
or `ssh` transport spelling exactly in lower case. The `github.com` host name
is compared without ASCII case distinctions. This prevents a case-variant
scheme from selecting a custom `git-remote-*` helper while the API client acts
on the production GitHub repository.

The implementation uses a command-scoped internal remote rather than placing
the destination literal in an argument vector constructed by GHerrit or
writing it to repository configuration. This requires Git 2.31 or newer,
whose `--config-env` option supplies configuration values without placing
those values in the top-level Git process's arguments. Promisor and
partial-clone repositories require Git 2.45 or newer so implicit lazy fetching
can be disabled. The adapter chooses a remote name absent from all
configuration activated by the destination. It configures that name with the
exact URL and push URL plus the effective `remote.<configured>.proxy` and
`remote.<configured>.proxyAuthMethod` values, when present. GHerrit supplies
these private values through dedicated environment variables rather than its
direct child's argument vector. Final validation proves that the generated
remote has exactly those planned keys and values, and a URL-only probe proves
that another `insteadOf` rule cannot rewrite it.

That argument-vector guarantee ends at the top-level Git process. Git may pass
the URL in an argument to a custom remote helper or another transport process
when its own protocol requires it. Those helpers and transports run with the
user's authority and are trusted extensions. GHerrit therefore does not
promise that a destination remains absent from every descendant process's
arguments; a destination which requires that stronger secrecy property is
outside this protocol's guarantee.

The configured remote is not an open-ended source of behavior for the
generated remote. Fetch and push refspecs, mirror and tag behavior, pruning,
promisor and partial-clone settings, and unknown keys are not copied. GHerrit
rejects `uploadpack`, `receivepack`, `vcs`, and `serverOption` on the configured
remote before external I/O because those settings change transport identity,
the executed program, or protocol input and cannot be represented by the
adapter's exact-destination contract. Proxy and proxy-authentication settings
are independent: either can be present, and an explicitly empty proxy remains
distinct from an absent proxy. Their effective last values follow Git's normal
configuration precedence. Non-UTF-8 values and values containing Unicode
control characters are rejected without rendering the value.

Destination-bearing children remove inherited `GIT_TRACE*` and
`GIT_CURL_VERBOSE` values and explicitly disable all three Trace2 target
families, including system or global targets which ignore command-line
configuration; the literal-graph boundary separately removes inherited
object-database, replacement, graft, and shallow overrides. HTTP redirects are
disabled. Pushes also disable implicit followed tags and submodule recursion
and clear inherited push options, so configuration cannot add refs, suppress
the planned repository push, write submodule remotes, or add server options.

Ordinary credential helpers and credential, HTTP proxy, SSH, and prompt-control
configuration and environment remain available as user-supplied transport
inputs. The generated remote's own proxy inputs are the two explicitly copied
configured-remote values above; other transport inputs are not scoped to a
remote name and therefore remain effective without copying. Helpers and
commands are trusted extensions: GHerrit cannot recognize or redact an
arbitrary bare secret which they choose to print. GHerrit does not attempt an
authentication-preserving environment allowlist.

Every network command is bounded by output, execution, and cleanup deadlines.
Failures include bounded terminal-safe diagnostics which redact the private
destination, the copied remote transport values, and path- or URL-shaped
values known to GHerrit. This protects GHerrit-controlled values, not arbitrary
diagnostics emitted by a trusted credential helper, SSH command, transport
helper, or hook.

### Git observation and acquisition

Remote observation is byte-oriented and validates exact ref names. It rejects:

- malformed object IDs or ref records;
- symbolic `HEAD` without a usable target;
- a duplicate, malformed, or symbolic requested public branch;
- disagreement between `HEAD` and its exact target ref;
- an unexpected namespace root;
- noncanonical or noncontiguous versions;
- annotated managed tags; and
- any response which cannot prove complete coverage of the requested local
  names.

Exact local queries, initial-ref pushes, and marker pushes use a conservative
variable-argument budget. Source-ref acquisition uses a separately bounded
standard-input payload. Batching never splits a tuple, public branch, or
marker operation.

Observation and acquisition do not update local branches, tags,
remote-tracking refs, `FETCH_HEAD`, or Git configuration. A successful
acquisition can add ordinary Git object data to the repository object
database; that is its only intended local side effect.

### Git publication acknowledgements

Git publication uses `--atomic`, porcelain output, and exact leases. The
adapter compares the acknowledgement with the complete requested ref set. A
missing, duplicate, extra, rejected, or malformed status makes the result
indeterminate or failed and withholds the next stage.

A composite pre-push hook may write to the internal push's standard output
before Git emits its porcelain acknowledgement, with or without a terminating
line feed. GHerrit removes the exact final `Done` record and the fixed number
of status lines, then selects the final `To ` header suffix from the remaining
prefix. Earlier output, including a forged complete-looking block or non-UTF-8
bytes, cannot supply a receipt. The displayed destination is discarded, and
output after Git's final `Done` record is invalid.

An exact up-to-date status is a usable acknowledgement: it proves that the ref
already has the requested object even if its planned lease is stale. The
adapter does not confuse that successful no-op with a lease rejection caused
by a different current object.

Each internal publication command sets an internal environment marker to the
exact per-worktree Git directory and generated remote name. The GHerrit hook
opens the current repository and returns immediately only when both marker
components match it, Git supplies the same nonempty remote name, and the
remote location is nonempty. An inherited marker therefore cannot suppress a
nested push from another repository or linked worktree which happens to use
the same remote name. A composite pre-push hook continues to run its other
checks for internal tuple, public-branch, and marker pushes. A wrapper which
invokes GHerrit must preserve the marker, arguments, and input stream. The
marker prevents cooperative recursion; it is not an authentication or
security boundary.

When such an independent check definitely rejects without any requested ref
mutation, GHerrit includes a bounded terminal-safe rendering of the captured
diagnostic. It removes the exact private destination, conservatively redacts
other path- and URL-like tokens, and escapes control characters. A normally
exiting command with indeterminate receipts may include the same rendering as
explicitly non-authoritative context; it does not weaken the indeterminate
effect classification. Failures which prevent safe output capture, including
timeouts and incomplete cleanup, expose no child output. This rendering cannot
identify an arbitrary bare secret printed by a user-supplied helper or hook;
those programs remain responsible for their own diagnostic hygiene.

### GraphQL queries

Read-only GraphQL requests have bounded serialized documents, response bodies,
connection and total-attempt timeouts, and a finite transient retry schedule.
Recognized resource-limit failures may reduce the number of aliases without
advancing an input cursor. The fixed one-row connection page is not another
backoff dimension.

The OPEN-first local-ID accumulator owns completeness. It accepts a page only
when the response alias, phase, requested ID, input cursor, returned head name,
and next cursor agree. It exposes one exact ordered local observation only
after every OPEN connection and every required terminal probe is exhausted.

### GraphQL mutations

Create, duplicate-close, and update documents use JSON string escaping for
every external value. Requests are limited by both alias count and serialized
byte size. A single operation which exceeds the byte limit rejects before
transmission.

A mutation is sent exactly once. Transport failure, timeout, non-success HTTP
status, GraphQL errors, missing or extra aliases, a null operation, malformed
payload, or an invalid receipt is indeterminate. The current attempt performs
no mutation retry and crosses no later barrier.

## Code model

The implementation uses dedicated values for evidence and authority. The
essential shape is:

```text
PushDestination
    -> InitialRemoteObservation(RemoteDefault, OptionalPublicBranchState)
    -> LocalStack

LocalStack + PushDestination
    -> ExactLocalGitObservation
    -> ExactLocalPullRequestObservation
    -> CommitGraphEvidence

ExactLocalPullRequestObservation[G]
    = SealedAbsent
    | NonemptyOpen(CanonicalProjection, HigherDuplicateIdentities)

terminal-only exact history
    -> pre-planning rejection

validated local evidence
    -> PublicationPlan
    -> InitialRefStage(Tuples, OptionalPublicBranchTransition)
    -> CreateStage
    -> MarkerStage
    -> FinalPullRequestProjection(DuplicateClosures, CanonicalUpdates)
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
- a public branch transition without an exact observed value or authoritative
  absence;
- a final duplicate closure or canonical update without marker
  acknowledgement;
- a duplicate closure for the lowest-numbered visible OPEN identity; or
- a create receipt for an unplanned change.

Invalid external combinations become errors before a plan exists rather than
additional planner states.

## Performance

Network latency and backend query execution dominate ordinary publication.
GHerrit therefore scopes every query namespace to the local stack instead of
scanning repository-wide refs or pull requests. The number of requested Git
namespaces and GraphQL connections scales with the number of local change IDs.
Total response pages, payload, and validation work additionally scale with the
visible OPEN pull requests for those IDs and with the literal commit graph
reachable from their relevant published versions. Terminal history is probed
only when an exact OPEN connection has no same-repository row. A single local
ID can therefore still require substantial work, but successfully closed
duplicate history does not tax later established pushes. Pull requests with
other head names do not enter the attempt. Cross-repository pull requests which
reuse an exact local head name can enter the bounded raw response, but are
discarded before they become identity or planning evidence.

An empty private stack performs only the Git work needed to identify its
boundary. An empty public stack may additionally perform one exact leased
public-branch push, but performs none when the branch is already current.
Neither authenticates to GitHub or sends a GraphQL request.

A normal nonempty attempt performs:

- one small symbolic remote `HEAD` observation which includes the optional
  exact public branch in the same request;
- byte-bounded exact Git reads for the default and local change namespaces;
- at most one exact object-acquisition process and one final graph reload, only
  when the initial graph load reports an authorized missing object;
- one logical OPEN-first GraphQL observation, with a second terminal-probe wave
  only for local IDs lacking a visible same-repository OPEN row; and
- only the mutation and push stages which have actual work.

The ordinary one-OPEN-row path has no duplicate-close operation. When a
canonical update is also needed, cleanup aliases share its bounded GraphQL
request and add no critical-path round trip. A close-only repair sends only the
bounded projection requests needed for higher visible rows.

There is no repository-wide GitHub OPEN scan, terminal history on the
established path, nonlocal tag accepted into logical evidence, nonlocal object
acquisition, marker confirmation query, rollback, or same-attempt
re-observation. Git advertisement breadth is the explicit transport boundary
described under exact local Git evidence.

Independent local connections and effects are batched to reduce round trips.
Alias batches back off when request, response, or backend resource limits
require it. Connection pages remain fixed at one row. Expensive graph work is
shared across local histories, and duplicate object IDs are loaded once
without collapsing distinct version positions.

Public mode adds at most one atomic ref unit. It performs no push when already
current, normally shares the final tuple batch, and adds at most one push batch
when it is the only initial work or the argument budget requires separation.
GHerrit does not scan unrelated ordinary refs for D/F conflicts, so the public
observation remains constant-size; the remote may reject such a conflict at
the write boundary.

## Testing obligations

[The testing strategy](../agent_docs/testing.md) assigns each claim to its
lowest faithful layer. At minimum, coverage proves:

- exact local Git ref and tag observation, including authoritative absence;
- exact public branch presence and absence, malformed and symbolic records,
  checked default-branch disjointness, and no extra observation round trip;
- the first-component public-name grammar, both directions of public/default
  D/F conflict, change-ID/default ancestor conflict, and unrelated ordinary-ref
  D/F rejection before GitHub mutation;
- deterministic causal-root provenance and exact source-ref acquisition
  payloads;
- no acquisition for complete or invalid graphs, or for missing ancestry in an
  ordinary repository;
- one negotiated acquisition for a missing advertised root, one direct
  refetch for missing ancestry in a promisor repository, and no second request
  after the authoritative reload;
- private empty-stack completion and public empty-stack create, advance, and
  already-current behavior before GitHub token access or requests;
- initial remote `HEAD` discovery before local-stack validation, followed by
  pending-autosquash rejection before trailer validation, later exact-local
  Git observation, GitHub authentication or requests, or any write;
- non-UTF-8 subjects and bodies rejected during local-stack validation before
  exact-local Git observation, GitHub authentication, or any write;
- a non-UTF-8 checked-out branch rejected before lossy management lookup,
  external observation, or any write;
- independent CommonMark label escaping and UTF-8 URL-path encoding for every
  valid public branch byte class;
- complete independently paginated OPEN queries for exact local IDs and
  bounded terminal probes only after OPEN absence;
- sealed absence, one OPEN, terminal-only, canonical selection among duplicate
  OPEN rows, fork, and malformed response outcomes;
- no repository-wide or nonlocal observation;
- historical reachability across all published and proposed pairings;
- complete tuple atomicity and exact public create/advance leases at a real Git
  remote, including public-last batch ordering and conflict rollback;
- already-current public no-op planning, same-desired race acknowledgement,
  deliberate replacement of an already-observed value, movement after
  observation or acknowledgement, lost acknowledgement, and fresh recovery;
- exact create and mixed final-projection receipt validation, including no
  ordinary-path close operation;
- generated-body equality which normalizes only CRLF pairs and preserves every
  other whitespace byte and Unicode scalar value;
- every patch-history Base link using its literal hexadecimal first-parent
  object ID rather than any movable branch name;
- repository-bound internal publication recursion suppression without
  bypassing other pre-push checks, including nested repositories and linked
  worktrees which reuse the internal remote name;
- exact installed-hook argument and input enforcement, loopback publication,
  and unmanaged input preservation;
- zero-argument direct invocation, incomplete and wrong arguments, wrapper
  forwarding, real dry-run publication, and later enclosing-push failure;
- no automatic public-ref deletion after privatization or rename;
- initial-ref, create, marker, and mixed final-projection interruption
  prefixes;
- protocol-conforming publisher interleavings, exact-lease conflicts, delayed
  creates after root retargeting, and deterministic duplicate cleanup;
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
- the initial Git barrier is crossed only after an optional public branch was
  observed already at the stack tip or its required exact-leased transition
  was acknowledged;
- every created pull request starts on its permanent safe owned base;
- every higher-numbered visible duplicate is closed and every canonical update
  begins only after durable marker acknowledgement;
- every acknowledged and indeterminate prefix remains safe;
- protocol-conforming publishers remain safe when their local stacks are
  disjoint, overlap, or temporarily express different stacks or revisions;
- a marked identity is never recreated because GitHub omitted it;
- crashes and lost acknowledgements require no rollback or transaction log;
- fresh attempts converge after one complete local intent stabilizes, required
  observations remain within their resource bounds, durable effects become
  stably visible, and required operations eventually receive usable
  acknowledgements; and
- accepted evidence is scoped to exact local head names and bounded even when
  forks reuse those names: for `N` IDs and `K <= N` terminal probes, pull
  request pagination accepts at most `N + K + 99` raw rows and
  `2N + 2K + 99` pages, while Git validation additionally depends on the
  distinct external published-version ancestry reachable from those IDs and
  Git command execution remains subject to the advertisement bounds above.

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
- preservation of independent public-branch writes which were already visible
  when GHerrit observed the branch;
- a continuously current public target while concurrent publishers are active;
- creation of a public ref whose path has an unrelated ordinary-ref D/F
  conflict;
- deletion of a stale public ref after a branch becomes private or is renamed;
- a side-effect-free `git push --dry-run` on a managed branch;
- use of native auto-merge or merge queues for owned-base pull requests or any
  noncanonical duplicate;
- progress when Git's broad advertisement or unrelated tail-matching refs keep
  a required observation above its 120-second or 64-MiB command boundary;
- progress when cross-repository pull requests which reuse local change IDs
  exhaust the bounded observation budget; or
- automatic rebasing after a root pull request merges.

## Appendix: future compatibility work

### Legacy pull request migration

This protocol does not adopt a pull request which predates owned bases and
immutable pull request markers. A future migration may authenticate each
legacy pull request, prove its historical head and base, publish the owned base
and marker, and retarget it in an order which preserves a safe comparison at
every durable prefix. Until such a migration exists, users must finish a
legacy published stack before using this representation or assign new change
IDs to begin new reviews. The publication path does not weaken its identity or
base validation to infer legacy ownership.

### Compatible landing automation

The auto-cascade GitHub Action is disabled. A future replacement must not parse
hidden metadata from a pull request body. It can instead use the merge event,
authenticated repository state, and GHerrit's owned refs and marker to identify
the next action. It must remain correct when the local user has not yet
advanced their default branch, and it must not mutate an owned base or managed
head outside this protocol.
