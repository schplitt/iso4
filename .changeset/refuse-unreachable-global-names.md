---
"@iso4/sandbox": minor
---

feat(sandbox): refuse host global names sandbox code cannot reference by bare name with the new `ERR_INVALID_GLOBAL_NAME`.
Names like `my-tool` or `class` used to install unreachably; host-module export names now also accept Unicode identifiers such as `café`.
