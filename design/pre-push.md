# Pre-push publication

GHerrit publishes a local stack of commits as Git refs and GitHub pull
requests. This document defines the Git representation, the safety conditions
for publishing it, and the protocol used by the pre-push hook.

The central rule is simple: every GHerrit change owns both its pull request head
branch and its pull request base branch. A change's base branch moves only when
that same change is published.

Git is the authoritative record of published change versions. GitHub pull
requests are derived from that record and the local stack.

This file is the canonical specification for pre-push publication. Until the
behavior-activation commit updates the repository's other documentation, this
file governs whenever another document describes a different publication
model.

## Scope

This design applies when a managed branch is pushed through GHerrit's pre-push
hook. It covers:

- deriving a stack from local Git history;
- validating local and remote state;
- publishing managed Git refs;
- creating and updating GitHub pull requests;
- retries after crashes, rejected writes, and lost acknowledgements; and
- preventing GitHub from indirectly merging an active managed pull request.

It assumes one GHerrit publisher at a time, no manual mutation of managed refs
or version tags, no independent automation which writes managed state, complete
Git history, exactly one configured push destination, and no concurrent
movement of the default branch.

Exact Git leases detect many violations of these assumptions. They do not make
the Git and GitHub operations a cross-system transaction, and GitHub pull
request updates do not provide compare-and-swap protection.

### Activation gate

The commit which first enables this protocol must be the final active pull
request published with any other representation. Every preceding pull request
in its implementation stack must be merged first, and the activation pull
request itself must be merged before the enabled client is used. The first
attempt under this protocol must therefore find no active pull request which
requires conversion. This release-ordering invariant is a gate on activation;
the protocol does not contain a representation-conversion path.

## Push destination

GHerrit resolves the selected remote before it reads local stack history or
performs network I/O. The configured remote name is a validated value. It must
name one remote and must produce exactly one push destination. Missing,
repeated, non-UTF-8, or otherwise malformed configuration is an error rather
than a reason to use a different remote.

The resolved push destination is the one literal destination for the entire
attempt. It determines the GitHub owner and repository and is retained in a
private value. Configured fetch destinations and remote-selection defaults do
not participate after resolution. Supported destinations include URLs,
scp-like Git destinations, and local paths used by repository tests.

Every Git subprocess addresses that literal through a reserved internal
remote. GHerrit reads the configured remote names and chooses the first absent
name in this deterministic sequence:

```text
gherrit-publication
gherrit-publication-1
gherrit-publication-2
...
```

Each remote command receives these command-scoped configuration entries in
this order:

```text
-c remote.<internal>.url=
-c remote.<internal>.pushurl=
--config-env=remote.<internal>.url=GHERRIT_PRIVATE_PUSH_DESTINATION
--config-env=remote.<internal>.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION
```

The named environment variable contains the private resolved literal. The
configuration exists only for the child process and is never written to the
repository. Destination-bearing network commands for observation, acquisition,
and publication name the internal remote rather than the destination. No
credential-bearing literal enters a child argument list or command trace. The
credential-free local matcher exception is described below.

Git URL and push-URL configuration is additive. The two empty entries reset
all earlier values before the private value is appended. This also defeats
values introduced by an `includeIf hasconfig:remote.*.url` include which becomes
active only after Git sees the injected internal URL.

Git resolves the configured remote's effective push destination using its
normal `remote.*.url`, `remote.*.pushurl`, `url.*.insteadOf`, and
`url.*.pushInsteadOf` rules. That resolution happens exactly once. GHerrit then
assigns the resulting literal to both the internal remote's explicit `url` and
explicit `pushurl`.

GHerrit inspects configuration only after activating the internal remote in the
same command context used by network operations. This makes conditional
includes triggered by the private URL visible. The only permitted keys for the
internal remote are its `url` and `pushurl`; any other key rejects the attempt.
Rewrite and redirect validation run in this same context.

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

For a credential-free `http` or `https` URL, GHerrit applies the
command-scoped `false` value and uses Git's exact URL-matching semantics to read
the effective `http.followRedirects` value for the literal URL. This matcher is
the one exception which places the literal destination in a child argument
list, and it is permitted only because the URL contains no credentials. The
result must be exactly one `false` value. For an HTTP or HTTPS URL containing
userinfo, GHerrit does not pass the credential-bearing URL to a matcher
command. It instead uses the configuration names observed with the internal
remote active and rejects any URL-scoped `http.<url>.followRedirects` key.

