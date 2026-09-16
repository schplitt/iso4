---
'@iso4/sandbox': patch
---

fix: an instance in a long turn no longer reads as idle to the join policy

Instances publish thread utilization once per closed window, and windows close only between turns. An instance deep in a turn that outlasts a window therefore still advertised whatever its last closed window measured — usually idle — so the router kept sending work to the one thread already busy. A turn that has been open for a whole window now reports as saturated.
