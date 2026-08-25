---
"@iso4/sandbox": patch
---

fix: WHATWG-compliant URL in the sandbox

The sandbox `URL` is now backed natively by the ada parser (the one Node.js
uses) and passes the WPT URL test suite, including IDNA, relative resolution
and non-special schemes. `URL.parse`, `URL.canParse` and the previously
missing component setters are now available.
