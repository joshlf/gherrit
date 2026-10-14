# Read-only inspection

`gherrit status` (`gherrit inspect`) reports local management intent and drift,
per-worktree/common/workdir/effective-hook paths, Git capabilities, and commit
identity/message problems. Ordinary upstream settings on unmarked or explicitly
unmanaged branches are not GHerrit configuration drift. `--json` emits report
schema version 1. Names and paths carry UTF-8 when valid plus exact hexadecimal
bytes. Human display uses
JSON escaping so terminal controls are data. Binary version and data formats
are distinct; inspection adds no persistent version marker.

Local inspection never authenticates, fetches objects, or contacts a remote.
Specify `--base <local-ref>`; otherwise its stack basis is the cached remote
HEAD. An unavailable or non-first-parent base remains unavailable. The 1000
commit budget bounds traversal. Shallow/graft/partial-clone restrictions are
reported as publication capabilities, not restrictions on installation or
status. Real Git path/format inspection remains available when gix cannot
open a backend such as reftable. This does not add publication support for it.

Reports are facts and diagnostics, not retained plans or publication authority.
Cached local refs can be stale. Inspection performs no config repair, branch
publication, lifecycle mutation, or automatic repository migration.

`--remote` explicitly observes the configured github.com push repository's
exact change refs and exhausts an all-lifecycle PR connection per local ID.
Current hosted head/base facts remain separate from immutable version-tag
OIDs; no archive objects are acquired or validated by inspection. Closed,
merged, deleted-head, and fork-head facts do not relax publisher policy.

Native membership is read with GraphQL and complete ordered stack resources
with REST. Null membership is observed absence; omitted fields, malformed
rows, duplicate identities, repeated cursors, request failures, or budget
exhaustion remain unavailable. Reads are sequential observations, not an
atomic snapshot. Every connection has 32-page/1000-row budgets; gh responses
have a 4-MiB cap, a 60-second command cap, and an overall 120-second hosted
observation deadline after credential lookup. All hosted responses together
have a 16-MiB raw-byte budget, enforced before JSON decoding. Retained PR rows
and native resource members together have a 10,000-row budget. The one current
response/connection remains independently bounded while being checked against
that aggregate allowance. Stack numbers and node IDs must identify each other
uniquely across memberships; repeated identical membership is allowed.
Git ref reads retain the
separate 120-second supervisor policy. All Git ref batches together have a
64-MiB raw-output and 100,000-selected-ref budget. Repeated identical refs
from overlapping tail filters are emitted once; conflicting observations are
unavailable. Remote inspection requires GitHub CLI;
the publisher's existing HTTP adapter remains unchanged. The reader exposes
no mutation method, consuming plan, or permission to publish terminal PRs.

*Updated by an AI agent acting on joshlf's behalf.*

The stack's `identity_ancestry` fact checks active IDs against all commits
reachable from HEAD, including the selected base and merged side parents. It
scans at most 10,000 commits and explicitly reports incomplete or unavailable
evidence. Unsupported branch-name encoding makes management unavailable while
leaving object and commit inspection available.

Shallow walks retain problems visible in the available history but mark identity
ancestry incomplete. A successful truncated walk does not establish that older
active-ID occurrences are absent.
