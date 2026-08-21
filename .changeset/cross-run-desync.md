---
"@iso4/sandbox": minor
---

fix: connection integrity and result correlation (#73)

Hardening pass on the host side of the IPC connection. The theme is that a
connection must never be reused once there is any doubt about its frame
alignment, and a result must never be accepted without proof it belongs to the
run that asked for it.

**Results are now correlated with their run.** Every `Result` frame is matched
against the `runId` the host sent before it is accepted. Previously the `runId`
was decoded and ignored, so a connection that had lost frame alignment could
hand a run a result assembled for an earlier one. The runtime already echoed the
`runId` on every completion path, so this needed no protocol change.

**New error code `ERR_PROTOCOL_DESYNC`.** Host-detected, never reported by the
runtime. A run that fails with it never reached the isolate, so its telemetry
fields are zero rather than partial, and the connection is destroyed and replaced
before the result is returned. It should be unreachable in practice; seeing it is
worth reporting.

**The frame loop now fails closed.** A decode failure on an inbound bridge-call
payload is answered with an error response, so the run completes normally rather
than the failure escaping and costing a connection. Anything else escaping the
loop, and any frame type with no place in the run protocol, tears the connection
down instead of leaving a misaligned stream in the pool. `precompile()` and
`stats()` do the same on their unexpected-frame path.

**A dead connection is no longer recycled.** `usable` consults the socket, and
the socket's `error`/`end`/`close` handlers mark the connection broken, so one
closed by the peer is replaced rather than handed to caller after caller.

**`maxIsolates` is now a capacity rather than a fixed set of connections.** Idle
connections are reused, a dead one is dropped, and the freed capacity is filled
on demand. A failed reconnect therefore fails one run and gives the capacity
back, where it previously reduced concurrency for the lifetime of the process.
Queued callers also observe their `AbortSignal` now; it was previously checked
only before acquiring, so a signal firing during the wait had no effect.

**Error payloads are bounded.** An error's message and stack are clamped to 64
KiB each (on a UTF-8 boundary, noting how much was cut) and an oversized
own-properties blob is dropped, with the error's `code` and `name` always
preserved. Previously a large enough failure could not be framed at all, which
cost the connection. Runs whose result still cannot be framed now report
`ERR_INTERNAL` instead of losing the connection.

Also fixed: the per-connection run counter wrapped into negative values after
2³¹ runs, which broke every subsequent run on that connection; a malformed frame
length prefix could surface as an uncaught exception on the host event loop; and
a bridge response too large to encode was silently discarded, leaving the
sandbox waiting on a reply that never came.

The `maxBridgeCallBytes` documentation is corrected: it claimed the framing layer
enforced the response limit automatically via `memoryMb`, but the binding ceiling
is the host's own 64 MiB frame limit.
