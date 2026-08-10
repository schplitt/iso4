---
"@iso4/sandbox": minor
---

Add a sandbox web runtime and make `Request`/`Response`/`Headers` cross the boundary as real instances.

The sandbox now provides `Headers`, `Request`, `Response`, `TextEncoder`, `TextDecoder`, `URL` and `URLSearchParams`. It previously had none of these — a bare isolate exposed only `console`.

These three classes serialize as V8 host objects rather than flattening to plain objects, so a `Response` returned from sandbox code arrives host-side as a real `Response`, at any nesting depth, with duplicate `set-cookie` headers intact. In the other direction a `Request` or `Response` can be passed in as a data global or returned from a bridge handler; there it is recognised at the top level of the value only.

New error code `ERR_TYPE_NOT_SERIALIZABLE`, reported when a value cannot cross in that position — a stream body, an unimplemented type, or a host type nested too deeply on the way in. Streams are deliberately unsupported: the wire format reserves tags for them so support can be added without a format change.

Wire format is specified in `docs/protocol.md` §4.4.
