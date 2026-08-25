//! Native WHATWG URL parsing for the sandbox `URL` class, backed by ada —
//! the parser Node.js itself uses — so component semantics match Node by
//! construction (IDNA included, no ICU).
//!
//! The JS class in `webtypes.js` owns the object surface (prototype shape,
//! `searchParams` linkage); its state is a flat array of component strings
//! produced here. Every parse and every component setter round-trips through
//! the two callbacks below, re-parsing from `href` and returning a fresh
//! array. That keeps the native side stateless: no `Url` outlives a callback,
//! so there is no per-instance allocation, no GC coupling, and nothing stored
//! in internal fields (see `webtypes.rs` on why internal-field pointers are a
//! trap here).
//!
//! Gated end to end on the WPT URL corpus (`tests/wpt/`): the tests run every
//! case through the real `new URL(...)` inside a V8 context with the web
//! runtime installed, so the glue is what is tested, not just ada.

use ada_url::Url;

/// Number of component strings handed to JS per parse.
///
/// Order is a contract with the `U_*` constants in `webtypes.js`:
/// `[href, protocol, username, password, host, hostname, port, pathname,
/// search, hash, origin]`. The setter indices in `url_set_callback` follow
/// the same table (`origin` has no setter).
const COMPONENT_COUNT: usize = 11;

fn components<'s>(scope: &mut v8::PinScope<'s, '_>, url: &Url) -> Option<v8::Local<'s, v8::Array>> {
    let origin = url.origin();
    let parts: [&str; COMPONENT_COUNT] = [
        url.href(),
        url.protocol(),
        url.username(),
        url.password(),
        url.host(),
        url.hostname(),
        url.port(),
        url.pathname(),
        url.search(),
        url.hash(),
        origin.as_str(),
    ];
    let array = v8::Array::new(scope, COMPONENT_COUNT as i32);
    for (index, part) in parts.iter().enumerate() {
        let value = v8::String::new(scope, part)?;
        array.set_index(scope, index as u32, value.into())?;
    }
    Some(array)
}

/// Convert an argument to a Rust string.
///
/// The lossy conversion (lone surrogates become U+FFFD) is exactly the
/// USVString coercion the URL spec applies to its inputs, so nothing is lost
/// relative to spec behaviour.
fn arg_string(
    scope: &mut v8::PinScope,
    args: &v8::FunctionCallbackArguments,
    index: i32,
) -> Option<String> {
    Some(
        args.get(index)
            .to_string(scope)?
            .to_rust_string_lossy(scope),
    )
}

/// `urlParse(input, base?)` → component array, or `null` on parse failure
/// (the JS constructor turns that into the spec's `TypeError`).
pub fn url_parse_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set_null();
    let Some(input) = arg_string(scope, &args, 0) else {
        return;
    };
    let base_arg = args.get(1);
    let base = if base_arg.is_undefined() || base_arg.is_null() {
        None
    } else {
        match arg_string(scope, &args, 1) {
            Some(base) => Some(base),
            None => return,
        }
    };
    let Ok(url) = Url::parse(input.as_str(), base.as_deref()) else {
        return;
    };
    if let Some(array) = components(scope, &url) {
        rv.set(array.into());
    }
}

/// `urlSet(href, which, value)` → component array after applying one DOM
/// setter, or `null` when the operation is invalid.
///
/// DOM setter semantics: only the `href` setter fails visibly (the JS side
/// throws); every other setter is a silent no-op on invalid input, which ada
/// implements, so their component array comes back possibly unchanged. The
/// empty-string → clear mappings are the spec's "empty means remove" rules
/// for `port`, `search` and `hash`.
pub fn url_set_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set_null();
    let Some(href) = arg_string(scope, &args, 0) else {
        return;
    };
    let Some(which) = args.get(1).uint32_value(scope) else {
        return;
    };
    let Some(value) = arg_string(scope, &args, 2) else {
        return;
    };
    // `href` is always our own previous serialization, so this parse only
    // fails if the caller bypassed the JS class.
    let Ok(mut url) = Url::parse(href.as_str(), None) else {
        return;
    };
    let value = value.as_str();
    match which {
        0 => {
            if url.set_href(value).is_err() {
                return;
            }
        }
        1 => {
            let _ = url.set_protocol(value);
        }
        2 => {
            let _ = url.set_username(Some(value));
        }
        3 => {
            let _ = url.set_password(Some(value));
        }
        4 => {
            let _ = url.set_host(Some(value));
        }
        5 => {
            let _ = url.set_hostname(Some(value));
        }
        6 => {
            let _ = url.set_port(if value.is_empty() { None } else { Some(value) });
        }
        7 => {
            let _ = url.set_pathname(Some(value));
        }
        8 => url.set_search(if value.is_empty() { None } else { Some(value) }),
        9 => url.set_hash(if value.is_empty() { None } else { Some(value) }),
        _ => return,
    }
    if let Some(array) = components(scope, &url) {
        rv.set(array.into());
    }
}

#[cfg(test)]
mod tests {
    const URLTESTDATA: &str = include_str!("../tests/wpt/urltestdata.json");
    const SETTERS_TESTS: &str = include_str!("../tests/wpt/setters_tests.json");

