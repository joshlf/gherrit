# Production subprocess inventory

All production child spawning belongs to `pre_push/subprocess/{unix,windows}`.
The borrowed synchronous entry and async remote entry share spawn ownership,
foreground terminal leasing, finite concurrent pipe draining, cleanup, and
leader reaping. Unit fixtures own any raw child they deliberately launch.

| Caller | Input | Stdout cap | Execution | Interaction / authority |
| --- | --- | --- | --- | --- |
| Remote advertisement, fetch, atomic push | null or prepared read-only regular file (64 MiB) | 64 MiB | 120 seconds | inherited terminal; exact destination; sealed output |
| Local config, repository checks, URL resolution | null | 4 MiB | 60 seconds | no network commands; original environment binding |
| Commit-message parsing, hash, insertion | null; cleanup uses prepared finite file | 4 MiB | 60 seconds | Git remains message formatter |
| `gh auth token` | null | 64 KiB | 30 seconds | inherited credential environment; output never logged |

Stderr retains its final 16 KiB and drains/counts all preceding bytes. Remote
output remains sealed behind destination redaction and terminal escaping.
Local config preserves exact stdout bytes and tests stderr emptiness, which
truncation cannot turn into an empty diagnostic. Local checked-command errors
escape terminal control bytes. Local status-entry output is forwarded after complete capture; checked output
remains caller-owned. Production local commands have no progress stream.
Forwarding preserves their existing output contract, bounded to the retained
bytes; unlike publication diagnostics it is not evidence of remote effects.

These new local/auth deadlines and caps are observable hardening: oversized
or stalled commands now stop. They are separate from the HTTP observation
deadline; credential lookup completes before that deadline starts. Each
execution has the existing additional five-second cleanup bound. A cleanup
failure is reported, never treated as proof that children are gone. Unix
descendants escaping the owned process group are outside the guarantee;
Windows uses the existing private kill-on-close job. Synchronous callers do
not gain async cancellation, but retain deadline cleanup and unwind guards.

Tests cover binary bytes/status, stdout overflow, no spawn at zero timeout,
prepared stdin, descendant termination, terminal restoration, and platform
fault paths. The 80 real Git message-cleanup witnesses remain independent of
the hook and exercise the new finite input path.

Audit with `rg -n '\.output\(|\.status\(|\.spawn\(' src`. Distinguish
test-only witnesses and status accessors from child execution. A new child
must choose this lifecycle boundary and an explicit finite policy; do not
replace it with `std::process::Command::output` in production.
