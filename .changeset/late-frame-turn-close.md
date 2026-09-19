---
'@iso4/sandbox': patch
---

fix: close the turn when an instance discards a late frame

A frame that arrives after its run concluded is discarded, and that path skipped the bookkeeping that marks the instance's turn as finished. The instance then reported full thread utilization to the registry until its next event, so the join policy routed new runs elsewhere — spawning or packing onto another instance while one with free capacity sat idle. It corrected itself on the next event; until then it cost warmth and memory.
