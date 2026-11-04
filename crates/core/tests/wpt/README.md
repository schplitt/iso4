# Vendored web-platform-tests URL corpus

`urltestdata.json` and `setters_tests.json` are copied verbatim from
[web-platform-tests](https://github.com/web-platform-tests/wpt)
(`url/resources/`), revision `181476aa16e8b28a07698bef3a0275fa53dd22e5`
(2026-07-05).

They gate the sandbox `URL` class end to end: `url.rs` tests run every case
through `new URL(...)` inside a real V8 context with the web runtime
installed, so the native glue is what is tested, not just the parser library.

To update: copy the two files from a newer WPT revision and bump the revision
recorded here.

WPT test materials are licensed under the
[3-Clause BSD License](https://github.com/web-platform-tests/wpt/blob/master/LICENSE.md).
