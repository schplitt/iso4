---
"@iso4/sandbox": patch
---

feat: a 128 MB host reserve, reconstructible capacity refusals, and a startup capacity log

The Node-host reserve behind the default memory budget and the admission
line drops from 256 MB to 128 MB, so small containers admit isolates they
previously refused. `ERR_CAPACITY` messages now state the whole calculation
(the usage+cap sum, the container limit, the reserve, and the headroom still
admissible), and the runtime logs its resolved capacity picture once at
startup.
