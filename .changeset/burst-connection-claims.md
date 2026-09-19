---
'@iso4/sandbox': patch
---

fix: stop a cold burst from opening a connection per run

Runs arriving together on an empty pool each found no connection with room and opened their own, so a burst of 64 opened 61 connections where the steady-state packing needs 16 — two runtime threads each, given back only by the idle reaper 30 s later. A caller turned away now claims a place on a connection already being opened when one still has room, and opens its own only when they are all spoken for: a connection seats `RUNS_PER_CONNECTION` runs, one of them its opener. Measured on the real child: 64 concurrent runs open 16 connections instead of 61, 16 runs open 4 instead of 13.