A push destination may contain credentials. It has no `Display` or debug form
which reveals the raw value. Command traces, errors, captured standard error,
and normalized Git diagnostics identify the selected remote without printing
the destination. Before destination resolution and every later Git probe,
observation, acquisition, or push, GHerrit removes every inherited environment
variable whose name case-insensitively matches `GIT_TRACE*`, along with
`GIT_CURL_VERBOSE`. Git therefore cannot persist a private destination through
an inherited transport trace or curl diagnostic stream.

## Change identity and local stacks

Each managed commit has exactly one nonempty `gherrit-pr-id` trailer. Its value
is the change ID.

A local stack is the ordered first-parent path from the default branch to
`HEAD`. It is valid only when:

- every stack commit has exactly one valid change ID;
- active change IDs are unique;
- no change ID equals the default branch name;
- the default branch is an ancestor of `HEAD`;
- each stack commit's first parent is available locally; and
- ancestry required by the checks in this document is complete.

The stack order determines parent and child relationships. The implementation
does not separately store those relationships where it can derive them from the
ordered commits.

A merge commit may appear in Git history. Stack order follows its first parent,
while identity and reachability validation inspect all reachable ancestry.

## Refs owned by a change

For change ID `G`, let:

- `H_G` be its current commit;
- `P_G` be `first_parent(H_G)`;
- `refs/heads/G` be its mutable head branch;
- `refs/heads/gherrit-bases/G` be its mutable owned base branch; and
- `refs/tags/gherrit/G/vN` be its immutable version tag.

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
create or consult local version tags. Two version tags may point at the same
commit; their version numbers remain distinct history records and determine the
patch count and table rows. Repeated object IDs are deduplicated only when
performing graph work.

A complete published version couples its head, first parent, and version tag.
A remote head or base branch that does not agree with the latest version tag is
not a partially repairable state. GHerrit rejects it before writing.

## Pull request bases

A non-root pull request for `G` always compares its head against its own owned
base:

```text
headRefName = G
baseRefName = gherrit-bases/G
```

The base branch belongs to the change being reviewed. It never names the
mutable head branch of the change's parent.

For example:

```text
main --- A --- B

refs/heads/A                   -> A
refs/heads/gherrit-bases/A     -> main

refs/heads/B                   -> B
refs/heads/gherrit-bases/B     -> A
```

The pull request for `B` compares `B` with `gherrit-bases/B`. Publishing a new
version of `A` does not move `B`'s base. Publishing a new version of `B` moves
both `B` and `gherrit-bases/B` together.

A root pull request targets the repository's default branch:

```text
root PR baseRefName = <default branch>
```

GHerrit still maintains the root's owned base branch at the root head's exact
first parent. This permits the change to become non-root later without creating
a different class of Git representation.

A pull request's base name changes only when its root status changes:

- root to non-root: `<default branch>` to `gherrit-bases/G`;
- non-root to root: `gherrit-bases/G` to `<default branch>`.

Amending, rebasing, or reordering non-root changes does not change their pull
request base names.

## Historical reachability safety

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

For one change ID `G`, let:

- `H_G^i` be any distinct valid head version ever published for `G`;
- `P_G^j` be `first_parent(H_G^j)`; and
- `X <= Y` mean that commit `X` is reachable from commit `Y`, including
  equality.

Every published and proposed version must satisfy:

```text
for every i and j: not (H_G^i <= P_G^j)
```

Suppose instead that `H_G^i <= P_G^j`. Since `P_G^j` is the first parent of
`H_G^j`, then:

```text
H_G^i <= P_G^j < H_G^j
```

If `H_G^i` and `H_G^j` are distinct, the history reachable from `H_G^j`
contains two commits carrying `gherrit-pr-id: G`. That violates the identity
rule that the ancestry of a published head contains exactly one commit carrying
that head's change ID.

If they are the same commit, a commit is reachable from its own parent. That
would require a cycle in Git's commit graph.

No valid published head is therefore reachable from the parent of any valid
published head for the same change. An owned base always points at one of those
parents, so every historical or current pairing of a managed head and base is
safe.

```text
historical/current head H_G^i
    x
historical/current owned-base tip P_G^j
```

Checking only the old and new values for one push is insufficient. A delayed
observer can pair a version-one head with a version-three base. The complete
history check covers that pairing.

## Validation

GHerrit validates local intent and remote observations before making any write.

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
2. The ancestry of every historical, current, and proposed head contains
   exactly one commit carrying the change's ID.
3. Existing version tags are canonical, contiguous, lightweight, immutable,
   and point at commits carrying the expected ID. Different version numbers
   may point at the same commit without collapsing into one version.
4. Historical version heads and their first parents are locally available.
   Shallow or incomplete history is not accepted as evidence of safety.
