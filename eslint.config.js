import schplitt from '@schplitt/eslint-config'

export default schplitt({
  // internal/ holds gitignored local notes — keep the linter's markdown
  // formatter away from them.
  ignores: ['DESIGN.md', 'internal/**'],
}).overrideRules({
  'antfu/no-top-level-await': 'off',
})
