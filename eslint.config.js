import schplitt from '@schplitt/eslint-config'

export default schplitt({
  ignores: ['DESIGN.md'],
}).overrideRules({
  'antfu/no-top-level-await': 'off',
})