5. The current head branch agrees with the latest version tag.
6. The current owned base agrees with the exact first parent of the current
   head branch.
7. Every historical, current, and proposed head is unreachable from every
   historical, current, and proposed first parent for that change.
8. A GitHub head object ID is one of the validated historical, current, or
   proposed heads for that change.
9. A GitHub owned-base object ID is one of the validated historical, current,
   or proposed first parents for that change.

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

### Object acquisition

Remote observation can advertise a version whose objects are not present
locally. GHerrit acquires missing history through the exact tag ref names from
that advertisement. It does not fetch a raw object ID and does not ask Git to
select a ref through configured fetch rules.

Object acquisition uses this request shape:

```text
git fetch --no-write-fetch-head --no-tags --no-recurse-submodules \
    --no-auto-maintenance --filter=blob:none -- <internal-remote> \
    <exact-advertised-tag-ref>...
```

The refspecs are source-only and name only tag refs present in the same remote
advertisement used for validation. The request does not create or update a
local ref, remote-tracking branch, tag, or `FETCH_HEAD`. It does not recurse
into submodules or run automatic repository maintenance, and it acquires no
blob which the remote can omit.

A normal exact-ref fetch may omit an object which a partial clone already
considers promised. If validation still encounters a missing object and the
repository has a promisor remote, GHerrit performs one second request with the
same flags and advertised source-only refs plus `--refetch`. A missing object
after that request is incomplete evidence and rejects the attempt. There is no
fetch-until-success loop or incidental object access which performs network
I/O.

Local IDs are active by construction, so their advertised version refs are
known as soon as local derivation finishes. Acquisition for missing local
history starts then, while GitHub pagination continues. Correlation can reveal
additional active nonlocal IDs; their missing refs use a second logical
acquisition wave. A wave is one request unless platform command limits require
batching, and each ordinary batch permits at most one promisor refetch. Starting
the first wave early avoids serializing the common cold-local-history fetch
behind repository-wide GitHub pagination.

### Root validation

The owned-base proof does not apply to the default branch. For every change
which is currently root or will become root, GHerrit separately verifies that
every historical, current, and proposed head is unreachable from the observed
default-branch tip.

The remote symbolic `HEAD`, its advertised target branch, GitHub's repository
default branch, and the corresponding local branch must have the same name and
object ID before GHerrit plans a root operation. The advertised Git ref tip is
the authority for root reachability. A root pull request's observed base object
ID must equal that tip.

A default branch which moves concurrently can absorb a root head through an
external merge. A client-only GitHub protocol cannot prevent or distinguish
that event, which is why default-branch stability is part of this design's
operating assumptions.

### Supported repository state

A repository is publishable only when its managed refs and open managed pull
requests use the representation in this document.

For a change which has never been published, the remote head, owned base,
version tags, and pull request are all absent. Every published active change has
a complete current head/base/tag tuple. Each open managed non-root pull request
targets its own owned base. Each open managed root pull request targets the
repository default branch. Managed identities remain associated with the same
base repository; a same-named fork branch is not the managed head.

An active managed ID is either in the local stack or belongs to an open
same-repository pull request identified by GHerrit's body metadata or matching
managed Git refs and version tags. Metadata, head names, and refs which purport
to identify the same managed change must agree. A same-named branch without
GHerrit metadata or managed version history is not classified as managed merely
because its name resembles a change ID.

An active nonlocal change participates in repository and reachability
validation, but a publication attempt never changes its pull request. Only IDs
from the local stack can produce Git updates, pull request creations, or pull
request metadata updates.

A repository containing an incomplete or conflicting managed representation is
rejected before publication. GHerrit does not mix representations within one
publication attempt.

An active change ID cannot equal the agreed default branch name. Because a
change ID cannot contain a slash, this collision can occur only for a
top-level default branch. `refs/heads/G` would otherwise be the default branch,
and publishing a managed head could force-update the branch which roots the
stack.

A closed or merged pull request permanently retires its change ID. GHerrit does
not create another pull request for an identity which historical observation
shows has already been used.

## One publication attempt

A publication attempt moves only forward:

```text
resolve push destination
    -> start Git and GitHub global observations concurrently
    -> as soon as Git observation establishes the default,
       derive local intent and start any local-ID object acquisition
       while GitHub pages continue
    -> after GitHub observation and local derivation both complete,
       correlate identities
    -> acquire any additional nonlocal objects and complete terminal lookups
    -> validate and construct an immutable publication plan
    -> publish Git tuples
    -> create missing pull requests
    -> validate every create receipt and construct an immutable projection plan
    -> project final pull request state
```

