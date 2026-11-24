import { describe, expect, test } from 'vitest'
import { parseProc } from '../bench/counted/counted.js'

const SMAPS_ROLLUP = `55d1c0a00000-ffffd8a3f000 ---p 00000000 00:00 0                          [rollup]
Rss:               14652 kB
Pss:                4321 kB
Pss_Dirty:          1980 kB
Pss_Anon:           1900 kB
Pss_File:           2421 kB
Pss_Shmem:             0 kB
Shared_Clean:      11204 kB
Shared_Dirty:          0 kB
Private_Clean:       120 kB
Private_Dirty:      1980 kB
Referenced:        14652 kB
Anonymous:          1900 kB
KSM:                   0 kB
LazyFree:              0 kB
AnonHugePages:         0 kB
Swap:                  0 kB
SwapPss:               0 kB
Locked:                0 kB
`

const STATUS = `Name:\tiso4-v8
Umask:\t0022
State:\tS (sleeping)
Pid:\t42
VmRSS:\t   14652 kB
RssAnon:\t    1900 kB
Threads:\t9
SigQ:\t0/31071
`

describe('counted /proc parser', () => {
  test('reads private, PSS, and threads from smaps_rollup and status', () => {
    expect(parseProc(SMAPS_ROLLUP, STATUS)).toEqual({
      privateBytes: (120 + 1980) * 1024,
      pssBytes: 4321 * 1024,
      threads: 9,
    })
  })

  test('throws on text without the expected fields', () => {
    expect(() => parseProc('Rss: 1 kB\n', STATUS)).toThrow(/unexpected \/proc format/)
    expect(() => parseProc(SMAPS_ROLLUP, 'Name:\tx\n')).toThrow(/unexpected \/proc format/)
  })
})
