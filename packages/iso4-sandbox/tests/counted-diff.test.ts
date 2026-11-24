import { describe, expect, test } from 'vitest'
import { diffTable, parseBase } from '../bench/counted/diff.js'

const MB = 1024 * 1024

const base = {
  sha: 'aaaaaaa1111111',
  date: '2026-11-24T00:00:00.000Z',
  rows: [
    { name: 'child.idle.pss_bytes', value: 10 * MB, unit: 'bytes' as const },
    { name: 'child.idle.threads', value: 6, unit: 'count' as const },
    { name: 'old.row', value: 3, unit: 'count' as const },
  ],
}

const head = {
  sha: 'bbbbbbb2222222',
  date: '2026-11-24T01:00:00.000Z',
  rows: [
    { name: 'child.idle.pss_bytes', value: 11 * MB, unit: 'bytes' as const },
    { name: 'child.idle.threads', value: 6, unit: 'count' as const },
    { name: 'new.row', value: 512 * MB, unit: 'bytes' as const },
  ],
}

describe('counted diff table', () => {
  test('shows changed, equal, added, and removed rows', () => {
    expect(diffTable(base, head)).toBe([
      '### Counted tier: main `aaaaaaa` vs PR `bbbbbbb`',
      '',
      '| row | main | PR | diff |',
      '| --- | ---: | ---: | ---: |',
      '| child.idle.pss_bytes | 10.00 MB | 11.00 MB | +10.0% |',
      '| child.idle.threads | 6 | 6 | 0 |',
      '| new.row | - | 512.0 MB | added |',
      '| old.row | 3 | - | removed |',
      '',
    ].join('\n'))
  })

  test('prints the PR values and a no-baseline line without a base', () => {
    expect(diffTable(undefined, head)).toBe([
      '### Counted tier: PR `bbbbbbb`',
      '',
      'no baseline yet',
      '',
      '| row | PR |',
      '| --- | ---: |',
      '| child.idle.pss_bytes | 11.00 MB |',
      '| child.idle.threads | 6 |',
      '| new.row | 512.0 MB |',
      '',
    ].join('\n'))
  })

  test.each(['{', '{"sha":"x"}', '{"rows":5}', '[]', 'null'])('treats base %s as unreadable', (text) => {
    expect(parseBase(text)).toBe('unreadable')
  })

  test('parses a well-formed base', () => {
    expect(parseBase(JSON.stringify(base))).toEqual(base)
  })

  test('prints the PR values and an unreadable line for a bad base', () => {
    expect(diffTable('unreadable', head).split('\n').slice(0, 3)).toEqual([
      '### Counted tier: PR `bbbbbbb`',
      '',
      'baseline unreadable, showing PR values only',
    ])
  })
})
