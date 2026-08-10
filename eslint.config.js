import schplitt from '@schplitt/eslint-config'

export default schplitt({
  // internal/ holds gitignored local notes — keep the linter's markdown
  // formatter away from them.
  // native/ is the Rust crate; only Rust and one JS asset live there. Ignoring
  // the whole tree would leave `src/webtypes.js` — the sandbox runtime, and the
  // largest hand-written file in the project — unlinted, so it stays in scope
  // and only the build output is excluded.
  ignores: ['DESIGN.md', 'internal/**', 'native/**/target/**'],
}).overrideRules({
  'antfu/no-top-level-await': 'off',
}).append({
  // The sandbox web runtime. Linted (it is the largest hand-written file here)
  // but two rules genuinely do not apply:
  //   - the file *is* one expression — it evaluates to a factory the Rust side
  //     calls with the native shells, so there is nothing to assign it to;
  //   - byte-level sequences like `out[p++] = 0xEF; out[p++] = 0xBF` read better
  //     on one line than spread over three.
  files: ['native/v8-runtime/src/webtypes.js'],
  rules: {
    'no-unused-expressions': 'off',
    'style/max-statements-per-line': 'off',
  },
})
