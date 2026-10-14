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

*Updated by an AI agent acting on joshlf's behalf.*

The stack's `identity_ancestry` fact checks active IDs against all commits
reachable from HEAD, including the selected base and merged side parents. It
scans at most 10,000 commits and explicitly reports incomplete or unavailable
evidence. Unsupported branch-name encoding makes management unavailable while
leaving object and commit inspection available.