There is no preparatory pull request state, rollback phase, post-push
confirmation phase, or same-attempt re-observation.

The publication plan contains every decision which can be made before writes.
It does not contain optional placeholders for identities which GitHub has not
assigned. Instead, each local pull request is represented as either one known
open identity or one create specification coupled to its absence proof.

After creation, a pure transition consumes the publication plan and a complete,
exact set of create receipts. It either constructs a separate immutable
projection plan or classifies the create acknowledgement as indeterminate and
stops. Execution never fills in a mutable plan or infers which remaining
actions are safe from partial local bookkeeping.

### Observe Git and GitHub

Git observation is one remote ref advertisement:

```text
git ls-remote --quiet --symref -- <internal-remote> \
    HEAD 'refs/heads/*' 'refs/tags/gherrit' \
    'refs/tags/gherrit/*'
```

The arguments are constant in size. The command obtains the remote `HEAD`, all
heads, the reserved tag namespace root, and every GHerrit version tag in one
network request. There is no per-ID query or conditional ref follow-up.

GHerrit parses the advertisement as bytes. Unrelated ref names need not be
UTF-8. Every record must have either the documented direct object-ID, tab,
ref-name shape or the symbolic `ref:`, target, tab, ref-name shape. Duplicate
direct or duplicate symbolic observations for a ref are rejected, as is a ref
other than `HEAD` observed in both forms. A symbolic `HEAD` record, the direct
`HEAD` object ID, and the advertised target head must all be present and agree.
A direct-only `HEAD`, a missing target, or disagreement is invalid repository
state.

The patterns passed to `ls-remote` use Git's tail-matching rules. The literal
`HEAD` pattern can therefore return unrelated refs whose final component is
`HEAD`; GHerrit ignores every such ref except the exact pseudoref. The
`refs/heads/*` result contains nested heads, including the owned-base
namespace.

The reserved names have exact grammars:

```text
refs/heads/gherrit-bases/<change-id>
refs/tags/gherrit/<change-id>/v<positive-canonical-decimal>
```

A namespace-root ref, invalid change ID, zero or leading-zero version, numeric
overflow, extra path component, or non-UTF-8 reserved name rejects the
attempt. Other heads remain unrelated evidence unless their complete top-level
name is a valid change ID. A top-level same-named branch alone does not prove
that it is managed.

Without `--refs`, Git emits a second `^{}` record when an advertised tag is
annotated. Managed version tags must be lightweight, so any peeled record in
the GHerrit namespace rejects the attempt. The object acquisition and history
checks later require every lightweight managed tag to point at a commit.

The wire parser recognizes the lengths and hexadecimal syntax of SHA-1 and
SHA-256 object IDs and requires one format throughout the advertisement. The
graph reader supports SHA-1 repositories. A SHA-256 advertisement therefore
produces an explicit unsupported-format error before a graph object ID is
constructed.

GitHub observation obtains:

- the repository node ID and default branch;
- the repository's complete set of open pull requests;
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

This phase performs no network writes.

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

A cross-repository pull request is ignored before metadata parsing. For a
same-repository pull request, valid metadata identifies its change only when
the metadata ID equals `headRefName`. Without metadata, a pull request is
managed only when its head name and reserved owned-base or version-tag evidence
identify the same change. A same-named head alone is not enough.

A same-repository open pull request whose head collides with a local ID but has
neither metadata nor managed history is unsupported state. GHerrit rejects it
rather than adopting an unrelated pull request. Conflicting identity signals,
duplicate node IDs or numbers, and more than one managed open pull request for
one change also reject the attempt.

An open pull request remains observable after its source ref is deleted.
Metadata, its remembered head name and object ID, and remaining managed history
still correlate it before tuple validation rejects the missing ref. GHerrit
does not mistake it for an unused ID and create another pull request.

Only local IDs without a correlated open pull request receive terminal
lookups. GHerrit batches them as aliased connections with one cursor per ID. A
same-repository closed or merged pull request retires the ID. Fork results are
ignored and pagination continues. Exhausting the connection produces the proof
required to create a pull request. The common case completes every ID in one
request. Missing or repeated cursors, an unexpected lifecycle state, or a
mismatched head name is invalid query evidence.

### Normalize and plan

Raw Git and GraphQL values may be malformed, incomplete, stale, or
contradictory. Validation converts them into domain values or rejects them.
Only validated values enter the planner.

The active set is the union of local IDs and correlated same-repository open
managed pull requests. Every active change receives complete Git history and
head/base safety validation. Nonlocal active changes stop there: they establish
repository eligibility but cannot produce an external action.

