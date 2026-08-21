---
"@iso4/sandbox": minor
---

fix: correlate `Result` frames with their run, fail closed on desync (#73)

A decode error on guest-controlled bridge arguments escaped the frame loop
uncaught, so the connection returned to the pool while the runtime was still
mid-run, and nothing compared the `runId` on a `Result` frame against the one
sent. The next run drawn from that slot received the *previous* run's exports,
stdout, stderr and bridge telemetry, while its own code never executed. In a
multi-tenant embedding that is cross-tenant disclosure, and it was reachable
from ordinary guest code with one host bridge global wired in.

Every `Result` is now matched against the run in flight before it is accepted.
The runtime already echoed the `Run` frame's `runId` onto every completion path,
so this needed no protocol or runtime change.

Alongside it, the paths that let a desynced connection stay poolable are closed:
an undecodable `BridgeCall` is answered with an error response rather than
letting the throw escape (`callId` precedes the guest value blob, so the call
stays answerable and the run completes normally instead of costing a pool slot);
anything else escaping the frame loop tears the connection down; frame types with
no place in the run protocol are rejected instead of skipped, and `precompile()`
/ `stats()` now tear down rather than leaving a misaligned stream in the pool;
and `usable` consults the socket while the socket's `error`/`end`/`close`
handlers mark the client broken, so a peer-closed connection is replaced instead
of recycled forever.

**New error code `ERR_PROTOCOL_DESYNC`.** Host-detected, never reported by the
runtime. A run that fails with it never reached the isolate, so its telemetry
fields are zero rather than partial, and the connection is destroyed and
replaced before the result is returned. It should be unreachable; seeing it is
worth reporting.
