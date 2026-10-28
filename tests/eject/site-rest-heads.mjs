#!/usr/bin/env node
// A rest-bundle file's head (web/site-bundle.mjs `restPaths`, `placeHeads`;
// eject-site.mjs --rest-paths "PATH HEAD [tail]"): the file leaves the bundle
// for the rest bundle whole, and its first HEAD MiB (and with `tail` its last,
// partial MiB) go back into the bundle tree at its path, with its time; a
// file no longer than its head gets none. The kernel's side (a bundled copy
// seeds the lazy file's chunks) is gex-kernel's
// `a_bundled_head_seeds_a_lazy_file` and `a_bundled_head_and_tail_seed_a_lazy_file`.
//
//   node tests/eject/site-rest-heads.mjs

import { mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, utimesSync, writeFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { placeHeads, restPaths, splitTree } from '../../web/site-bundle.mjs';

const root = mkdtempSync(join(tmpdir(), 'gex-rest-heads-'));
let failed = 0;
const check = (what, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failed++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}${ok ? '' : `\n  got  ${JSON.stringify(got)}\n  want ${JSON.stringify(want)}`}`);
};
const MiB = 1048576;
try {
  check('lines: a path, a path with its head, spaces in a path',
    restPaths(['/g/music.dat', '/g/music.dat 2', '/g/My Music/bgm.dat 1', '/g/arc.dat 1 tail']),
    [['/g/music.dat', 0, false], ['/g/music.dat', 2 * MiB, false], ['/g/My Music/bgm.dat', MiB, false], ['/g/arc.dat', MiB, true]]);
  const x = join(root, 'x');
  mkdirSync(join(x, 'g'), { recursive: true });
  const big = Buffer.alloc(3 * MiB + 5);
  for (let i = 0; i < big.length; i++) big[i] = (i * 7) % 251;
  writeFileSync(join(x, 'g/bgm.dat'), big);
  writeFileSync(join(x, 'g/small.dat'), Buffer.alloc(1000, 1));
  writeFileSync(join(x, 'g/arc.dat'), big);
  writeFileSync(join(x, 'g/even.dat'), big.subarray(0, 3 * MiB));
  writeFileSync(join(x, 'g/exe'), 'MZ');
  for (const f of ['g/bgm.dat', 'g/small.dat', 'g/arc.dat', 'g/even.dat']) utimesSync(join(x, f), 1061035200, 1061035200);
  const store = join(root, 'store');
  const lazy = splitTree(x, new Set(['/g/exe']), store);
  check('the data files are lazy', lazy.map((f) => f[0]).sort(), ['/g/arc.dat', '/g/bgm.dat', '/g/even.dat', '/g/small.dat']);
  const heads = new Map([['/g/bgm.dat', { head: MiB, tail: false }], ['/g/small.dat', { head: MiB, tail: false }], ['/g/gone', { head: MiB, tail: false }],
    ['/g/arc.dat', { head: MiB, tail: true }], ['/g/even.dat', { head: MiB, tail: true }]]);
  placeHeads(x, lazy, store, heads);
  check('the head is the file\'s first MiB', readFileSync(join(x, 'g/bgm.dat')).equals(big.subarray(0, MiB)), true);
  check('with the file\'s time', Math.trunc(statSync(join(x, 'g/bgm.dat')).mtimeMs / 1000), 1061035200);
  check('a file no longer than its head has none', existsSync(join(x, 'g/small.dat')), false);
  check('with a tail: the first MiB, then the last, partial one',
    readFileSync(join(x, 'g/arc.dat')).equals(Buffer.concat([big.subarray(0, MiB), big.subarray(3 * MiB)])), true);
  check('a file of whole MiB gets its head only', readFileSync(join(x, 'g/even.dat')).length, MiB);
  check('the heads left, as the copies\' lengths', [...heads.entries()].sort(), [['/g/arc.dat', MiB + 5], ['/g/bgm.dat', MiB], ['/g/even.dat', MiB]]);
  const whole = lazy.find((f) => f[0] === '/g/bgm.dat');
  check('the lazy copy stays whole', [whole[1], readFileSync(join(store, whole[2])).equals(big)], [big.length, true]);
} finally {
  rmSync(root, { recursive: true, force: true });
}
console.log(failed ? `site-rest-heads: ${failed} failed` : 'site-rest-heads: ok');
process.exit(failed ? 1 : 0);