An open managed pull request is validated as either root or non-root. A root
base must name the agreed default branch and have its exact observed object ID.
A non-root base must name `gherrit-bases/G`, and its object ID must be a
validated historical, current, or proposed first parent for `G`. Any other
base is invalid. A non-root pull request cannot have native auto-merge enabled
or belong to a merge queue. A root pull request with either feature enabled
cannot be moved to a non-root base.

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

For each local change, pull request state is either one validated open pull
request or an absence proof obtained by exhausting its terminal lookup. Git
publication state is independent: a complete tuple with a missing pull request
is a normal crash-recovery prefix.

GHerrit supports pull request titles containing at most 256 Unicode scalar
values and generated bodies containing at most 131,072 UTF-8 bytes. These are
explicit product limits, independent of mutation batching. The planner rejects
an over-limit title or body before publishing Git state.

Before any write, the planner renders every provisional body and every final
body whose pull request numbers are known. For every final body which depends
on a missing pull request's number, it renders an upper bound using the widest
decimal representation of `2,147,483,647`, the largest positive value permitted
by GitHub's GraphQL `Int` type. It similarly proves that each individual create
or update fits in an otherwise empty mutation batch. An assigned number cannot
make the final action larger than that bound.

The immutable publication plan also contains:

- complete specifications for missing local pull requests, each carrying its
  absence proof;
- known identities for existing local pull requests; and
- the validated information needed to construct final projection after all
  create identities are known.

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
of a planned tuple; no configured default refspec participates.

Both mutable branches use exact lease protection. A missing mutable branch uses
an absence expectation. If another publisher has already installed the exact
desired object, Git may report the ref as up to date; that is equivalent to an
acknowledged result. Any different object rejects the lease. Version tags are
create-only and never force-updated, with the same already-desired behavior.

One Git push contains all tuples when platform command limits permit it. When a
stack requires multiple pushes:

- a change's head, owned base, and version tag never appear in separate
  batches;
- every batch is atomic and exactly leased; and
- every acknowledged batch prefix is safe.

GitHub need not observe the branches atomically. The historical reachability
invariant makes every old or current combination safe.

An acknowledged successful push is sufficient. A post-push remote query cannot
add a serialization guarantee under this design's assumptions. A lost
acknowledgement ends the attempt; a later invocation observes whether the
atomic batch landed.

The adapter treats porcelain output as a receipt protocol. Acknowledged success
requires a successful process exit and exactly one recognized status for every
planned ref, each reporting either the requested update or an already-desired
value. Acknowledged rejection requires complete evidence that every requested
change was rejected by the atomic operation; statuses for refs which were
already desired may accompany that rejection. An unplanned ref, duplicate or
missing status, malformed framing, contradictory status, transport failure, or
successful update reported alongside an unsuccessful process exit is
indeterminate. Human-readable standard error is normalized for diagnostics but
is not acknowledgement evidence.

### Create missing pull requests

GHerrit creates a missing pull request only after its head and final base exist.

A root pull request is created against the default branch. A non-root pull
request is created against `gherrit-bases/G`.

Several new pull requests may require their assigned numbers to render complete
parent/child navigation. GHerrit creates them with a deterministic provisional
body containing the commit text and complete reserved metadata, but no
number-based navigation. The metadata lets a fresh attempt correlate any
create whose acknowledgement was lost.

Every create receipt couples the requested head with the new pull request
number and GraphQL node ID. After all acknowledged receipts are available,
GHerrit consumes the publication plan and receipts to construct one numbered
local stack and a separate projection plan. Receipt aliases, requested heads,
`clientMutationId` values, pull request numbers, and GraphQL node IDs must form
an exact one-to-one match with the planned creates. Exactly one identity per
local change is required before the transition can produce a projection plan.

An indeterminate create acknowledgement ends the attempt. On a fresh attempt,
the global open-pull-request scan correlates every create which ran; only IDs
which remain absent repeat the terminal lookup and creation plan.

### Apply final pull request state

For every local pull request, GHerrit derives the complete desired state:

- title;
- body;
- patch-history table;
- complete ordered stack navigation, including the pull request's own number;
  and
- root or non-root base name.

It compares that state with the observed state and sends only necessary
updates. A provisional create omits all number-based navigation, including its
own number, so every newly created pull request receives a nonempty update with
the final numbered body. Every other update action is also nonempty by
construction. Nonlocal managed pull requests are validated but never projected
by this attempt. There is no temporary base branch.

Each patch-history row compares that version against its own immutable
first-parent object ID:

```text
<version-first-parent-oid>...gherrit/G/vN
```

