---
"@iso4/sandbox": patch
---

fix(native): name the run in the fatal OOM line when V8 runs out of memory on a GC worker thread

Such OOMs used to print `with no run executing on this thread`. Between runs it now reads `with no run executing in this isolate`, and an OOM with no current isolate now prints the line with `outside any isolate`.
