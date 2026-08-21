# Pre-push publication

GHerrit publishes a local stack of commits as Git refs and GitHub pull
requests. This document defines the Git representation, the safety conditions
for publishing it, and the protocol used by the pre-push hook.

The central rule is simple: every GHerrit change owns both its pull request head
branch and its pull request base branch. A change's base branch moves only when
that same change is published.

Git is the authoritative record of published change versions and durable pull
request existence markers. GitHub pull requests are derived from that record
and the local stack.

This file is the canonical specification for pre-push publication.

## Scope

This design applies when a managed branch is pushed through GHerrit's pre-push
hook. It covers:

- deriving a stack from local Git history;
- validating local and remote state;
- publishing managed Git refs;
- creating and updating GitHub pull requests;
- retries after crashes, rejected writes, and lost acknowledgements; and
- preventing GitHub from indirectly merging an active managed pull request.

It assumes one GHerrit publisher at a time, no manual mutation of managed refs,
version tags, or pull request markers, no independent automation which writes
managed state, complete Git history, exactly one configured push destination,
and no concurrent movement of the default branch.

It also assumes the following GraphQL effect boundary. Each top-level alias in
a mutation request names one complete create or update resolver. GitHub applies
that complete resolver as one indivisible pull request operation. A request may
execute any subset of its complete aliased operations, but one operation does
not partly apply its requested title, body, or base fields.

One additional GitHub backend operating contract defines the pull request
create-retry boundary. GitHub atomically prevents a create from establishing a
second same-repository OPEN pull request when an OPEN pull request already has
the identical creation key:

```text
(base repository, head repository, head ref, base ref)
```

The competing create is rejected without creating another pull request. The
[official create endpoint][create-pr-api] documents only a generic `422`
validation failure, not this uniqueness rule or its atomicity. A
[CLI discussion][same-key-pr] reports duplicate refusal, but the client may
perform a preflight query. A [direct-API incident][direct-api-duplicate]
reports `POST /pulls` returning an already-exists `422` after a preceding
lookup missed the pull request. Those observations are consistent with the
required behavior but do not prove atomic enforcement. The rule remains an
explicit operating assumption. `clientMutationId` does not provide this
property.

Exact Git leases detect many violations of these assumptions. They do not make
the Git and GitHub operations a cross-system transaction, and GitHub pull
request updates do not provide compare-and-swap protection.

Under the quiescent-writer assumptions, each successful exact Git ref
advertisement is complete authoritative evidence for every requested ref. It
is one remote ref operation, not a paginated set query, so absence of the exact
`pr` ref proves marker absence at that observation.

Safety does not require fresh GitHub observations. Convergence additionally
requires that each committed GitHub effect which remains durable eventually
becomes and remains visible to all later observations which need it for
planning, and that operations which remain necessary eventually receive usable
acknowledgements. This eventual GitHub visibility is a liveness condition, not
permission to recreate a marked identity. GitHub's paginated OPEN connection
need not include a pull request monotonically: any complete observation may
omit one which an earlier observation returned. Individual fields may likewise
expose older valid values. The durable Git marker defined below makes either
case fail closed or remain safe until eventual stable visibility supplies
progress.

## Publication model

A change has one stable identity. Its published Git state is either absent or
a nonempty immutable sequence of versions whose final entry determines one
current Git tuple, plus an optional immutable pull request marker. A change has
at most one open pull request. The pull request is a projection of the Git
state, not an independent source of publication history.

### Change identity and local stacks

Each managed commit has exactly one `gherrit-pr-id` trailer whose value is a
nonempty ASCII alphanumeric string. The same grammar is used for managed ref
components and body metadata. Its value is the change ID.

A local stack is the ordered first-parent path from the default branch to
`HEAD`. It is valid only when:

- every stack commit has exactly one valid change ID;
- active change IDs are unique;
- no change ID equals the default branch name;
- the default branch is neither `gherrit-bases` nor below
  `gherrit-bases/`;
- the default branch is an ancestor of `HEAD`;
- each stack commit's first parent is available locally; and
- ancestry required by the checks in this document is complete.

The stack order determines parent and child relationships. The implementation
does not separately store those relationships where it can derive them from the
ordered commits.

A merge commit may appear in Git history. Stack order follows its first parent,
while identity and reachability validation inspect all reachable ancestry.

### Refs owned by a change

For change ID `G`, let:

- `H_G` be its current commit;
- `P_G` be `first_parent(H_G)`;
- `refs/heads/G` be its mutable head branch;
- `refs/heads/gherrit-bases/G` be its mutable owned base branch;
- `refs/tags/gherrit/G/vN` be its immutable version tag; and
- `refs/tags/gherrit/G/pr` be its optional immutable pull request marker.

Publishing version `N` establishes this complete tuple:

```text
refs/heads/G                    -> H_G
refs/heads/gherrit-bases/G      -> P_G
refs/tags/gherrit/G/vN          -> H_G
```

The head and owned base branches are mutable because later versions of the
change move them. Version tags are immutable: a tag either does not exist and
is created once, or already exists with its established meaning.

Remote version tags are the authoritative patch history. GHerrit does not
create or consult local version tags. Adjacent version tags cannot point at the
same literal revision: GHerrit creates a version only when the desired revision
differs from the latest published revision, and normalization rejects an
adjacent duplicate. Nonadjacent versions may return to the same literal
revision, so a history such as `A, B, A` retains three distinct version
positions and patch-history rows. Repeated object IDs are deduplicated only for
graph traversal.

A complete published version couples its head, first parent, and version tag.
A remote head or base branch that does not agree with the latest version tag is
not a partially repairable state. GHerrit rejects it before publication.

The `pr` marker is a lightweight tag which points at any head in the validated
published history for `G`. Its fixed name records only that GHerrit has either
acknowledged creation of, or observed, a validated same-repository OPEN pull
request for `G`. It does not identify a version, pull request number, or
GraphQL node ID, and it never moves or disappears. The marker's target remains
valid when later publication makes another version current.

### Pull request bases

Every create request for `G`, including one whose desired projection is root,
uses the permanent owned base:

```text
headRefName = G
baseRefName = gherrit-bases/G
```

Together with the base and head repositories, these stable ref names form the
creation key. The key does not vary across amendments, rebases, reorders,
moves between stacks, or root-status changes. The base branch belongs to the
change being reviewed. It never names the mutable head branch of the change's
parent.

For example:

```text
main --- A --- B

refs/heads/A                   -> A
refs/heads/gherrit-bases/A     -> main

refs/heads/B                   -> B
refs/heads/gherrit-bases/B     -> A
```

The converged pull request for `B` compares `B` with `gherrit-bases/B`.
Publishing a new version of `A` does not move `B`'s base. Publishing a new
version of `B` moves both `B` and `gherrit-bases/B` together.

A converged root pull request targets the repository's default branch:

```text
root PR baseRefName = <default branch>
```

GHerrit still maintains the root's permanent owned base branch at the root
head's exact first parent. A newly created root remains on that safe owned base
until its required final numbered-body update moves it to the default branch.
This safe created-root prefix permits recovery without a different Git
representation or creation key.

After a create reaches its final projection, its base name changes only when
its root status changes:

- root to non-root: `<default branch>` to `gherrit-bases/G`;
- non-root to root: `gherrit-bases/G` to `<default branch>`.

The required final update also moves a newly created root from its owned base
to the default branch. Amending, rebasing, or reordering does not change the
creation key.

## State of one attempt

