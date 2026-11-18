---
"@iso4/sandbox": minor
---

feat(native): the runtime writes one fatal line to a `crash` file next to its socket and to stderr when it dies

The line is `key=value` (`[iso4-v8] fatal cause=oom-heap phase=oneoff run=1 msg="..."`), replaces the old out of memory sentence, and also covers V8 fatal errors and SIGSEGV, SIGTRAP, SIGABRT, SIGBUS and SIGILL, after which the runtime dies by the same signal.
