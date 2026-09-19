---
'@iso4/sandbox': patch
---

chore: drop the separate ICU data embed

The runtime carried `deno_core_icudata` because the V8 it was built against shipped without ICU data, and a locale-aware call in a sandbox aborts the whole process when that data is missing. The prebuilt V8 the runtime now pins links the full ICU data itself, so the embed was a second copy: dropping it takes 10.9 MB off the binary (54.1 MB to 43.2 MB) and changes nothing a sandbox can observe — locales, time zones, collation, segmentation and non-Gregorian calendars all still work.