It does not use the mutable current owned-base branch. A later rebase therefore
does not change the meaning of an older row's Base link. The version tag keeps
the head and its parent reachable, so a separate base tag is unnecessary.

During a root-status transition, either observable base is safe. The owned base
is safe by the historical reachability invariant, and the default branch is
safe by root validation. A partial update batch therefore leaves a pull request
on either its old safe base or its final safe base.

## Acknowledgement and retry rules

Queries, Git pushes, and GraphQL mutations have different semantics. They do
not share a generic retry mechanism.

### Queries

Queries are read-only. GHerrit may retry transient transport failures, back off
from deterministic resource limits, and split a query before retrying it.
These retries are explicit query behavior rather than transparent HTTP-client
retries shared with writes.

A retried query remains part of the initial observation phase. GHerrit does not
query again after writes in order to decide how to continue the same plan.

### Git pushes

An atomic Git push has three possible outcomes:

- acknowledged success;
- acknowledged rejection, in which the batch changed nothing; or
- indeterminate acknowledgement, in which the whole batch may or may not have
  changed.

GHerrit stops on an indeterminate outcome. Both possible remote states are safe,
and a subsequent invocation starts by observing the resulting state.

### GraphQL mutations

GHerrit sizes mutation batches before transmission by both alias count and
serialized byte size.

The HTTP transport neither retries mutation requests nor follows redirects
which preserve and resend their POST bodies. Every mutation batch is
transmitted at most once per publication attempt.

A mutation batch is acknowledged only when its response contains every
expected alias, expected `clientMutationId`, and expected returned pull request
identity. Those returned fields are receipts for the acknowledged mutations.

A timeout, transport failure, malformed response, GraphQL error, missing alias,
null result, or mismatched receipt after transmission is indeterminate. Some
mutation fields may have executed. GHerrit stops without replaying, rechunking,
rolling back, or continuing the plan.

`clientMutationId` correlates a response with a request. It is not an
idempotency key.

The complete rule is:

> An indeterminate external write ends the attempt. A later invocation begins
> with fresh observation and pure replanning.

## Failure prefixes

| Failure point | Visible state | Reason it remains safe |
| --- | --- | --- |
| Before Git publication | Nothing changed | No write occurred |
| Atomic Git rejection | That batch did not change | Git atomicity |
| Earlier Git batches succeed | A coherent prefix is published | Each change tuple is coherent and all versions are safe together |
| Git acknowledgement is lost | A batch is either old or new | Both possibilities satisfy the invariant |
| Crash after Git publication | Git is current; PRs may be stale | Existing and final bases are safe |
| Some creates succeed | Some PRs have provisional bodies | Every created PR has complete identity metadata and its final safe base |
| Some updates succeed | Each PR has old or final metadata | Both possible bases are safe |
| A query is stale | A write may fail or be redundant | Freshness is not part of the reachability proof |
| Process restart | A fresh attempt begins | Remote refs, tags, and PRs describe the remaining work |

Rollback is not part of publication. It would introduce additional
failure-prone writes and can restore older combinations which no longer belong
to the current plan. Every acknowledged effect is a safe forward step.

## Pull request lifecycle

Managed IDs are permanent identities. A historical pull request for an ID
prevents later reuse of that ID.

A non-root pull request targets an owned scratch base and is not landable on the
default branch. GHerrit rejects a non-root pull request with native auto-merge
enabled or enrolled in a merge queue. Repository policy should reserve and
protect the `gherrit-bases/**` namespace.

Only a settled root pull request is landable through GitHub's normal merge
operation.

GHerrit does not automatically rebase the remaining stack after a root pull
request lands. The user rebases the remaining commits and publishes them
through the normal pre-push hook.

## Code model

The implementation distinguishes raw observations, validated domain values,
and immutable external actions.

Raw Git refs, tag names, GraphQL values, and JSON fields may contain arbitrary
strings, absent values, and contradictions. Constructors validate those values
before the planner receives them.

The core model has these properties:

- a change ID is nonempty and syntactically valid;
- Git publication history is keyed by change ID and is either absent or a
  nonempty ordered sequence of published versions;
- each published version stores only its head and first parent; its positive
  version number and immutable tag ref are derived from its one-based position
  and the history's change ID;
- the mutable current head and base are derived from the final published
  version rather than stored as independent history fields;
- repeated heads remain distinct entries in the ordered version sequence;
- a pull request identity couples its number and GraphQL node ID;
- an ordinary projected pull request is open by construction;
- a pull request base is either the default branch or the owned base of the
  same change;
- a missing pull request can be created only with an opaque terminal-exhaustion
  proof;
