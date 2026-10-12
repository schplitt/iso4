/**
 * Name rules behind the public `@iso4/sandbox/utils` entry. Imports nothing,
 * so loading it never touches the client or the native binary.
 */

/**
 * Every global name a fresh sandbox defines: the JS built-ins of the bundled
 * V8 plus the runtime's own (`console`, `waitUntil`, timers, web classes).
 * A host global under one of these names replaces the built-in, unless the
 * runtime reserves the name. Names the grammar
 * forbids outright are in {@link RESERVED_WORDS}.
 */
export const SANDBOX_GLOBALS: ReadonlySet<string> = new Set([
  'AggregateError',
  'Array',
  'ArrayBuffer',
  'AsyncDisposableStack',
  'Atomics',
  'BigInt',
  'BigInt64Array',
  'BigUint64Array',
  'Boolean',
  'DataView',
  'Date',
  'DisposableStack',
  'Error',
  'EvalError',
  'FinalizationRegistry',
  'Float16Array',
  'Float32Array',
  'Float64Array',
  'Function',
  'Headers',
  'Infinity',
  'Int16Array',
  'Int32Array',
  'Int8Array',
  'Intl',
  'Iterator',
  'JSON',
  'Map',
  'Math',
  'NaN',
  'Number',
  'Object',
  'Promise',
  'Proxy',
  'RangeError',
  'ReferenceError',
  'Reflect',
  'RegExp',
  'Request',
  'Response',
  'Set',
  'String',
  'SuppressedError',
  'Symbol',
  'SyntaxError',
  'Temporal',
  'TextDecoder',
  'TextEncoder',
  'TypeError',
  'URIError',
  'URL',
  'URLSearchParams',
  'Uint16Array',
  'Uint32Array',
  'Uint8Array',
  'Uint8ClampedArray',
  'WeakMap',
  'WeakRef',
  'WeakSet',
  'clearTimeout',
  'console',
  'decodeURI',
  'decodeURIComponent',
  'encodeURI',
  'encodeURIComponent',
  'escape',
  'eval',
  'globalThis',
  'isFinite',
  'isNaN',
  'parseFloat',
  'parseInt',
  'setTimeout',
  'undefined',
  'unescape',
  'waitUntil',
])

/**
 * Names that cannot be a binding in module source. A host-module key may not
 * use one, except `default`.
 */
export const RESERVED_WORDS: ReadonlySet<string> = new Set([
  'break',
  'case',
  'catch',
  'class',
  'const',
  'continue',
  'debugger',
  'default',
  'delete',
  'do',
  'else',
  'export',
  'extends',
  'finally',
  'for',
  'function',
  'if',
  'import',
  'in',
  'instanceof',
  'new',
  'return',
  'super',
  'switch',
  'this',
  'throw',
  'try',
  'typeof',
  'var',
  'void',
  'while',
  'with',
  'yield',
  'let',
  'static',
  'enum',
  'await',
  'implements',
  'package',
  'protected',
  'interface',
  'private',
  'public',
  'null',
  'true',
  'false',
  'eval',
  'arguments',
])

/**
 * A JavaScript identifier name, Unicode included: ID_Start/ID_Continue plus
 * `$`, `_`, ZWNJ and ZWJ. A name that matches and is not in
 * {@link RESERVED_WORDS} can be a binding in module source, so it is a valid
 * host-module export name.
 */
export const IDENTIFIER_NAME: RegExp = /^[\p{ID_Start}$_][\p{ID_Continue}$\u200C\u200D]*$/u

/**
 * Names the runtime owns; a host global under one fails with
 * `ERR_RESERVED_NAME`. The client checks them before sending and the runtime
 * again; this mirrors `RESERVED_GLOBAL_NAMES` in `v8.rs`. `utils.test.ts`
 * checks every name here and every {@link SANDBOX_GLOBALS} name against the
 * runtime.
 */
export const RUNTIME_OWNED_GLOBALS: ReadonlySet<string> = new Set([
  'console',
  'waitUntil',
  'setTimeout',
  'clearTimeout',
  'Headers',
  'Request',
  'Response',
  'TextEncoder',
  'TextDecoder',
  'URL',
  'URLSearchParams',
  'NaN',
  'undefined',
  'Infinity',
  '__proto__',
])

/**
 * Whether `name` can be a key of a host-module object in `imports`: an
 * identifier per {@link IDENTIFIER_NAME} that is not in
 * {@link RESERVED_WORDS}, or `default`, which maps to the module's default
 * export. `default` cannot be written as `export const default` in generated source.
 * @param name candidate name
 */
export function isValidExportName(name: string): boolean {
  return name === 'default' || (IDENTIFIER_NAME.test(name) && !RESERVED_WORDS.has(name))
}

/**
 * Whether `prepare()` and `run()` accept `name` as a host global name: any
 * well-formed string that is not a runtime-owned name such as `console` or
 * `Response`. A name that is not an identifier is reachable only as
 * `globalThis[name]`. Names starting with `__iso4_` are iso4's internal
 * namespace (the dispatcher, shim handlers): a host global there may be
 * silently overwritten or dropped, and this does not flag it.
 * @param name candidate host global name
 */
export function isValidGlobalName(name: string): boolean {
  return name.isWellFormed() && !RUNTIME_OWNED_GLOBALS.has(name)
}
