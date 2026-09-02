---
"@iso4/sandbox": patch
---

fix: stream frames must carry a real run id (#127)

An unattributed stream frame (run id 0) now tears the connection down as a
protocol desync instead of being matched to a run by stream id — with
several runs on one connection that guess could credit or cancel the wrong
run's body stream. The production runtime always tags stream frames.
