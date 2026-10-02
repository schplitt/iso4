---
"@iso4/sandbox": patch
---

fix(sandbox): stop dispatching queued bridge calls once a run is aborted

Host globals are no longer invoked for bridge calls the sandbox sent before the abort but the client had not yet dispatched. Handlers already running still finish.
