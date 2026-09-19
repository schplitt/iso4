---
'@iso4/sandbox': patch
---

fix: keep `waitUntil` grace phases from opening connections, and reap idle ones

Runs in their grace phase counted toward a connection's run cap until their `RunComplete`, so a steady stream of epilogues opened connection after connection — two runtime threads each, never closed. Grace runs are now excluded from the cap (their frames are routed by run id on whichever connection they already hold), and a connection that has carried nothing for 30 s is closed; one is always kept. Measured on a 16-slot sandbox with 100 ms grace work: 385 open connections before, 13 after.
