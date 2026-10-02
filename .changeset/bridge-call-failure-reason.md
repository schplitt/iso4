---
"@iso4/sandbox": patch
---

feat(sandbox): say why a bridge call failed in `bridgeCalls`

`BridgeCallEntry` loses `blocked` and gains `reason` (`blocked` | `error` | `unanswered` | `dropped`) whenever `ok` is false. Replace `entry.blocked` checks with `entry.reason === 'blocked'`.