One attempt observes and derives all state needed to decide its writes. The Git
and GitHub reads are not an atomic cross-system snapshot. They are evidence
which must describe one valid publication state under the assumptions in
[Scope](#scope).

### Observed state

The complete logical observation contains:

- one push destination and the repository identity it names;
- one default-branch name and object ID agreed by remote Git, GitHub, and the
  corresponding local branch;
- the ordered local first-parent stack after that default branch;
- every remote head, including every managed head and owned base;
- the exact immutable version history and optional pull request marker of every
  active change;
- every row returned by the completely paginated OPEN connection, including
  its identity, head, base, lifecycle, title, body, and native merge state;
- for each local identity without a correlated open pull request, the result of
  exhaustively searching same-repository closed and merged pull requests whose
  head name is that identity; and
- the literal commit graph needed to validate every published and proposed head
  and first parent.

Finding a same-repository closed or merged pull request retires the identity.
For a local identity without a correlated OPEN pull request, exhaustive
terminal lookup is evidence rather than creation authority by itself. The
planner produces an exact stable-key creation authorization only by combining
OPEN absence, terminal exhaustion without a match, and exact absence of the
`pr` marker. That authorization does not assert absolute pull request absence:
an unmarked provisional pull request may have been omitted from the OPEN
connection, including after an earlier acknowledgement or observation but
before marker acknowledgement. A retry is safe because it uses the same
creation key. With a marker, the same terminal exhaustion fails closed until
eventual observation exposes the OPEN pull request or terminal evidence
retires the identity.

The active set is the union of local changes and same-repository open managed
pull requests. Exact tag namespaces for local IDs can be observed before pull
request correlation finishes. Correlation can add nonlocal active IDs, whose
histories and optional markers then complete the observation. The
implementation details and request shapes appear in
[Implementation contracts](#implementation-contracts).

### Derived state

From that observation, GHerrit derives for each active change:

- its absent or nonempty ordered published history and, when nonempty, its
  coherent current head, owned base, latest version tag, and optional validated
  pull request marker;
- its current or desired pull request root status;
- its validated pull request state: one open pull request, terminal evidence
  that the identity is retired, or, for a local change, one planner-produced
  exact authorization to create a pull request;
- for a local change, its desired head and literal first parent, whether a new
  immutable version is needed, and its complete desired pull request
  projection; and
- for a nonlocal change, validation evidence but no external action.

A local change has one of these valid nonterminal publication realities:

- no published history or marker, no correlated OPEN pull request, and one
  exact stable-key creation authorization;
- nonempty published history, no marker or correlated OPEN pull request, and
  one exact stable-key creation authorization;
- nonempty published history, no marker, and one validated OPEN pull request
  on its owned base, which authorizes the marker phase; or
- nonempty published history, a validated marker, and one validated OPEN pull
  request whose projection may be stale.

The second reality includes both the ordinary prefix after Git publication and
the ambiguous prefix in which an unmarked provisional pull request is absent
from the OPEN observation. A marker with no correlated OPEN pull request is not
creation authorization: complete terminal evidence retires the identity, and
terminal exhaustion fails closed. A nonlocal active change has nonempty
published history and one validated OPEN pull request. A missing marker is
valid only while that pull request remains on its owned base; it produces no
action in this attempt. Any other combination is rejected before publication.

For a valid observation, the final derived value is an immutable publication
plan. It contains every Git tuple which must move and the staged pull request
work. Creation becomes available only after the initial tuple barrier, and
final projection only after every required marker request is acknowledged.

## Publication lifecycle

A publication attempt moves only forward:

```text
resolve push destination
    -> start the global Git head advertisement and GitHub observation
       concurrently
    -> after the head advertisement establishes the default,
       derive local intent, observe local tag namespaces, and complete the
       local graph while GitHub pages continue
    -> after GitHub observation and local graph completion both finish,
       correlate identities
    -> observe tags for newly discovered nonlocal active IDs,
       acquire any additional objects, and complete terminal lookups
    -> validate and construct an immutable publication plan
    -> publish Git tuples
    -> create missing pull requests
    -> validate every create acknowledgement, authorize the already-preflighted
       missing pull request markers, and prepare a gated exact final projection
    -> publish missing pull request markers
    -> validate exact acknowledgement of every marker request and release the
       prepared final projection
    -> apply any prepared final updates
```

There is no pre-Git pull request staging, rollback phase, post-push
confirmation phase, or same-attempt re-observation. A newly created pull
request has a safe provisional body and owned base until the required marker
barrier releases its final update.

The initial boundary between Git tuples and pull request creation is an
acknowledgement barrier. A plan with no tuple work crosses it immediately.
Otherwise, only exact acknowledgement of every planned tuple request releases
pull request creation. A second Git acknowledgement barrier follows creation
or first observation: final pull request updates remain unavailable until
every required immutable marker request is exactly acknowledged. A plan with
no marker work crosses the second barrier immediately. An indeterminate Git
result at either barrier ends the attempt before the next pull request write.

Each top-level GraphQL alias names one complete create or update resolver. That
complete resolver is one pull request operation and is the indivisible effect
used throughout this document. A transmitted batch may execute any subset of
its complete aliased operations, but an individual operation does not partly
apply its requested title, body, or base fields.

## Why publication is safe and retryable

A published version records a head and that head's literal first parent
together. Before writing, GHerrit proves that no published or proposed head for
a change is reachable from any published or proposed first parent for that
change. GitHub can therefore observe any old or new pairing of the change's
head and owned base without seeing the head as merged into its base.

Every create uses the owned base covered by that proof. A final root projection
uses the stable default branch instead, so GHerrit proves the corresponding
reachability property separately for every published and proposed root head.

Git publication happens before pull request publication. Each change's head,
owned base, and new immutable version tag move in one atomic tuple. Only exact
acknowledgement of every required tuple request releases creation. Every create
uses the permanent safe owned base. Acknowledged or observed pull request
existence for a local change is then recorded by an immutable marker before any
final update is released. A final update leaves a nonroot on its owned base and
can move a root directly to its separately proved safe default base.

A crash or lost acknowledgement can leave only a safe prefix: no write, some
complete Git tuples, some provisional pull requests, some immutable pull
request markers, or some final pull request updates. Nothing needs to be rolled
back. A later invocation observes refs, tags, and pull requests again and
derives only the remaining work. With unchanged local intent and operating
assumptions, exact Git advertisements expose every durable Git effect.
Forward-only retries converge provided durable committed GitHub effects
eventually become and remain visible to later planning observations and
still-needed operations eventually receive usable acknowledgements.

## Correctness argument

The guarantee is conditional on the operating assumptions in
[Scope](#scope). Within that boundary, the following invariants are sufficient
for safety and recovery.

### Essential invariants

1. Every observation and action is bound to one push destination, one base
   repository, and one agreed stable default-branch name and object ID.
2. Every active identity has a complete tag-namespace observation. Its version
   history is absent before first publication or is a nonempty sequence whose
   mutable head and owned base both agree with the latest version. Its optional
   immutable `pr` marker points at a head in that sequence.
3. For one change, every published or proposed head is unreachable from every
   published or proposed first parent. For a change which is or will become
   root, every published or proposed head is also unreachable from the agreed
   default-branch tip.
4. A Git publication unit contains exactly one change's head, owned base, and
   new version tag. That unit is never split, and each batch is atomic and
   exactly leased.
5. Every initially observed managed pull request head is explained by validated
   published history. Its base is explained by either a validated published
   first parent or the agreed default-branch tip. Without a marker, only the
   permanent owned base is valid. A new pull request can be created only from a
   marker-free stable-key creation authorization after its Git tuple exists.
6. Every create uses head `G` and base `gherrit-bases/G`. GitHub atomically
   prevents a second OPEN pull request with the identical repository and ref
   key. A duplicate rejection creates nothing and supplies no pull request
   identity to the current attempt.
7. Before publication, every local markerless pull request has an immutable,
   absent-leased `pr` marker fully preflighted. Observation of the pull request
   authorizes that marker immediately; a planned create authorizes it only
   after an exact create receipt. Final updates remain unavailable until every
   required marker push is exactly acknowledged. Each update leaves the pull
   request on its old validated base, its owned base, or its final base.
8. Indeterminate writes stop the attempt. Plans, receipts, and prepared actions
   are one-use values; a later invocation reconstructs state from durable Git
   and GitHub observations rather than from attempt-local memory.

### Historical reachability safety

[GitHub can mark a pull request merged][indirect-merge] when its head becomes
reachable from its base. That lifecycle change is permanent for GHerrit's
purposes: GHerrit cannot safely recreate the open pull request later.

The required safety property is therefore:

> At every state GitHub can observe, the head of every active managed pull
> request is unreachable from its base.

GitHub may observe ref changes in an order different from the publishing
client's order. It may also continue to observe an older valid ref tip after a
newer tuple has been published. The safety property covers all such
observations.

For one change ID `G`, let `R_G` contain every published revision for `G` and,
for a local change, its proposed revision. Repeated entries are retained;
duplication does not affect the quantified property. For revision `r`, let
`H(r)` be its head and `P(r)` its literal first parent. Let `X <= Y` mean that
`X` is reachable from `Y`, including equality.

Every pair of published or proposed revisions must satisfy:

```text
for every r and s in R_G: not (H(r) <= P(s))
```

Suppose instead that `H(r) <= P(s)`. Since `P(s)` is the first parent of
`H(s)`:

```text
H(r) <= P(s) < H(s)
```

If `H(r)` and `H(s)` are different commits, the ancestry of `H(s)` contains
both commits carrying `gherrit-pr-id: G`, contradicting the identity rule. If
they are the same commit, the head is reachable from its own first parent,
which would require a cycle. Therefore no published or proposed head is
reachable from any published or proposed first parent for the same change.

```text
published or proposed head
    x
published or proposed owned-base tip
```

Checking only the old and new values for one push is insufficient. A delayed
observer can pair a version-one head with a version-three base. The complete
history check covers that pairing.

### Safety of every visible prefix

The initially observed state is accepted only after all active managed pull
requests, complete published tuples, and optional pull request markers satisfy
the invariants. Read-only observation and exact object acquisition cannot
change a managed ref or pull request, so the state remains safe until
publication begins.

Each acknowledged Git batch moves whole tuples atomically. GitHub may still
observe the individual refs in any order, or retain an older valid tip. For an
owned-base pull request, every such visible head/base combination is one of the
historical or proposed pairs covered by the reachability invariant. For a root
pull request, the separate default-branch check supplies the same result.
Earlier acknowledged batches and a later indeterminate batch therefore leave a
safe Git prefix.

Creation is unavailable until all required tuple requests are acknowledged.
Every create consequently names an existing head and its permanent safe owned
base. Each acknowledged marker batch adds only immutable tags whose targets
are validated published heads; it changes no pull request base or managed
branch. Final updates are unavailable until all required marker requests are
acknowledged. An update can leave a pull request on its old validated base, its
owned base, or its final safe base. Each complete aliased create or update is
one indivisible operation: it applies all of its requested pull request fields
or none of them. Title and body changes cannot affect reachability. A batch may
execute any subset of these complete operations, so every mutation-batch prefix
and every such execution subset is safe.

### Failure prefixes

| Failure point | Visible state | Reason it remains safe |
| --- | --- | --- |
| Before Git publication | No managed remote or GitHub state changed; the local ODB may contain acquired objects | Observation and acquisition change no ref, `FETCH_HEAD`, or configuration |
| Git push is not acknowledged | That batch is entirely old or entirely new | Git atomicity and the reachability invariant make both states safe |
| Earlier Git batches succeed | A coherent prefix is published | Each change tuple is coherent and all versions are safe together |
| Crash after Git publication | Git is current; PRs may be stale | Existing, owned, and final bases are safe |
| Some creates succeed | Some PRs have provisional bodies and owned bases | The create key is stable and owned-base reachability was proved |
| Create acknowledgement is lost | An unmarked provisional PR may exist but be absent from OPEN | A same-key retry is rejected without creating a duplicate |
| Marker push is not acknowledged | Each marker is absent or immutable; affected PRs remain on validated owned bases | The gated final projection has not been released |
| Earlier marker batches succeed | A coherent subset of PR existence facts is durable | Every marker target belongs to validated history |
| OPEN omits a marked PR | Terminal evidence can retire it; no create or update has an OPEN identity | The marker makes the attempt fail closed |
| Some updates succeed | Each PR has provisional, old, or final metadata | Every possible base is safe and each complete update is indivisible |
| A query field is stale | A write may fail or be redundant | Freshness is not part of the reachability proof |
| Process restart | A fresh attempt begins | Remote refs, tags, and PRs describe the remaining work |

Rollback is not part of publication. It would introduce additional
failure-prone writes and can restore older combinations which no longer belong
to the current plan. Every acknowledged effect is a safe forward step.

### Recovery and convergence

Remote refs, immutable version and pull request tags, pull request metadata,
and pull request lifecycle are the durable record of progress. An
acknowledgement is evidence that an operation completed, but it is not
additional durable state. If an acknowledgement is lost, the next attempt
observes whether none, some, or all of the transmitted effects became visible
and validates that state in the same way as any other starting state.

A lost create acknowledgement cannot be followed by a marker or final update.
The possible pull request therefore remains provisional on its permanent owned
base. If an OPEN observation omits it, a repeated create has the identical
repository, head, and base key. GitHub's atomic same-key rule rejects that
create without adding a duplicate. The rejection is indeterminate to the
current attempt: it does not reveal the existing pull request identity and
cannot release either a marker or final update. Eventual visibility exposes
the provisional pull request to a later attempt.

Exact acknowledgement of local creation, or observation of a validated local
provisional pull request, permits the absent-leased marker push. A lost marker
acknowledgement still cannot be followed by a final update, so the pull request
remains on the owned-base key. Before exact marker acknowledgement, a later
OPEN observation may omit even a previously acknowledged or observed pull
request; the same-key rule still prevents a duplicate. Once the marker is
present in validated Git state, an absent OPEN row never permits another
create. Complete terminal evidence retires the identity; otherwise GHerrit
fails closed until eventual stable visibility exposes the still-OPEN pull
request. Returned field values may independently lag at older valid states,
but validation and historical reachability keep accepted projections safe.

For convergence, hold local intent, the default branch, and the one-publisher
assumption fixed. Exact Git advertisements expose every durable Git effect.
Assume that every durable committed GitHub effect eventually becomes and
remains visible to all later observations which need it for planning, and that
every operation which remains necessary eventually receives a usable
acknowledgement. For this fixed local publication, measure durable progress by
the lexicographically ordered counts of missing Git tuples, missing pull
requests, missing pull request markers, and stale pull request projections.
The measure includes only effects owned by local changes. A missing marker on a
validation-only nonlocal change is valid but is not work this plan must reduce.

Applying a desired effect never increases that measure. If the effect is
already satisfied, a redundant operation leaves the measure unchanged. If it
is still missing, applying it strictly reduces the earliest affected class. An
indeterminate operation may have either result, but it cannot introduce
rollback work. Same-key duplicate rejection and marked-absence failure create
nothing and leave the measure unchanged. A stale observation may temporarily
plan already-satisfied work or fail closed. Eventual visibility makes each
durable satisfied effect disappear from later plans and eventually supplies
the identity hidden by a marked OPEN absence. If work remains after
observations have caught up, the plan contains a still-missing effect, and
eventual usable acknowledgement supplies a strict decrease. Each class is
finite, so repeated attempts reach a state with no remaining action.

Encoding a pull request number or node ID in the marker could support a direct
lookup after OPEN omission, but it adds marker grammar, durable identity state,
and query behavior without improving safety. The fixed existence marker already
forbids creation, terminal history handles retirement, and eventual complete
OPEN observation supplies the identity needed for progress. Relying instead on
monotonic row inclusion in GitHub's paginated OPEN connection would add a
strong undocumented backend assumption. This protocol rejects both variants.

An observation which cannot be explained by the validated histories is not a
recovery prefix. GHerrit rejects it instead of guessing whether or how to
repair it.

## Implementation contracts

The sections below specify how GHerrit obtains the evidence used above,
validates it, constructs bounded requests, and recognizes acknowledgements.
These mechanics implement the model and proof; they do not add another source
of publication state.

### Push destination

GHerrit resolves the selected remote before it reads local stack history or
performs network I/O. The configured remote name is a validated value. It must
name one remote and must produce exactly one push destination. Missing,
repeated, non-UTF-8, or otherwise malformed configuration is an error rather
than a reason to use a different remote.

The resolved push destination is the one literal destination for the entire
attempt. It determines the GitHub owner and repository and is retained in a
private value. Configured fetch destinations and remote-selection defaults do
not participate after resolution. Supported destinations include
credential-free URLs, SCP-style SSH destinations, and local paths used by
repository tests.

Every Git subprocess addresses that literal through a reserved internal
remote. Before choosing its name, GHerrit uses Git's null-delimited
configuration output to enumerate the baseline active configuration and
chooses an absent probe name from this deterministic sequence:

```text
gherrit-publication-probe
gherrit-publication-probe-1
gherrit-publication-probe-2
...
```

GHerrit then runs a second network-free configuration probe. It supplies the
exact destination once as the probe remote's URL and once as its push URL
through `--config-env`, then enumerates every active `remote.*` record. This
causes Git to evaluate the normal include graph and every
`includeIf hasconfig:remote.*.url` condition which matches the destination. An
include cycle, malformed output, or any failure to obtain a finite enumeration
rejects the attempt. The probe remote is never used for network I/O.

GHerrit chooses the first name absent from every configured remote subsection
in that active configuration, using this deterministic sequence:

```text
gherrit-publication
gherrit-publication-1
gherrit-publication-2
...
```

Remote configuration names are compared case-insensitively. Any active key in
a candidate's `remote.<candidate>.*` subsection makes that candidate occupied,
even if it does not define a URL. Because the active configuration is finite,
trying one more candidate than there are records proves that an absent name
exists.

Each final remote command then receives exactly these two command-scoped
configuration entries for the internal remote:

```text
--config-env=remote.<internal>.url=GHERRIT_PRIVATE_PUSH_DESTINATION
--config-env=remote.<internal>.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION
```

The named environment variable contains the private resolved literal. The
configuration exists only for the child process and is never written to the
repository. Destination-bearing network commands for observation, acquisition,
and publication name the internal remote rather than the destination. There
are no empty reset values.

Git resolves the configured remote's effective push destination using its
normal `remote.*.url`, `remote.*.pushurl`, `url.*.insteadOf`, and
`url.*.pushInsteadOf` rules. That resolution happens exactly once. GHerrit then
assigns the resulting literal to both the internal remote's explicit `url` and
explicit `pushurl`.

GHerrit inspects configuration again after activating the chosen internal
remote in the same command context used by network operations. The exact URL
activates the same destination-dependent includes as the probe. The internal
remote must have exactly one `url`, exactly one `pushurl`, equal to the resolved
literal, and no other key. Any empty, repeated, added, or disagreeing value
rejects the attempt. Rewrite and redirect validation run in this final context.

The internal remote assigns the same explicit value to `url` and `pushurl`. An
explicit `pushurl` prevents `pushInsteadOf` from applying to the internal
remote. An `url.*.insteadOf` rule can still rewrite either explicit value, and
it interprets their equal values identically. GHerrit therefore performs one
no-network fixed-point probe of the internal remote's read URL. Requiring that
result to equal the resolved literal also proves the push URL: the two explicit
inputs are equal, `insteadOf` treats them equally, and `pushInsteadOf` cannot
replace the explicit push URL. A further `insteadOf` rewrite rejects the
attempt. GHerrit does not reject a `pushInsteadOf` rule which Git cannot apply
to the explicit internal `pushurl`.

Every destination-bearing network command places `--` before the internal
remote where the command permits it and adds
`-c http.followRedirects=false`. A redirect response fails the attempt instead
of moving an observation, fetch, or push to another server.

For a credential-free `http` or `https` URL, GHerrit applies the command-scoped
`false` value and uses Git's exact URL-matching semantics to read the effective
`http.followRedirects` value for the literal URL. This matcher is the one
exception which places the literal destination in a Git child argument list.
The result must be exactly one `false` value.

Credentials embedded in a URI destination are unsupported and are rejected
after destination resolution, before the configuration probe or any network
request. The enclosing `git push` can trace the arguments with which it invokes
the pre-push hook, including its destination, before GHerrit starts and can
sanitize its own child environment. Rejection inside the hook cannot provide
secrecy retroactively. HTTP users must use Git credential helpers, and SSH
users can use an SCP-style destination instead of embedding credentials in a
URI.

A supported, credential-free destination has no `Display` or debug form which
reveals its raw value. GHerrit's command traces and errors identify the
selected remote without printing the destination. Before destination
resolution and every later Git probe, observation, acquisition, or push,
GHerrit removes every inherited environment variable whose name
case-insensitively matches `GIT_TRACE*`, along with `GIT_CURL_VERBOSE`. A Git
child therefore cannot persist the literal through an inherited transport
trace or curl diagnostic stream.

Destination-bearing Git commands run through one finite subprocess boundary.
Execution has a 120-second deadline. When execution stops through completion,
timeout, cancellation, monitoring failure, or reader failure, GHerrit starts a
fresh five-second interval for terminating and reaping the owned process group
or job and draining both output pipes.

GHerrit retains at most 64 MiB of exact stdout. Bytes beyond that limit are
still drained, but the command fails. Stderr is always drained and never
retained; only its saturating byte count is exposed. No child stderr content
can enter a diagnostic or reveal the destination.

### Observe Git and GitHub

The global Git observation is one remote head advertisement:

```text
git ls-remote --quiet --symref -- <internal-remote> \
    HEAD 'refs/heads/*' refs/tags/gherrit
```

The arguments are constant in size. The command obtains the remote `HEAD`, all
heads, every owned base, and the literal reserved tag-namespace root in one
network request. It does not enumerate version tags or pull request markers.
An advertised `refs/tags/gherrit` root rejects the attempt because it would
prevent the managed tags below that namespace from existing.

GHerrit parses the advertisement as bytes. Unrelated ref names need not be
UTF-8. Every record must have either the documented direct object-ID, tab,
ref-name shape or the symbolic `ref:`, target, tab, ref-name shape. Duplicate
direct or duplicate symbolic observations for a ref are rejected. Git may
advertise both forms for a symbolic ref, including an unrelated symbolic ref
whose name tail-matches `HEAD`, so that pair is valid. A symbolic `HEAD`
record, the direct `HEAD` object ID, and the advertised target head must all be
present and agree. A direct-only `HEAD`, a missing target, or disagreement is
invalid repository state.

The patterns passed to `ls-remote` use Git's tail-matching rules. The literal
`HEAD` pattern can therefore return unrelated refs whose final component is
`HEAD`; GHerrit ignores every such ref except the exact pseudoref. The
`refs/heads/*` pattern can likewise return a ref outside `refs/heads/` whose
tail resembles a head. GHerrit ignores that tail-only match. Exact head results
include nested heads, including the owned-base namespace.

The reserved head names have this exact grammar:

```text
refs/heads/gherrit-bases/<change-id>
```

A ref at the `refs/heads/gherrit-bases` namespace root, an invalid change ID,
an extra path component, or a non-UTF-8 reserved name rejects the attempt. A
symbolic `HEAD` which targets that root or anything below it also rejects the
attempt. Other heads remain unrelated evidence unless their complete top-level
name is a valid change ID. A top-level same-named branch alone does not prove
that it is managed.

The wire parser recognizes the lengths and hexadecimal syntax of SHA-1 and
SHA-256 object IDs and requires one format throughout every advertisement in
the attempt. The graph reader supports SHA-1 repositories. A SHA-256
advertisement therefore produces an explicit unsupported-format error before a
graph object ID is constructed.

GitHub observation obtains:

- the repository node ID and default branch;
- every row in a completely paginated OPEN pull request connection;
- paired pull request number and GraphQL node ID;
- head and base names and object IDs;
- whether the head belongs to the same repository;
- lifecycle state;
- title and body; and
- native auto-merge and merge-queue state.

The Git advertisement and the first GitHub request start concurrently. They
are not an atomic cross-system snapshot. A GitHub ref object ID may refer to an
older valid published version; that is safe because all valid historical
pairings satisfy the reachability invariant. An unknown value is rejected.

GHerrit establishes repository eligibility with one paginated connection of
open pull requests. Because the connection is nested beneath the base
repository, `isCrossRepository == false` establishes that a pull request's head
belongs to that same repository. The observation preserves head names, head
object IDs, body metadata, and the managed Git refs needed for later
correlation. This detects duplicate managed pull requests and an open managed
pull request whose source ref has been deleted. A same-named fork head is not
eligible.

Completeness means that GHerrit follows every returned cursor to exhaustion.
It does not mean that GitHub supplies a snapshot or includes a row which an
earlier invocation observed. The marker and terminal rules determine what an
absent row can authorize.

The first page's repository-root document also contains the repository identity
and default branch. Later pages contain only the connection fields needed to
complete the same observation. GHerrit does not perform a separate ref query or
an `associatedPullRequests` lookup for each local change.

The GitHub default branch must have a name and non-null object ID. Its name and
object ID must agree with the symbolic remote `HEAD` and its advertised target
head. Repository absence, a null target, or any disagreement rejects the
attempt.

### Derive local intent

The remote Git default branch determines the local stack boundary. The
corresponding local ref must have the same name and object ID. GHerrit derives
the ordered changes, heads, first parents, root status, titles, bodies, and
parent/child navigation inputs from the first-parent path after that default.
Remote version records later supply the authoritative patch history.

Derivation starts as soon as the Git advertisement establishes the default. It
does not wait for the open-pull-request connection to finish paginating.

A local commit body cannot contain the reserved `<!-- gherrit-meta:` prefix.
The renderer appends exactly one generated record, so accepting the prefix in
user text would make the published body ambiguous on the next observation.

Read-only Git and GitHub observations may already be in flight when local body,
identity, or topology validation fails. Such a failure cancels or ignores the
remaining observations and occurs before any managed remote or GitHub write.

### Observe active tag namespaces

As soon as local intent is available, GHerrit starts one or more
command-size-batched exact tag-namespace observations for the local change IDs.
Each batch has this shape for its assigned IDs:

```text
git ls-remote --quiet -- <internal-remote> \
    refs/tags/gherrit/G 'refs/tags/gherrit/G/*' ...
```

For every assigned `G`, the request includes both its namespace root and all
descendants. The response therefore proves the complete remote version history
and optional pull request marker for those IDs without enumerating namespaces
for other IDs.

This exact Git advertisement is complete authoritative ref evidence under the
quiescent-writer contract. It has no page cursor or cross-page omission mode.
In particular, absence of `refs/tags/gherrit/G/pr` from the successful exact
namespace response is the marker-absence fact consumed by the planner.

The patterns use `ls-remote` tail matching. The byte parser accepts evidence
only under the exact requested namespace. It ignores a ref which merely ends
in one of the requested patterns but has a different complete name. An exact
`refs/tags/gherrit/G` root is invalid repository state. The global observation
has already rejected the top-level `refs/tags/gherrit` root. Under an assigned
ID, every other returned name must have one of these exact forms:

```text
refs/tags/gherrit/<change-id>/v<positive-canonical-decimal>
refs/tags/gherrit/<change-id>/pr
```

The change ID must equal the requested ID. The fixed `pr` name may appear at
most once through Git's ordinary ref uniqueness and has no descendants. For a
version, zero, a leading zero, or numeric overflow is invalid. A non-UTF-8
reserved name, unknown leaf, or extra path component rejects the attempt.
After raw parsing, a version is represented as `Version(NonZeroU64)`, so `v0`
cannot enter validated state.

The command intentionally omits `--refs`. Git therefore emits a second `^{}`
record for an annotated tag. All managed version and pull request tags must be
lightweight, so a peeled record in an observed active namespace rejects the
attempt. History validation later requires every managed tag to point at a
commit and the marker to point at one of the version heads. Object IDs must use
the format established by the global head advertisement.

Local namespace observation begins before GitHub pagination needs to finish.
Once the tags for a local ID are known, acquisition of any missing local
objects can start. Pull request correlation can discover additional active
nonlocal IDs; GHerrit then queries only those newly discovered namespaces and
acquires any of their missing history. IDs already covered by the local wave
are not queried again.

The complete head observation retains the exact push destination which
produced it and is consumed to begin active-namespace observation. The
cumulative namespace observation accepts no destination argument. Adding the
nonlocal wave consumes the existing observation, rejects duplicate or
overlapping requested IDs, and preserves explicit empty namespace
observations.

Before normalization, the cumulative observation is consumed with the ordered
local and nonlocal ID sequences. It proves that both sequences are individually
unique, mutually disjoint, and that their union is exactly the covered history
namespace key set. It then yields opaque per-change observations in those
exact orders. No raw namespace map, subset extractor, relabelling constructor,
or independently supplied head, base, or marker value can enter history
normalization. The normalizer consumes one opaque observation and retains its
change ID through proposal coupling and validation.

### Object acquisition

Remote observation can advertise a version whose objects are not present
locally. GHerrit acquires missing history through the exact version-tag ref
names from that advertisement. It does not fetch the marker, a raw object ID,
or a ref selected through configured fetch rules. A valid marker target is
also a version target, so the version refs provide all required objects.

Object acquisition uses this request shape:

```text
git fetch --no-write-fetch-head --no-tags --no-recurse-submodules \
    --no-auto-maintenance -- <internal-remote> \
    <exact-advertised-tag-ref>...
```

The refspecs are source-only and name only version-tag refs present in the same
remote advertisement used for validation. The request does not create or
update a local ref, remote-tracking branch, tag, or `FETCH_HEAD`. It does not
recurse into submodules or run automatic repository maintenance. Because it
does not request a partial fetch, it does not establish or persist promisor
configuration.

An exact fetch can transfer reachable blobs which history validation does not
need. This is an intentional bandwidth tradeoff for simpler, durable repository
state. Missing remote history is rare in the expected workflow, so ordinary
attempts perform no acquisition at all.

Each logical graph-completion wave first loads the complete all-parent graph
rooted at every required external root and every version target accumulated so
far. The local wave's external roots are the exact default tip and local
proposal heads. The final wave reloads from the complete active root set after
nonlocal correlation.

Invalid or non-commit evidence fails immediately and never triggers
acquisition. On a missing object, GHerrit constructs an action from every exact
advertised version ref assigned to that logical wave, not from the missing
object ID. Consequently, a locally present tag tip with a missing promised
ancestor still fetches the ref which can supply that ancestor, and two tags
which point to one object retain both exact source refs.

The action executes once normally, possibly in fixed command-size batches,
followed by one graph reload. Only if evidence is still missing and the
repository already has promisor configuration does the same destination-bound
action execute once with `--refetch`, followed by one final graph load. There
is no loop, recursion, changed destination, raw-object fetch, or third
acquisition. If the wave has no advertised refs, GHerrit returns the original
missing-object error.

Local IDs are active by construction, so their tag-namespace observation and
graph-completion wave start after local derivation. Correlation can reveal
additional active nonlocal IDs; their namespace observation and the final graph
wave complete the active graph after correlation.

Exact tag-namespace queries, exact acquisition requests, and publication
pushes each permit at most 16 KiB of variable arguments, counting one separator
byte per argument. A single indivisible query unit, exact source ref, three-ref
publication tuple, or one-ref marker creation which exceeds that budget rejects
the attempt. Batching never splits one change's publication tuple or marker
creation.

### Correlate pull requests

The renderer appends exactly one standalone metadata line to the generated
footer of both provisional and final bodies:

```text
<!-- gherrit-meta: {"id":"G","parent":"P","child":null} -->
```

The compact JSON fields always appear in `id`, `parent`, `child` order. `id` is
the body's change ID. `parent` and `child` are the adjacent change IDs in the
local stack, or `null` at the corresponding end; they are not pull request
numbers. The parser distinguishes absence from an invalid claim. A repeated
marker, unterminated comment, malformed JSON, duplicate or unknown field,
wrong value type, invalid ID, self-link, or equal non-null parent and child is
invalid. Parent and child values may be stale because they are derived
projection state; only the metadata ID establishes identity.

Before repository eligibility or managed correlation is considered, GHerrit
validates the number and node ID of every row in the complete OPEN connection,
including unmanaged and cross-repository pull requests. Numbers and node IDs
must each be independently unique. The resulting initial identity registry is
retained for later create-receipt collision checks.

After its identity has entered that registry, a cross-repository pull request
is excluded from metadata parsing and managed correlation. Its body, branch
names, and object IDs cannot establish managed state. For a same-repository
pull request, valid metadata identifies its change only when the metadata ID
equals `headRefName`. Without metadata, a pull request is managed only when its
head name and reserved owned-base evidence identify the same change. A
same-named head alone is not enough.

Every valid pull request using this representation has an owned-base branch,
including a root pull request. Correlation therefore needs the global head
advertisement but not a repository-wide version-tag advertisement. Exact tag
namespace state for each correlated active ID is observed afterward.

A same-repository open pull request whose head collides with a local ID but has
neither metadata nor managed history is unsupported state. GHerrit rejects it
rather than adopting an unrelated pull request. Conflicting identity signals,
duplicate node IDs or numbers, and more than one managed open pull request for
one change also reject the attempt.

An OPEN row returned after its source ref is deleted still carries metadata,
its remembered head name and object ID, and remaining managed history. GHerrit
correlates that row before tuple validation rejects the missing ref. If a later
OPEN observation omits a previously established pull request, its durable
marker prevents GHerrit from mistaking the identity for unused.

Only local IDs without a correlated open pull request receive terminal
lookups. GHerrit batches them as aliased connections with one cursor per ID. A
same-repository closed or merged pull request retires the ID. Fork results are
ignored and pagination continues. Exhausting the connection produces terminal
evidence, not creation authority by itself. The planner combines that evidence
with OPEN absence and exact marker absence to produce the authorization for a
stable-key create. With a marker, the same evidence is ambiguous and fails
closed. The common case completes every ID in one request. Missing or repeated
cursors, an unexpected lifecycle state, or a mismatched head name is invalid
query evidence.

### Evidence validation

GHerrit validates local intent and remote observations before exposing any
managed remote or GitHub write. Exact object acquisition is preparation: it may
add unreachable objects to the local object database, but it changes no ref,
`FETCH_HEAD`, repository configuration, remote ref, or pull request.

All reachability evidence comes from Git's literal object graph. GHerrit
rejects shallow repositories and repositories with grafts, disables replace
refs in both its Git library and Git subprocesses, and disables lazy object
fetching while inspecting evidence or publishing refs. A partial clone is
supported only when GHerrit's explicit observation fetch obtains every object
needed by validation. Missing objects are errors rather than an invitation for
an incidental network request.

For each active change, validation requires:

1. The local stack satisfies the identity and topology requirements above.
   No local or nonlocal active change ID equals the agreed default branch name;
   otherwise its managed head would alias the default branch itself.
   The default branch is not the reserved `refs/heads/gherrit-bases` root or
   any ref below it.
2. The ancestry of every published and proposed head contains
   exactly one commit carrying the change's ID.
3. Existing version tags are canonical, contiguous, lightweight, immutable,
   and point at commits carrying the expected ID. The optional fixed-name
   marker is lightweight and immutable and points at one of those version
   heads. Adjacent versions cannot record the same literal revision because
   publication creates a version only when the desired revision changes. A
   later nonadjacent version may return to an older revision without collapsing
   either history position.
4. Published version heads and their first parents are locally available.
   Shallow or incomplete history is not accepted as evidence of safety.
5. The current head branch agrees with the latest version tag.
6. The current owned base agrees with the exact first parent of the current
   head branch.
7. Every published and proposed head is unreachable from every published and
   proposed first parent for that change.
8. A GitHub head object ID is one of the validated published heads for that
   change. A merely proposed revision cannot explain an initial observation.
9. A GitHub owned-base object ID is one of the validated published first
   parents for that change. A merely proposed parent cannot explain an initial
   observation.

The direct reachability check is retained even though the identity rule implies
it. It makes the operational safety condition explicit and catches mistakes in
history decoding or identity validation. It is local Git graph work, and the
implementation may cache graph traversals rather than invoke a separate Git
process for every pair.

An unexpected remote combination is invalid state. GHerrit reports it and
makes no attempt to repair it silently.

GHerrit's in-process graph reader ignores object-related environment variables
and replacement objects. Every Git subprocess explicitly disables replacement
objects and implicit promisor fetches and removes environment variables which
redirect the object database, graft file, shallow file, or replacement
namespace.

Before traversing history, GHerrit rejects every nonempty file which can rewrite
or truncate ancestry. It checks both the conventional `info/grafts` and
`shallow` files in the common Git directory and any effective alternate paths.
An installed hook also checks nonempty files named by `GIT_GRAFT_FILE` or
`GIT_SHALLOW_FILE` from the hook's execution root: changing child-process
environments cannot change the environment retained by the enclosing
`git push`. Empty and absent files have no effect and are accepted.

Implicit object fetching can be disabled reliably only by Git 2.45 and newer.
GHerrit therefore requires at least that version when any remote is marked as a
promisor or `extensions.partialClone` is configured. Partial clones remain
supported when every commit needed for validation is local. Blobs may remain
omitted because commit ancestry and GHerrit's commit metadata do not read them.

### Root validation

The owned-base proof does not apply to the default branch. For every change
which is or will become root, GHerrit separately verifies that every published
and proposed head is unreachable from the observed default-branch tip.

The remote symbolic `HEAD`, its advertised target branch, GitHub's repository
default branch, and the corresponding local branch must have the same name and
object ID before GHerrit plans a root operation. The advertised Git ref tip is
the authority for root reachability. That name cannot be the reserved
`refs/heads/gherrit-bases` root or lie below it. A pull request observed on the
default base must have that exact object ID. A desired root may instead still
be on its validated owned base before its final update.

A default branch which moves concurrently can absorb a root head through an
external merge. A client-only GitHub protocol cannot prevent or distinguish
that event, which is why default-branch stability is part of this design's
operating assumptions.

### Supported repository state

A repository is publishable only when every active change's managed refs and
open managed pull request use the representation in this document. Inactive
tag namespaces are outside an attempt unless their ID appears locally. Exact
namespace observation and terminal pull request history then validate that
state before any publication write. This protocol has no
representation-conversion path. A repository whose active managed state uses
another representation is unsupported and is rejected before publication.

For a change which has never been published, the remote head, owned base,
version tags, marker, and pull request are all absent. Every published active
change has a complete current head/base/version-tag tuple and may have one
immutable `pr` marker. A converged nonroot targets its owned base, and a
converged root targets the repository default branch. A provisional pull
request, including a desired root, remains on its owned base until the marker
barrier releases its final update. Managed identities remain associated with
the same base repository; a same-named fork branch is not the managed head.

An observed OPEN pull request without a marker is valid only on its permanent
owned base. For a local change it authorizes creation of the absent marker,
not a second pull request. For a nonlocal change it remains validation-only. A
marker remains after the pull request closes or merges. If a local marked
identity is absent from OPEN, complete terminal evidence retires it; otherwise
the attempt fails closed.

An active managed ID is either in the local stack or belongs to an open
same-repository pull request identified by GHerrit's body metadata or matching
head and owned-base refs. Metadata, head names, and refs which purport to
identify the same managed change must agree. A same-named branch without
GHerrit metadata or an owned base is not classified as managed merely because
its name resembles a change ID. The exact tag namespace, including version
history and the optional marker, is observed after the active identity is
known.

An active nonlocal change participates in repository and reachability
validation, but a publication attempt never produces any action for it. A
validated markerless nonlocal provisional pull request remains on its owned
base and is not marked opportunistically. Only IDs from the local stack can
produce tuple updates, marker creation, pull request creation, or pull request
metadata updates.

An active change with an incomplete or conflicting managed representation is
rejected before publication. GHerrit does not mix representations within one
publication attempt.

An active change ID cannot equal the agreed default branch name. Because a
change ID cannot contain a slash, this collision can occur only for a
top-level default branch. `refs/heads/G` would otherwise be the default branch,
and publishing a managed head could force-update the branch which roots the
stack.

The default branch cannot be `refs/heads/gherrit-bases` or any ref below that
namespace. Those names are reserved for change-owned base branches and cannot
also define the repository root.

A closed or merged same-repository pull request whose head name is `G`
permanently retires `G`. A same-named fork pull request does not retire the ID.
GHerrit does not create another pull request for an identity which terminal
observation shows has already been used in the base repository.

### Normalize and plan

Raw Git and GraphQL values may be malformed, incomplete, stale, or
contradictory. Validation converts them into domain values or rejects them.
Only validated values enter the planner.

Local reserved-marker failures and local or nonlocal identity failures are
part of this read-only validation. Earlier observations may already have run,
but the planner emits no external action until all such validation succeeds.

The planner enforces the active set and the realities described in
[State of one attempt](#state-of-one-attempt). In particular, no published
history plus an open pull request is invalid because published-only evidence
cannot explain the initial GitHub head. Nonlocal active changes establish
repository eligibility but produce no action.

The publication plan contains every decision which can be made before a
managed remote or GitHub write. Its post-tuple path is exactly one of: a final
projection; marker publication authorized by observed pull request existence
and coupled to a final projection; or nonempty prepared creates coupled to a
projection seed. The seed retains complete marker wire preflights, their
one-use existence evidence, and everything locally knowable about the final
projection. It never contains an optional pull request identity.

The mutation adapter exposes `CompleteCreateReceipts` only after every planned
create has an exact acknowledgement. A malformed, incomplete, or ambiguous
acknowledgement is indeterminate and never reaches the pure transition. In the
create case, the projection seed consumes the complete receipt set. It either
authorizes the exact marker preflight retained since planning and constructs a
nonempty exact final-update set, or deterministically rejects the complete set
because a response-dependent update exceeds a supported limit. The updates
remain inside a one-use final-projection gate. Without creates, marker
publication is already authorized by observed pull request existence and
carries a gate containing either `NoAction` or nonempty prepared updates,
according to the ordinary minimal projection comparison. Only exact
acknowledgement of every marker request consumes either gate and exposes its
final outcome. No transition mutates a partially filled plan or exposes a
subset of final updates.

The planner consumes the complete creation-authorization set against exactly
the missing local IDs in stack order. A missing, duplicated, extra, unconsumed,
or subsequently omitted authorization rejects the complete plan before an
action escapes.

An open managed pull request's observed base is validated as either the default
or the owned base. The default base must have the agreed object ID. The owned
base must name `gherrit-bases/G`, and its object ID must be a validated
published first parent for `G`. Any other base is invalid. A markerless pull
request must use the owned base. A desired root may use that base as a safe
provisional prefix until its final update. A pull request on the owned base
cannot have native auto-merge enabled or belong to a merge queue. A pull
request on the default base with either feature enabled cannot be moved to the
owned base.

For each local change which needs a new Git version, the publication plan
contains one indivisible tuple:

```text
head branch:       absent or expected H -> desired H_G
owned base branch: absent or expected P -> desired P_G
version tag:       absent               -> desired H_G
```

If the current head already equals the desired head, the owned base must
already equal its first parent and the latest tag must already record that
head. A mismatched base is invalid state, not an independent repair action.

For each local change, pull request state is one validated open pull request or
a marker-aware planner result combining OPEN absence, exhaustive terminal
evidence, and exact marker state. Terminal evidence which finds a historical
pull request retires the identity. Terminal exhaustion plus marker absence
permits stable-key creation; terminal exhaustion plus marker presence produces
only a fail-closed result. Git tuple state is independent: a complete tuple
with neither a pull request nor a marker is a normal crash-recovery prefix.

Public-stack navigation treats the attached branch as data in two different
grammars. Its displayed label backslash-escapes every ASCII Markdown
punctuation character. Its GitHub tree path preserves UTF-8 bytes only when
they are RFC 3986 unreserved data or `/`, and percent-encodes every other byte.
Both forms derive independently from the same raw branch name, so every valid
UTF-8 Git branch remains linkable without creating mismatched representations.

GHerrit supports nonempty pull request titles containing at most 256 Unicode
scalar values and generated bodies containing at most 131,072 UTF-8 bytes.
These are explicit product content limits. Independently, one GraphQL mutation
request may contain at most 64 aliased operations and its exact serialized
GraphQL-over-JSON body may contain at most 1 MiB (1,048,576 bytes). These are
the mutation request limits. The planner rejects locally determined content or
requests outside these limits before publishing Git state.

Before any managed remote or GitHub write, the planner renders every
provisional body and constructs a final-body recipe whose only unresolved
inputs are pull request numbers which GitHub has not assigned yet. A final body
which depends on a missing pull request's number is bounded using the widest
decimal representation, `2147483647`, of `2_147_483_647`, the largest positive
value permitted by GitHub's GraphQL `Int` type. Every unresolved identity may
deliberately receive that same representative number. The renderer therefore
identifies the current navigation row by its ordered stack index or change
identity, never by number equality.

The recipe fixes its patch-history layout: it selects the full layout if the
widest-number rendering fits, otherwise selects the sparse layout if that fits,
and otherwise rejects the attempt. The actual body uses that same selected
layout after numbers are assigned. Freezing this choice is necessary because
allowing a shorter assigned number to switch layouts could produce a larger
body than the widest-number rendering. Rendering uses a bounded writer and
stops once the body is known to exceed the limit.

The body layer retains the selected layout and each concrete bounded
widest-number rendering needed by mutation preflight. This makes the
conservative operation an exact serialized operation over retained bytes,
rather than a later reconstruction which could choose a different layout or
misidentify a row whose representative number is duplicated. Representative
renders are preflight-only; a published final body uses the actual assigned
numbers with the selected layout.

The planner uses the production GraphQL and JSON serializers for all request
preflight. A create is serialized exactly. An update whose node ID and final
body are already known is also serialized exactly. When an update has a known
node ID but its body depends on a number which GitHub has not assigned, the
planner instead serializes a conservative representative produced from the
widest-number final-body recipe. That representative includes the body
field whenever a pending number could change it. Each exact operation or
conservative representative must fit in an otherwise empty mutation batch
before any prepared Git or GitHub action escapes the planner.

A missing pull request's future GraphQL node ID cannot be bounded before
creation: GitHub assigns it, the schema treats it as opaque, and the later
update request must contain it. The pre-publication plan therefore proves every
locally determined part of that future update, including the widest possible
pull request numbers, but does not claim to bound an undocumented node-ID
length. After all creates are acknowledged, the projection seed validates the
returned identities, serializes each exact final update, and requires it to fit
the product content limits and mutation request limits before retaining it
behind the marker gate. A failed transition exposes no update write. Some
create batches may already have run before all identities are available;
malformed receipts which can be recognized immediately still stop execution
between create batches. A later invocation begins with fresh observation.
Using REST updates, removing numbered final navigation, or assuming an
undocumented node-ID maximum would avoid this deferred check only at a greater
simplicity, request-count, or robustness cost.

In the create variant, the prepared-create value contains complete
specifications for every missing local pull request, each derived from its
marker-free stable-key creation authorization. Its paired projection seed
retains the known existing identities, the complete preflighted marker batches
behind one-use evidence, and the validated information needed to construct a
gated nonempty final-update set after every create identity is known. Neither
value exposes a general-purpose subset or lookup transition.

### Publish Git tuples

GHerrit publishes ref tuples with [`git push --atomic`][git-push].

The push adapter uses this command policy in addition to the private internal
remote configuration:

```text
git -c http.followRedirects=false -c push.pushOption= push \
    --porcelain --atomic --no-verify --no-follow-tags \
    --recurse-submodules=no --no-signed --no-force-if-includes \
    <exact-lease-options> -- <internal-remote> <explicit-refspecs>
```

`--no-verify` prevents GHerrit's own publication push from recursively invoking
the installed pre-push hook. The remaining negative options prevent user
configuration from adding tags, submodule work, signing, force-inclusion
requirements, or server push options. Every refspec and lease names one member
of the current planned tuple or marker phase; no configured default refspec
participates.

Both mutable branches use exact lease protection. A missing mutable branch uses
an absence expectation. If a requested destination already contains the exact
desired object when Git processes the push, Git may report it as up to date; an
exact up-to-date receipt is equivalent to acknowledgement for that destination.
This defensive case does not make concurrent publishing supported. Any
different object fails its exact lease, and the one-publisher assumption
remains part of the guarantee boundary. Version tags are create-only and never
force-updated, with the same already-desired behavior.

One Git push contains all tuples when the fixed 16 KiB variable-argument budget
permits it. When a stack requires multiple pushes:

- a change's head, owned base, and version tag never appear in separate
  batches;
- every batch is atomic and exactly leased; and
- every acknowledged batch prefix is safe.

GitHub need not observe the branches atomically. The historical reachability
invariant makes every old or current combination safe.

Once a prepared push request is handed to the executor, it has only two
outcomes: acknowledged or indeterminate. There is no third `Result` channel
which appears to prove that no remote write occurred. A post-push remote query
cannot add a serialization guarantee under this design's assumptions. A lost
acknowledgement ends the attempt; a later invocation observes whether the
atomic batch landed.

Acknowledgement requires a normal zero-status exit and exactly one recognized
porcelain status for every planned ref. Each status must name the exact source
and destination and satisfy the planned transition. Startup or monitoring
failure, timeout, cancellation, cleanup failure, signal, nonzero exit, stdout
overflow, malformed framing, an extra, duplicate, or missing status, a
contradictory status, or any transport or remote failure is indeterminate.
Stderr content is not retained and is never acknowledgement evidence.

### Create missing pull requests

GHerrit creates a missing pull request only after its head and permanent owned
base exist. Every create uses this stable ref-name pair, regardless of desired
root status:

```text
headRefName = G
baseRefName = gherrit-bases/G
```

Together with the unchanged base and head repositories, that pair is identical
across amendments, rebases, reorders, moves between stacks, and root-status
changes.

Several new pull requests may require their assigned numbers to render complete
parent/child navigation. GHerrit creates them with a deterministic provisional
body containing the commit text and complete reserved metadata, but no
number-based navigation. A root also retains the owned base. The metadata lets
a fresh attempt correlate a create whose acknowledgement was lost, while the
stable creation key makes a repeated create safe if OPEN observation omits it.

Every create receipt couples the requested head with the new pull request
number and GraphQL node ID. A validated number is in
`1..=2_147_483_647`; zero and values outside GitHub's positive GraphQL `Int`
range are rejected. After every create is acknowledged, the projection seed
consumes the complete receipt set and constructs the numbered local stack
internally. It authorizes the exact absent-marker preflight retained since
planning and constructs a nonempty set of exact updates for the whole stack,
retained behind the marker barrier. Every created pull request is in that set
because its provisional body omits numbered navigation. Response aliases are
batch-local and may repeat across batches, but each batch must return exactly
its own alias set. Across all batches, the requested head and
change-ID-derived `clientMutationId` must match the planned creates one-to-one.
Returned numbers and node IDs must be independently unique and independently
disjoint from every identity in the complete initial OPEN observation,
including fork and unmanaged rows. The transition returns one authorized
marker stage carrying one gated nonempty prepared-update value, or fails
without exposing either stage.

An indeterminate create acknowledgement ends the attempt before any marker or
final update. On a fresh attempt, OPEN observation may correlate the
provisional pull request. If it omits the pull request, terminal exhaustion and
an absent marker permit a repeated create with the same key. GitHub must reject
that request without creating a duplicate. The rejection remains
indeterminate: it cannot supply the existing identity or authorize a
same-attempt marker or update.

### Publish pull request markers

Before the first write, GHerrit fully preflights one immutable lightweight
marker with an absence lease for each absent `refs/tags/gherrit/G/pr`. It
points at the latest validated published head after the initial tuple barrier
for `G`; later validation accepts any head in that complete published history.
Observation of a validated local same-repository OPEN pull request on its owned
base supplies immediate one-use authority to execute that preflight. A planned
create supplies only pending evidence; its exact receipt converts the pending
evidence into execution authority.

The fixed marker name is only an existence fact. It contains neither the pull
request number nor the GraphQL node ID. The marker therefore has no additional
numeric or opaque-identity grammar and never needs to move after an amendment,
reorder, or root-status change.

Marker creations use the same finite, destination-bound atomic-push adapter
and porcelain acknowledgement rules as tuple publication. They may be batched
across identities within the 16 KiB variable-argument budget. Each marker is a
create-only one-ref unit with an exact absence lease. Marker batches run only
after every initial tuple request is acknowledged and after every planned
create has an exact receipt.

Every required marker request must receive exact acknowledgement before the
one-use gate exposes the final projection: either `NoAction` or nonempty
`PreparedUpdates`. An indeterminate marker result ends the attempt. Whether
none, some, or all marker batches landed is safe: the pull requests remain on
their owned bases because the final-projection barrier was not crossed. A
later exact namespace observation sees the immutable fact; an absent OPEN row
then causes terminal retirement or a fail-closed result, never another
authorized create.

### Apply final pull request state

For every local pull request, GHerrit derives the complete desired state:

- title;
- body;
- patch-history table;
- complete ordered stack navigation, including the pull request's own number;
  and
- root or non-root base name.

It compares that state with the observed state and sends only necessary
updates. Body comparison treats CRLF and LF spellings of each line ending as
equal. It preserves every other byte: leading or trailing whitespace, blank
lines, lone carriage returns, and the presence of a terminal line ending are
part of the desired projection. Any such difference causes a body update.

A provisional create omits all number-based navigation, including its own
number, so every newly created pull request receives a nonempty update with the
final numbered body after its marker is acknowledged. That same complete
update moves a newly created root to the default branch; a nonroot remains on
its owned base. Every other update action is also nonempty by construction. A
locally observed markerless pull request which already equals its desired
projection instead carries `NoAction` behind the marker gate. Nonlocal managed
pull requests are validated but never projected by this attempt. There is no
temporary Git ref: `gherrit-bases/G` is the permanent owned base even while it
is a provisional pull request base. The marker phase adds no GraphQL mutation:
every create already requires its final numbered-body update, and ordinary
comparison never emits a redundant update for an observed pull request.

Each patch-history row compares that version against its own immutable
first-parent object ID:

```text
<version-first-parent-oid>...gherrit/G/vN
```

It does not use the mutable current owned-base branch. A later rebase therefore
does not change the meaning of an older row's Base link. The version tag keeps
the head and its parent reachable, so a separate base tag is unnecessary.

During root creation or a later root-status transition, either observable base
is safe. The owned base is safe by the complete historical reachability
invariant, and the default branch is safe by root validation. A partial update
batch therefore leaves a pull request on its provisional, old, or final safe
base.

## Acknowledgement and retry rules

Queries, Git pushes, and GraphQL mutations have different semantics. They do
not share a generic retry mechanism.

### Queries

Queries are read-only. GHerrit may retry transient transport failures, back off
from deterministic resource limits, and split a query before retrying it.
These retries are explicit query behavior rather than transparent HTTP-client
retries shared with writes.

Each HTTP attempt has a 10-second connect timeout, 30-second read and write
timeouts, and a 60-second wall-clock timeout covering the request and complete
response body. The GraphQL transport follows no redirects.

One concrete read-only request shape receives one initial attempt and at most
three transient retries after 100, 200, and 400 milliseconds. HTTP 429, HTTP
5xx, transport or response-body read failures, and attempt timeouts are
transient. Other HTTP statuses, malformed success JSON, mixed or partial
GraphQL errors, and invalid decoded evidence are fatal.

The generated UTF-8 GraphQL document, before the outer JSON envelope, may
contain at most 256 KiB. A successful query response may contain at most 64
MiB. Either local query-size overflow or query-response overflow is
deterministic planning feedback: GHerrit reduces the page or alias batch
without spending transient retry budget or advancing the input cursor. A
GraphQL resource response is reducible only when data is absent or null and
every nonempty error is a recognized resource-limit error. Mixed errors or
partial data are fatal. Recognized resource errors have type
`RESOURCE_LIMITS_EXCEEDED` or `MAX_NODE_LIMIT_EXCEEDED`, or the exact message
`A query attribute must be specified and must be a string.` An OPEN query
halves its page size. A terminal query first halves its alias count and, once
one alias remains, halves that connection's page size. Every reduced request
keeps the same input cursor. If the minimum page and alias sizes still exceed a
bound, the observation fails.

An HTTP error body is read up to 64 KiB. Any retained service detail is
normalized to one terminal-safe ASCII line of at most 256 bytes.

A retried query remains part of the initial observation phase. GHerrit does not
query again after publication writes in order to decide how to continue the
same plan.

### Git pushes

An atomic Git push has two possible outcomes:

- acknowledged success;
- indeterminate acknowledgement, in which the whole batch may or may not have
  changed.

GHerrit stops on every indeterminate outcome. Both possible remote states are
safe, and a subsequent invocation starts by observing the resulting state.

### GraphQL mutations

GHerrit sizes mutation batches before transmission by both limits: at most 64
aliases and at most 1 MiB (1,048,576 bytes) in the exact serialized request
body. It uses the same GraphQL and outer JSON serializer for preflight and
transmission, including both layers of escaping. Operations whose exact values
were known during initial planning, and conservative widest-number
representatives for operations with unresolved numbers, are proven
individually before Git publication. Exact updates for newly created pull
requests are proven during the projection-seed transition because their
opaque node IDs did not previously exist.

The HTTP transport neither retries mutation requests nor follows redirects
which preserve and resend their POST bodies. Every mutation batch is
transmitted at most once per publication attempt. It uses the same finite
connection, read, write, and wall-clock deadlines as a query. A successful
mutation response may contain at most 4 MiB; crossing that bound after a send
is an indeterminate acknowledgement.

A mutation batch is acknowledged only when its response contains exactly the
expected alias set, with neither missing nor extra aliases, and every expected
`clientMutationId`. `errors: []` is accepted; any nonempty error set after
transmission is indeterminate. An update must return the exact preplanned pull
request identity. A create must return one syntactically valid new pull request
identity coupled to the requested head; the complete receipt transition
subsequently checks its global uniqueness and correspondence with the planned
creates. Those returned fields are receipts for the acknowledged mutations.

A timeout, transport failure, malformed response, GraphQL error, missing alias,
null result, or mismatched receipt after transmission is indeterminate. Some
complete aliased mutation operations may have executed. One alias names one
indivisible create or update resolver: a batch may execute any subset of those
complete operations, but one operation does not partly apply its requested
title, body, or base fields. GHerrit stops without replaying, rechunking,
rolling back, or continuing the plan.

A same-key duplicate rejection is one allowed post-send GraphQL outcome. The
backend must create no second pull request, but the response remains
indeterminate to this attempt: it does not prove whether this request or an
earlier request created the existing pull request, does not supply a validated
identity, and cannot authorize a same-attempt marker or final update.

`clientMutationId` correlates a response with a request. It is not an
idempotency key.

The complete rule is:

> An indeterminate external write ends the attempt. A later invocation begins
> with fresh observation and pure replanning.

## Pull request lifecycle

Managed IDs are permanent identities. A historical same-repository pull request
whose head uses an ID prevents later reuse of that ID. A same-named fork pull
request does not retire it.

A non-root pull request targets its permanent owned base and is not landable on
the default branch. A newly created root uses the same safe base until its
marker is acknowledged and its final update moves it. GHerrit rejects a pull
request on an owned base with native auto-merge enabled or enrolled in a merge
queue. Repository policy should reserve and protect the
`gherrit-bases/**` namespace.

Only a root pull request whose Git tuple and marker exist and whose final
desired metadata has been observed or acknowledged is intended to be merged
through GitHub.

GHerrit has no GitHub Action for post-merge stack maintenance. After a root pull
request lands, the user rebases the remaining commits and publishes them
through the normal pre-push hook.

## Code model

The implementation distinguishes raw observations, validated domain values,
and immutable external actions.

Raw Git refs, tag names, GraphQL values, and JSON fields may contain arbitrary
strings, absent values, and contradictions. Constructors validate those values
before the planner receives them.

The core model has these properties:

- a change ID is a nonempty ASCII alphanumeric string;
- Git publication history is keyed by change ID and is either absent or a
  nonempty ordered sequence of published versions;
- a published history carries at most one immutable fixed-name pull request
  marker whose target is one of its version heads;
- a validated version number is `Version(NonZeroU64)`, so zero is
  unrepresentable after raw tag parsing;
- each published version stores only its head and first parent; its version
  number and immutable tag ref are derived from its one-based position and the
  history's change ID;
- the mutable current head and base are derived from the final published
  version rather than stored as independent history fields;
- nonadjacent repeated heads remain distinct entries in the ordered version
  sequence, while an adjacent duplicate is invalid;
- a pull request identity couples a positive GraphQL-`Int`-bounded number and
  a nonempty GraphQL node ID;
- an ordinary projected pull request is open by construction;
- a pull request base is either the default branch or the owned base of the
  same change;
- a missing pull request can be created only with an opaque stable-key
  authorization synthesized from OPEN absence, exhaustive terminal evidence,
  and exact marker absence;
- a markerless observed pull request is valid only on its owned base, and only
  a local one can be converted into an absent-leased marker action;
- nonlocal active changes cannot be converted into any external action;
- the destination-bound head observation is consumed into cumulative local and
  nonlocal history evidence whose exact union is consumed into ordered opaque
  per-change observations;
- raw history maps, subset extraction, and independently supplied identity,
  ref, or marker values cannot enter history normalization;
- every marker's refspec, absence lease, receipt expectation, batch boundary,
  and byte budget is preflighted before the first write, but a marker for a
  newly created pull request cannot become executable until exact complete
  create receipts are consumed;
- a marker action is coupled to a one-use gated final projection of `NoAction |
  nonempty PreparedUpdates`, which is exposed only when exact acknowledgement
  of every required marker request consumes the gate;
- a publication plan privately contains exactly one of three post-tuple paths:
  a final projection, observed marker publication followed by a final
  projection, or pull request creation followed by receipt-authorized marker
  publication and a final projection;
- the planner is the sole constructor of executable tuple, marker, create, and
  update stages; wire adapters can serialize plan-owned specifications but
  cannot forge a lifecycle stage from raw IDs or strings;
- final pull request bodies require one numbered identity per local change,
  contain the complete ordered local stack, and identify the current row by
  stack position or change identity rather than number equality;
- stack order derives parent and child relationships;
- every concrete create, marker, or update action is nonempty; and
- the publication plan, projection seed, exact create receipts, authorized
  marker stage, final-projection gate, and prepared actions are immutable and
  one-use.

A late recovery path does not receive arbitrary raw state and decide whether to
repair it.

`pre_push/mod.rs` is a short orchestration module. Behavior-oriented
submodules contain local intent, Git publication history and planning, pull
request state derivation, and GitHub protocol handling.

## Performance

Network round trips and GitHub backend work dominate local graph traversal. The
protocol minimizes those expensive operations without depending on a second
observation for correctness.

The counts below cover network operations for a nonempty local stack.
Network-free configuration probes and local graph reads do not contribute to
the Git-read column. If local derivation produces no changes, GHerrit performs
no active-namespace observation, terminal lookup, Git publication, or GraphQL
mutation and does not wait for any no-longer-needed GitHub observation.

Let:

- `V` be the command-size-batched exact active-tag-namespace advertisement
  requests;
- `O` be the number of pages in the repository-wide open-pull-request
  connection;
- `T` be the batched terminal-query requests for local IDs without open pull
  requests;
- `F` be exact object-acquisition fetches, normally zero;
- `P` be initial atomic Git tuple-push batches;
- `M` be atomic pull request marker-push batches;
- `C` be create-mutation batches; and
- `U` be final-update batches.

Every attempt has one global head advertisement. A nonempty local stack needs
at least one exact active-tag-namespace request, so `V` is normally one when
every active change is local. Active nonlocal IDs discovered during correlation
or a command-size limit can increase `V`. `O` is at least one. `T` is zero for
an established stack and normally one for any number of new local IDs. Query
resource backoff, pagination, command-size batching, and transient read retries
can increase the corresponding count.

| Operation | Git reads | Git writes | GraphQL reads | GraphQL writes |
| --- | ---: | ---: | ---: | ---: |
| Established no-op | `1 + V + F` | 0 | `O` | 0 |
| Established amend or reorder | `1 + V + F` | `P` | `O` | `U` |
| First-observed provisional PRs | `1 + V + F` | `M` | `O` | `U` |
| New local pull requests | `1 + V + F` | `P + M` | `O + T` | `C + U` |
| Restart after ambiguity | Fresh normal attempt | As needed | Fresh normal attempt | As needed |

In the common single-page, single-batch case, an existing amend uses one Git
head read, one Git active-namespace read, one GraphQL read, one Git push, and
at most one GraphQL update. A new stack has the same two Git reads and adds one
terminal-query request, one create request, one marker push, and one
final-update request.

The fixed marker adds no query: the existing exact namespace request already
returns `refs/tags/gherrit/G/pr`. It adds no GraphQL mutation: every create
already requires a final numbered-body update, and an observed pull request
retains the ordinary minimal projection comparison. It adds at most one
immutable marker ref per identity and one marker phase when at least one local
pull request is first acknowledged or observed. That phase is normally one
batched Git round trip; the 16 KiB argument budget can split it into `M` atomic
batches.

Git's global head advertisement performs work proportional to the repository's
heads, and its response contains all of them. Exact active-tag-namespace
requests return only the namespace roots, version tags, and optional marker for
active IDs. Common established attempts therefore use two Git reads, while
response size and backend ref work scale with all heads plus the published
histories and one optional marker per active change rather than the
repository's complete GHerrit history.

GHerrit records elapsed time, response bytes, ref counts, requested-ID counts,
and batch counts separately for the head advertisement and each local or
nonlocal namespace wave. It also records object-acquisition request counts and
elapsed time. These trace-level measurements make round-trip, command-size, and
history-size bottlenecks distinguishable without logging destinations or
change IDs.

The global head advertisement and repository-wide OPEN observation begin
concurrently. After the head advertisement completes, GHerrit derives local
intent, observes local version namespaces, and completes the local graph wave
while OPEN pagination continues. Correlation begins after both the complete
OPEN observation and the local destination observation and graph work are
available. This direct join avoids tasks and mutable coordination; its tradeoff
is that unusual local acquisition can delay correlation when it outlasts OPEN
pagination.

Correlation determines newly active nonlocal IDs and missing local pull
requests. Nonlocal history and graph completion and terminal lookups then run
concurrently. If there are no newly active nonlocal IDs, the already complete
local graph can be reused.

```text
global heads -> local derivation -> local namespace batches -> local graph -\
                                                                         -> correlate
repository-wide OPEN pages ----------------------------------------------/

correlate -> nonlocal namespace batches -> complete active graph \
correlate -> terminal lookup pages -----------------------------> validate and plan
```

The representation adds one mutable base branch and at most one immutable pull
request marker per active change. Immutable version history remains in tags
rather than accumulating one base branch per patch version.

## Testing obligations

Tests establish the safety proof and the correctness of each external boundary.

### Pure graph and planning tests

Pure tests enumerate bounded histories containing amends, rebases, stack
reorders, moves between stacks, reused Git objects, duplicate IDs in ancestry,
multiple historical versions, nonadjacent repeated version object IDs, and
root or non-root transitions. Body tests prove that each historical Base link
retains that version's first-parent object ID after later rebases.

Publication-history construction tests prove that absent and nonempty published
history are the only domain states, that version numbers and tag refs derive
from sequence positions, that the current tuple derives from the last entry,
that adjacent duplicate revisions are rejected, and that nonadjacent repeated
heads do not collapse entries. They also prove that the optional fixed `pr`
marker is not a version, contains no pull request identity, and targets any
validated published head without changing the current tuple. Raw parser tests
reject `v0`, while domain construction tests show that every normalized
version is a `Version(NonZeroU64)`.

Local-intent tests reject a commit body containing the reserved metadata prefix
after observations may have started but before any managed remote or GitHub
write. Identity tests enforce the same ordering while rejecting a local or
nonlocal active change ID equal to the default branch name. Default-branch
tests reject both the `refs/heads/gherrit-bases` root and descendants before
publication. Rendering tests accept nonempty titles and provisional and
worst-case final bodies at their exact supported limits and reject empty titles
and the next larger value before emitting an external action. They distinguish
Unicode scalar counts from UTF-8 byte and grapheme counts. They also prove that
the history layout chosen with widest pending numbers remains fixed for the
actual render; that all unresolved rows may share the widest representative
number while the current row is still selected by index or change identity;
that each retained bounded render is exactly the one serialized for preflight;
and that an oversized history is rejected with bounded work.

For every valid generated history, tests assert:

```text
for each change G:
    for every r and s in R_G:
        H(r) is not reachable from P(s)
```

For every invalid history, planning fails before any external action is
emitted.

A test-only semantic world models commit reachability, managed refs, immutable
version and pull request tags, GitHub's atomic same-key OPEN uniqueness, and
pull request lifecycle. Whenever an open pull request head becomes reachable
from its base, the model permanently marks that pull request merged.

Tests enumerate all meaningful ref visibility orders and acknowledged Git batch
prefixes. Initial tuple effects, create effects, marker-push effects and
receipts, and final-update effects are enumerated independently. Across GraphQL
batches the tests enumerate acknowledged batch prefixes; within each
transmitted batch they enumerate every possible subset of complete aliased
create or update operations, including holey subsets. The model never
constructs a partial title, body, or base effect within one operation. Tests
restart from every resulting durable state and construct exactly the local and
nonlocal realities described above. Active nonlocal pull requests, including
markerless provisional ones, produce no external action.

Recovery schedules lose create acknowledgements and marker acknowledgements,
omit an unmarked provisional pull request before or after its first
acknowledgement or observation, and omit an OPEN row after its marker. A
repeated create always retains head `G` and base `gherrit-bases/G`, including
across root/nonroot intent changes, and same-key rejection creates no duplicate
or same-attempt identity. A present marker with OPEN absence authorizes only
terminal retirement or a fail-closed result.
Eventual stable visibility and eventual usable acknowledgements must converge
every valid schedule. Stale field values are modeled separately from row
presence and may expose any older validated projection.

Planner tests also cover a locally observed markerless nonroot whose title,
body, and owned base already equal the desired projection. Its marker is
prepared, exact marker acknowledgement releases `NoAction`, and no redundant
GraphQL mutation is constructible. Create paths still release nonempty updates
because every provisional create lacks numbered navigation.

A focused body-comparison truth table proves the sole CRLF-to-LF equivalence in
both directions and preserves every other byte distinction, including outer
spaces and tabs, blank lines, lone carriage returns, and terminal-newline
presence. These text classes remain outside the semantic recovery oracle: the
oracle models final, provisional, and stale bodies while the focused test owns
exact body equality.

Receipt tests prove that only an exact complete create-receipt set can be
consumed by a projection seed to authorize the exact retained marker preflight
and construct a gated nonempty final-update set, and that only exact
acknowledgement of every required marker request exposes those updates. No
optional identity state or partial prepared action is constructible between
those stages. They cover zero and out-of-range numbers, empty node IDs, node
IDs whose exact updates exceed the mutation request limit, independent number
and node-ID collisions within and across batches and with every initial OPEN
identity, exact post-receipt mutation-request limits, and missing, duplicate,
extra, or indeterminate marker acknowledgements.

The semantic world begins only from validated states and applies typed logical
actions. Malformed, incomplete, and contradictory raw ref or GraphQL values
belong in validation and protocol tests; they are not generated as if they were
reachable semantic states.

The semantic world does not parse GraphQL or emulate Git commands. It proves
the product property. Protocol tests prove adapters send and decode the correct
wire operations.

### Git boundary tests

Git adapter tests use temporary repositories and a real bare remote. They
cover:

- atomic tuple publication;
- exact leases for heads and owned bases;
- absent-or-already-desired handling for new refs;
- create-only version tags and absent-leased immutable `pr` markers;
- exact 16 KiB variable-argument batching for history queries, acquisition,
  tuple pushes, and marker pushes, including indivisible-unit overflow;
- controlled server rejection preserving every relevant ref while the adapter
  still classifies the result as indeterminate;
- successful earlier batches followed by rejection;
- root and owned-base ref spelling;
- one byte-oriented global head advertisement with exact patterns and constant
  command-line size;
- large, arbitrarily ordered head advertisements;
- immediate local-ID namespace observation, post-correlation observation only
  for newly discovered nonlocal IDs, and command-size batching of both waves;
- exact active tag-namespace parsing, including ignored tail-only matches,
  a globally rejected tag root, rejected per-ID namespace roots, optional
  fixed-name `pr` markers, rejected unknown leaves and marker descendants, and
  no repository-wide history request or extra marker query;
- complete authoritative marker presence or absence from one successful exact
  Git advertisement under a quiescent writer, with no pagination model;
- symbolic and direct `HEAD` agreement with the advertised target branch;
- rejection of a default branch at or below the owned-base namespace;
- unrelated non-UTF-8 refs and malformed observed reserved namespaces;
- SHA-1, explicit SHA-256 rejection, mixed formats across advertisements, and
  annotated version or marker tags;
- rejection of adjacent duplicate literal revisions and retention of
  nonadjacent repeated tag object IDs without loss of version records;
- marker targets at every validated published head, and rejection of a marker
  without history or at any other object;
- fetch and push destinations which resolve to different repositories;
- URL, scp-like, and local-path push destinations;
- zero or multiple push destinations rejected before publication writes;
- remote-name collisions and chained or divergent URL rewrites;
- configured-remote `pushInsteadOf` resolution followed by an explicit internal
  `pushurl` to which `pushInsteadOf` no longer applies;
- the network-free configuration probe activating destination-dependent
  includes, enumerating every active remote, and selecting an absent
  deterministic name even when probe or candidate names collide;
- the final internal remote containing exactly one private `url`, exactly one
  private `pushurl`, no empty values, and no other key;
- matching URL-scoped HTTP redirect configuration rejected, unrelated scoped
  configuration ignored for credential-free destinations, and redirects
  rejected;
- credential-bearing URI destinations rejected because the enclosing Git
  process can trace hook arguments;
- inherited case-insensitive `GIT_TRACE*` and `GIT_CURL_VERBOSE` variables
  unable to make GHerrit children persist a supported literal, and no supported
  literal present in GHerrit diagnostics or child arguments except the
  credential-free redirect matcher;
- shallow repositories and grafts rejected as incomplete evidence;
- replacement refs ignored by both library and subprocess traversal;
- unfiltered exact advertised-ref object acquisition which writes no ref,
  `FETCH_HEAD`, or promisor configuration;
- graph completion which loads before acquisition, fetches every exact ref in
  its wave despite repeated tip IDs or a missing ancestor beneath a present
  tip, reloads once, and performs no third fetch;
- one explicit promisor refetch followed by deterministic failure if history
  remains incomplete;
- publication pushes which cannot recursively invoke the installed hook or
  inherit follow-tag, submodule, signing, force-inclusion, or push-option
  behavior;
- complete success, already-desired refs, missing or duplicate porcelain
  statuses, unknown refs, malformed output, and lost tuple or marker-push
  acknowledgements, including successful earlier marker batches;
  and
- timeout, cancellation, stdout overflow, stderr discard, cleanup failure, and
  every post-request failure mapping to `Indeterminate`.

### GitHub boundary tests

GitHub adapter tests use a strict scripted HTTP transport. Each test declares
the complete ordered request sequence and explicit responses. Unexpected
requests, wrong request order, malformed documents, and unconsumed expected
responses fail the test.

Tests cover complete open-pull-request pagination, null and disagreeing default
branches, strict reserved metadata, owned-base correlation without global tag
evidence, local name collisions, fork isolation, identity registration for
unmanaged and fork OPEN rows, deleted source refs, duplicate open pull requests,
active nonlocal validation without mutation, batched terminal lookup
pagination, marker-aware creation authorizations and fail-closed results,
query splitting, exact retry delays,
resource-only query reduction, fatal mixed or partial GraphQL responses,
`errors: []`, exact mutation alias sets, and provisional creates using head `G`
and base `gherrit-bases/G` for both root and nonroot intent. They cover lost
create acknowledgement, OPEN absence before and after a marker, a same-key
duplicate-conflict response which is indeterminate and releases no marker or
update, final numbered projection only after marker acknowledgement, mutation
sizing, arbitrary subsets of complete aliased operations, the absence of
partial per-operation field outcomes, missing receipts, and receipt identity
mismatches. A marker-only locally observed exact projection releases
`NoAction` after marker acknowledgement and sends no GraphQL mutation. Boundary
cases exercise the exact supported title and body limits, an over-limit
provisional body, and a worst-case final render which crosses the body limit
only after one more history row.

The scripted transport can prove request shape and outcome classification, but
it cannot establish GitHub's backend same-key uniqueness. That behavior remains
an explicit operating assumption. Observed API behavior is evidence of
duplicate rejection, not proof of atomic enforcement. The fixtures may omit a
row after an earlier observation; they never treat eventual GitHub visibility
as creation authority for a marked identity.

### Complete-process tests

A small process suite verifies only composition claims that cannot be
established at a lower layer:

1. An installed pre-push hook publishes a complete successful stack, including
   the marker barrier before final numbered projection.
2. Invalid or unsupported state blocks the enclosing Git push and changes no
   managed remote ref or pull request.
3. A previously published Git tuple with incomplete pull request work converges
   on fresh invocations. The recovery trace independently loses a create
   acknowledgement and a marker acknowledgement, omits an unmarked provisional
   pull request, repeats the identical owned-base create without a duplicate,
   omits OPEN after the marker and fails closed, then supplies eventual stable
   visibility and converges. Root/nonroot intent changes retain the same create
   key throughout.
4. The global Git head advertisement and first repository-wide
   OPEN query start concurrently. Local namespace observation and graph
   completion run after local derivation, without a repository-wide tag scan,
   and
   correlation waits for both that work and complete OPEN pagination.
5. Correlation succeeds from metadata and owned-base evidence, then starts tag
   observation only for newly discovered nonlocal active IDs.
6. Missing local history starts exact object acquisition after its tag
   observation while OPEN pagination may continue. Correlation waits if local
   graph completion outlasts pagination, and validation waits for the final
   active graph and every terminal lookup.

Tests snapshot complete deterministic plans, diagnostics, rendered pull request
bodies, and protocol traces. Explicit invariant assertions accompany snapshots.
Request-budget snapshots enforce Git command counts, GraphQL document counts,
head and tag response sizes, local and nonlocal namespace-batch counts,
mutation batch sizes, marker-push counts and receipts, object-fetch counts, and
the absence of a repository-wide tag scan, extra marker query, extra GraphQL
mutation, local ref writes during observation, pre-Git staging, rollback,
confirmation, and same-attempt re-observation requests.

## Guarantee boundary

Under this document's operating assumptions, GHerrit guarantees:

- no indirect merge caused by any published or proposed pairing of a managed
  head and its owned base or, for a root, the agreed default-branch tip;
- no duplicate from a repeated create while an OPEN pull request occupies the
  identical base-repository, head-repository, head-ref, and base-ref key;
- safe acknowledged prefixes of tuple, marker, and GraphQL batches, and safe
  arbitrary subsets of complete aliased operations within one transmitted
  GraphQL batch;
- resumability after crashes and indeterminate acknowledgements;
- exactly leased Git updates with immutable version history and pull request
  existence markers;
- convergence of derived pull request metadata, provided exact Git
  advertisements remain authoritative, durable committed GitHub effects
  eventually remain visible to all later planning observations, and
  still-needed operations eventually receive usable acknowledgements, for
  operations which fit the supported content and mutation request limits; and
- bounded network phases for normal publication.

The duplicate and resumability guarantees require GitHub to atomically reject
a second same-repository OPEN pull request for the identical creation key
without creating another pull request. Neither the official generic `422`
contract nor the observed CLI and direct-API duplicate rejections establish
that backend rule or its atomicity. It remains an explicit undocumented
operating assumption. The protocol does not assume that a pull request row
remains present in later paginated OPEN observations. Exact Git ref
advertisement, by contrast, is complete authoritative evidence under the
quiescent-writer contract and is not paginated. Once the durable marker exists,
arbitrary OPEN absence authorizes only terminal retirement or a fail-closed
result. Eventual stable GitHub visibility supplies convergence; it never
supplies permission to recreate a marked identity.

It does not guarantee:

- serializability across concurrent publishers;
- safety against concurrent default-branch movement;
- safety against manual mutation of managed refs, tags, or pull request
  topology;
- preservation of concurrent manual pull request metadata edits;
- use of native auto-merge or merge queues for non-root pull requests; or
- automatic rebasing after a root pull request merges.

[git-push]: https://git-scm.com/docs/git-push
[indirect-merge]: https://docs.github.com/en/pull-requests/reference/pull-request-merges
[create-pr-api]: https://docs.github.com/en/rest/pulls/pulls#create-a-pull-request
[direct-api-duplicate]: https://github.com/googleapis/release-please/issues/2773
[same-key-pr]: https://github.com/cli/cli/discussions/5792
