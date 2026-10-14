# Read-only inspection

`gherrit status` (`gherrit inspect`) reports local management intent and drift,
per-worktree/common/workdir/effective-hook paths, Git capabilities, and commit
identity/message problems. `--json` emits report schema version 1. Names and
paths carry UTF-8 when valid plus exact hexadecimal bytes. Human display uses
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
observation deadline after credential lookup. Git ref reads retain the
separate 120-second supervisor policy. Remote inspection requires GitHub CLI;
the publisher's existing HTTP adapter remains unchanged. The reader exposes
no mutation method, consuming plan, or permission to publish terminal PRs.

*Updated by an AI agent acting on joshlf's behalf.*