    /// Evaluate `driver` (an expression producing a string) in a fresh
    /// context with the web runtime installed and `__corpus` holding
    /// `corpus`.
    fn run_driver(corpus: &str, driver: &str) -> String {
        crate::v8::init_platform();
        let isolate = &mut v8::Isolate::new(Default::default());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        crate::webtypes::install(scope).expect("install web globals");

        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, "__corpus").unwrap();
        let corpus_value = v8::String::new(scope, corpus).unwrap();
        global.set(scope, key.into(), corpus_value.into()).unwrap();

        let source = v8::String::new(scope, driver).unwrap();
        v8::tc_scope!(let tc, scope);
        let script = v8::Script::compile(tc, source, None).expect("compile driver");
        let Some(value) = script.run(tc) else {
            let detail = tc
                .exception()
                .and_then(|e| e.to_string(tc))
                .map(|s| s.to_rust_string_lossy(tc));
            panic!("driver threw: {detail:?}");
        };
        value.to_string(tc).unwrap().to_rust_string_lossy(tc)
    }

    fn assert_no_failures(out: &str, what: &str) {
        let (count, sample) = out.split_once('|').expect("driver output shape");
        assert_eq!(count, "0", "{what} failures (first 25 shown):\n{sample}");
    }

    /// Every corpus case runs through the real `new URL(...)`; a bare string
    /// entry is a comment. `URL.parse` and `URL.canParse` are cross-checked
    /// against the constructor on every case.
    #[test]
    fn wpt_url_corpus_passes() {
        let out = run_driver(
            URLTESTDATA,
            r#"
(() => {
  const cases = JSON.parse(globalThis.__corpus)
  const failures = []
  const COMPONENTS = ['href','protocol','username','password','host','hostname','port','pathname','search','hash']
  for (const c of cases) {
    if (typeof c === 'string') continue
    const base = c.base === null || c.base === undefined ? undefined : c.base
    let url = null
    try { url = new URL(c.input, base) } catch {}
    if (URL.canParse(c.input, base) !== (url !== null))
      failures.push(`canParse disagrees: <${c.input}> base <${base}>`)
    if ((URL.parse(c.input, base) !== null) !== (url !== null))
      failures.push(`URL.parse disagrees: <${c.input}> base <${base}>`)
    if (c.failure) {
      if (url !== null) failures.push(`expected failure: <${c.input}> base <${base}> got <${url.href}>`)
      continue
    }
    if (url === null) { failures.push(`threw: <${c.input}> base <${base}>`); continue }
    for (const k of COMPONENTS) {
      if (k in c && url[k] !== c[k])
        failures.push(`<${c.input}> base <${base}> .${k}: got <${url[k]}> want <${c[k]}>`)
    }
    if ('origin' in c && url.origin !== c.origin)
      failures.push(`<${c.input}> base <${base}> .origin: got <${url.origin}> want <${c.origin}>`)
  }
  return failures.length + '|' + failures.slice(0, 25).join('\n')
})()
"#,
        );
        assert_no_failures(&out, "WPT urltestdata");
    }

    #[test]
    fn wpt_setters_corpus_passes() {
        let out = run_driver(
            SETTERS_TESTS,
            r#"
(() => {
  const groups = JSON.parse(globalThis.__corpus)
  const failures = []
  for (const prop of Object.keys(groups)) {
    if (prop === 'comment') continue
    for (const t of groups[prop]) {
      let url
      try { url = new URL(t.href) } catch { failures.push(`parse threw: <${t.href}>`); continue }
      try { url[prop] = t.new_value } catch (e) { failures.push(`set ${prop} threw on <${t.href}>: ${e}`); continue }
      for (const k of Object.keys(t.expected)) {
        if (url[k] !== t.expected[k])
          failures.push(`<${t.href}> ${prop}=<${t.new_value}> .${k}: got <${url[k]}> want <${t.expected[k]}>`)
      }
    }
  }
  return failures.length + '|' + failures.slice(0, 25).join('\n')
})()
"#,
        );
        assert_no_failures(&out, "WPT setters_tests");
    }

    #[test]
    fn href_setter_throws_and_preserves_state() {
        let out = run_driver(
            "",
            r#"
(() => {
  const u = new URL('http://example.com/a')
  let threw = null
  try { u.href = 'not a url' } catch (e) { threw = e instanceof TypeError }
  return [threw, u.href].join('|')
})()
"#,
        );
        assert_eq!(out, "true|http://example.com/a");
    }

    #[test]
    fn search_params_stay_linked_through_native_updates() {
        let out = run_driver(
            "",
            r#"
(() => {
  const u = new URL('http://example.com/?a=1')
  u.searchParams.set('a', '2')
  const afterSet = u.href
  u.search = 'b=3'
  const paramsFollow = u.searchParams.get('b')
  u.searchParams.delete('b')
  return [afterSet, paramsFollow, u.href, u.search === ''].join('|')
})()
"#,
        );
        assert_eq!(out, "http://example.com/?a=2|3|http://example.com/|true");
    }
}
