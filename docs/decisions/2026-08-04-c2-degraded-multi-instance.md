# C2 decision — degraded multi-instance compatibility

Date: 2026-08-04

Decision: vc-frame will not embed an old server inside a new client or silently
send writes across an incompatible client/server contract. A protocol mismatch
is a degraded, fail-closed state: keep the old session alive, identify the
server generation, refuse mutating actions, and offer save→resurrect migration.

The existing typed protocol-error path already disconnects once with a clear
error instead of looping unknown messages. Full cross-version save→resurrect
and self-retirement remain part of Warden W4 and require a two-binary fixture.
Until that fixture lands, C2 is deliberately partial: no picture-in-picture,
no automatic kill, and no claim that an incompatible session was migrated.

This is a dated defer of the migration mechanism, not a defer of safety. The
fail-closed refusal and preservation rule is effective immediately.
