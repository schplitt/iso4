import schplitt from '@schplitt/eslint-config'

export default schplitt({
  // internal/ holds gitignored local notes — keep the linter's markdown
  // formatter away from them.
  // native/ is the Rust crate. `src/webtypes.js` lives there as an
  // `include_str!` asset — sandbox runtime source compiled into the binary,
  // not part of any TS package — so the TS ruleset does not apply to it.
  ignores: ['DESIGN.md', 'internal/**', 'native/**'],
}).overrideRules({
  'antfu/no-top-level-await': 'off',
})