- nonlocal active changes cannot be converted into mutation actions;
- a publication plan represents each local pull request as either an existing
  identity or a create specification and absence proof, never an optional
  identity placeholder;
- a projection plan can be constructed only by consuming a publication plan
  and an exact complete create-receipt set;
- final pull request bodies require one numbered identity per local change and
  contain the complete ordered local stack, including the current pull request;
- stack order derives parent and child relationships;
- a concrete create or update action is nonempty; and
- both publication and projection plans are immutable.

A late recovery path does not receive arbitrary raw state and decide whether to
repair it.

`pre_push/mod.rs` remains a short orchestration module. Behavior-oriented
submodules contain local intent, Git publication history and planning, pull
request state derivation, and GitHub protocol handling.

## Performance

Network round trips and GitHub backend work dominate local graph traversal. The
protocol minimizes those expensive operations without depending on a second
observation for correctness.

Let:

- `O` be the number of pages in the repository-wide open-pull-request
  connection;
- `T` be the batched terminal-query requests for local IDs without open pull
  requests;
- `F` be exact object-acquisition fetches, normally zero;
- `P` be atomic Git push batches;
- `C` be create-mutation batches; and
- `U` be final-update batches.

Every attempt has exactly one `ls-remote` request. `O` is at least one. `T` is
zero for an established stack and normally one for any number of new local
IDs. Query resource backoff, pagination, command-size batching, and transient
read retries can increase the corresponding count.

| Operation | Git reads | Git writes | GraphQL reads | GraphQL writes |
| --- | ---: | ---: | ---: | ---: |
| Established no-op | `1 + F` | 0 | `O` | 0 |
| Existing amend or reorder | `1 + F` | `P` | `O` | `U` |
| New local pull requests | `1 + F` | `P` | `O + T` | `C + U` |
| Restart after ambiguity | Fresh normal attempt | As needed | Fresh normal attempt | As needed |

In the common single-page, single-batch case, an existing amend uses one Git
read, one GraphQL read, one Git push, and at most one GraphQL update. A new
stack adds one terminal-query request, one create request, and one final-update
request.

Git's global head advertisement performs work proportional to the repository's
heads, and its response contains all of them. This single prefix scan avoids a
second sequential Git request after the GitHub scan discovers nonlocal managed
IDs. GHerrit records advertisement bytes, ref counts, and elapsed time at trace
level so an observed large-repository bottleneck can be addressed without
changing the observation model.

The two global observations run concurrently. Local-ID acquisition begins when
local derivation completes and does not delay correlation. Correlation
identifies additional active nonlocal changes; their acquisition and terminal
queries then begin together. Validation waits for both acquisition waves and
the terminal queries. The dependency graph is approximately:

```text
max(ls-remote -> local derivation, open-PR pages) -> correlate
local-ID acquisition starts after local derivation and overlaps that maximum
correlate -> max(remaining local acquisition,
                 nonlocal acquisition,
                 terminal pages)
          -> validate and plan
    -> atomic Git publication
    -> create missing PRs, if any
    -> final GitHub update, if needed
```

The representation adds one mutable base branch per active change. Immutable
history remains in tags rather than accumulating one base branch per patch
version.

## Testing obligations

Tests establish the safety proof and the correctness of each external boundary.

### Pure graph and planning tests

Pure tests enumerate bounded histories containing amends, rebases, stack
reorders, moves between stacks, reused Git objects, duplicate IDs in ancestry,
multiple historical versions, repeated version object IDs, and root or non-root
transitions. Body tests prove that each historical Base link retains that
version's first-parent object ID after later rebases.

Publication-history construction tests prove that absent and nonempty published
history are the only domain states, that version numbers and tag refs derive
from sequence positions, that the current tuple derives from the last entry,
and that repeated head IDs do not collapse entries.

Local-intent tests reject a commit body containing the reserved metadata prefix
before any external observation or write. Identity tests reject a local or
nonlocal active change ID equal to the default branch name before publication.
Rendering tests accept titles and provisional and worst-case final bodies at
their exact supported limits and reject the next larger value before emitting
an external action.

For every valid generated history, tests assert:

```text
for each change G:
    for every published or proposed head H_G^i:
        for every published or proposed parent P_G^j:
            H_G^i is not reachable from P_G^j
```

For every invalid history, planning fails before any external action is
emitted.

A test-only semantic world models commit reachability, managed refs, immutable
tags, and pull request lifecycle. Whenever an open pull request head becomes
reachable from its base, the model permanently marks that pull request merged.

