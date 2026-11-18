---
"@iso4/sandbox": patch
---

fix(native): name the run in the fatal OOM line when V8 runs out of memory on a GC worker thread

The line now names the run of the isolate that ran out of memory, also when V8 reports it from a worker thread, where it used to name no run.
