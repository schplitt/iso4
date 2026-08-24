---
"@iso4/sandbox": patch
---

fix: keep resource limits armed through result serialization

Serializing a run's result executes guest getters, so it now stays under the
run's CPU, wall and memory budgets; serialization time counts against
`cpuTimeMs`/`wallTimeMs`.