Tests enumerate all meaningful ref visibility orders, Git batch prefixes, pull
request creation prefixes with provisional bodies, pull request update
prefixes, crash boundaries, and fresh restarts from each committed prefix.
They also prove that active nonlocal pull requests never produce an action.
Receipt tests prove that only an exact complete create-receipt set can consume a
publication plan and construct a projection plan; no optional identity state is
constructible between those stages.

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
- create-only version tags;
- command-length batch splitting;
- rejected batches preserving every relevant ref;
- successful earlier batches followed by rejection;
- root and owned-base ref spelling;
- one byte-oriented global advertisement with exact pattern and command
  arguments;
- large, arbitrarily ordered advertisements with constant command-line size;
- symbolic and direct `HEAD` agreement with the advertised target branch;
- unrelated non-UTF-8 refs and malformed reserved namespaces;
- SHA-1, explicit SHA-256 rejection, mixed formats, and annotated managed tags;
- repeated tag object IDs without loss of version records;
- fetch and push destinations which resolve to different repositories;
- URL, scp-like, and local-path push destinations;
- zero or multiple push destinations rejected before writes;
- remote-name collisions and chained or divergent URL rewrites;
- configured-remote `pushInsteadOf` resolution followed by an explicit internal
  `pushurl` to which `pushInsteadOf` no longer applies;
- additive internal URL values reset before the private values, including
  values from a conditional include activated by the injected URL, and every
  other internal-remote configuration key rejected;
- matching URL-scoped HTTP redirect configuration rejected, unrelated scoped
  configuration ignored for credential-free destinations, every scoped key
  rejected for a credential-bearing destination without argument disclosure,
  and redirects rejected;
- inherited case-insensitive `GIT_TRACE*` and `GIT_CURL_VERBOSE` variables
  unable to persist a credential-bearing destination, and no credential-bearing
  destination present in command arguments or diagnostics;
- shallow repositories and grafts rejected as incomplete evidence;
- replacement refs ignored by both library and subprocess traversal;
- exact advertised-ref object acquisition which writes no ref or `FETCH_HEAD`;
- one explicit promisor refetch followed by deterministic failure if history
  remains incomplete;
- publication pushes which cannot recursively invoke the installed hook or
  inherit follow-tag, submodule, signing, force-inclusion, or push-option
  behavior; and
- complete success, complete atomic rejection, already-desired refs, missing or
  duplicate porcelain statuses, unknown refs, malformed output, and lost push
  acknowledgements.

### GitHub boundary tests

GitHub adapter tests use a strict scripted HTTP transport. Each test declares
the complete ordered request sequence and explicit responses. Unexpected
requests, wrong request order, malformed documents, and unconsumed expected
responses fail the test.

Tests cover complete open-pull-request pagination, null and disagreeing default
branches, strict reserved metadata, Git-ref correlation, local name collisions,
fork isolation, deleted source refs, duplicate open pull requests, active
nonlocal validation without mutation, batched terminal lookup pagination,
terminal-exhaustion proofs, query splitting, provisional creates, final numbered
projection, mutation sizing, partial responses, missing receipts, and receipt
identity mismatches. Boundary cases exercise the exact supported title and body
limits, an over-limit provisional body, and a worst-case final render which
crosses the body limit only after one more history row.

### Complete-process tests

A small process suite verifies only composition claims that cannot be
established at a lower layer:

1. An installed pre-push hook publishes a complete successful stack.
2. Invalid or unsupported state blocks the enclosing Git push and changes no
   managed remote ref or pull request.
3. A previously published Git tuple with incomplete pull request work converges
   on a fresh invocation.
4. The one global Git advertisement and first repository-wide open-pull-request
   query start concurrently and no conditional remote-ref query occurs.
5. Missing local history starts exact object acquisition before the final open
   pull request page completes, while validation waits for every required
   acquisition and terminal lookup.

Tests snapshot complete deterministic plans, diagnostics, rendered pull request
bodies, and protocol traces. Explicit invariant assertions accompany snapshots.
Request-budget snapshots enforce Git command counts, GraphQL document counts,
batch sizes, object-fetch counts, and the absence of local ref writes during
observation, staging, rollback, confirmation, conditional ref follow-up, and
same-attempt re-observation requests.

## Guarantee boundary

Under this document's operating assumptions, GHerrit guarantees:

- no indirect merge caused by any historical or current pairing of a managed
  head and its owned base;
- safe prefixes of Git push batches and GraphQL mutation batches;
- resumability after crashes and indeterminate acknowledgements;
- exactly leased Git updates with immutable version history;
- convergence of derived pull request metadata; and
- bounded network phases for normal publication.

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
