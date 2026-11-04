#!/usr/bin/env node
// A dist's rest-only files (web/site-bundle.mjs `REST_ONLY`: a side module
// the loader dlopens only when a program needs it, Wine's winegstreamer.so):
// out of the read set whatever a recorded run read, and out of a bundle not
// split; then in the compressed rest bundle (`buildRest`), whose frames hold
// each file's bytes at its entry's offset.
//
//   node tests/eject/site-rest-only.mjs

import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { buildRest, hasRestOnly, readLessRestOnly, restOnly, splitTree } from '../../web/site-bundle.mjs';

const root = mkdtempSync(join(tmpdir(), 'gex-rest-only-'));
let failed = 0;
const check = (what, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failed++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}${ok ? '' : `\n  got  ${JSON.stringify(got)}\n  want ${JSON.stringify(want)}`}`);
};
const tree = (name) => {
  const x = join(root, name);
  mkdirSync(join(x, 'w64/lib/wine/x86_64-unix'), { recursive: true });
  writeFileSync(join(x, 'w64/lib/wine/x86_64-unix/wine'), 'loader');
  writeFileSync(join(x, 'w64/lib/wine/x86_64-unix/gex-rest-only'), '# side modules\nwinegstreamer.so\n\nlibSDL2-2.0.so.0\n');
  writeFileSync(join(x, 'w64/lib/wine/x86_64-unix/winegstreamer.so'), side);
  writeFileSync(join(x, 'w64/lib/wine/x86_64-unix/libSDL2-2.0.so.0'), side2);
  writeFileSync(join(x, 'w64/other.dat'), 'other');
  return x;
};
const side = Buffer.alloc(3 * 1048576 + 77);
for (let i = 0; i < side.length; i++) side[i] = (i * 13) % 241;
const side2 = Buffer.from('a second side module');
try {
  const a = tree('a');
  check('the lists name files of their own directory', [...restOnly(a)].sort(),
    ['/w64/lib/wine/x86_64-unix/libSDL2-2.0.so.0', '/w64/lib/wine/x86_64-unix/winegstreamer.so']);
  execFileSync('tar', ['-cf', join(root, 'a.tar'), '-C', a, '.']);
  execFileSync('tar', ['-cf', join(root, 'none.tar'), '-C', a, 'w64/other.dat']);
  check('a tar with a list has one', [hasRestOnly(join(root, 'a.tar')), hasRestOnly(join(root, 'none.tar'))], [true, false]);
  // A recorded run that read the side module: it leaves the read set.
  const read = readLessRestOnly(a, new Set(['/w64/lib/wine/x86_64-unix/wine', '/w64/lib/wine/x86_64-unix/winegstreamer.so']));
  check('read by the run, still out of the read set', [...read].sort(), ['/w64/lib/wine/x86_64-unix/wine']);
  // A bundle not split: every other file stays in it.
  const b = tree('b');
  const all = readLessRestOnly(b, null);
  check('no read set: every file but them', [...all].sort(),
    ['/w64/lib/wine/x86_64-unix/gex-rest-only', '/w64/lib/wine/x86_64-unix/wine', '/w64/other.dat']);
  const store = join(root, 'store');
  const lazy = splitTree(b, all, store);
  check('the split leaves them lazy', lazy.map((f) => f[0]).sort(),
    ['/w64/lib/wine/x86_64-unix/libSDL2-2.0.so.0', '/w64/lib/wine/x86_64-unix/winegstreamer.so']);
  const bin = join(root, 'site/lazy/rest.bin');
  const r = buildRest(lazy, store, { bin, work: join(root, 'work'), frame: 1048576, zlev: '-3' });
  check('entries in the rest bundle', r.files.map((f) => [f[0], f[1], /^rest\.bin#\d+$/.test(f[2])]).sort(),
    [['/w64/lib/wine/x86_64-unix/libSDL2-2.0.so.0', side2.length, true], ['/w64/lib/wine/x86_64-unix/winegstreamer.so', side.length, true]]);
  // The frames, decoded one by one, are the tar; each file at its offset.
  const idx = JSON.parse(readFileSync(`${bin}.json`, 'utf8'));
  const z = readFileSync(bin);
  const tar = Buffer.concat(idx.frames.map(([o, n]) => execFileSync('zstd', ['-d', '-q', '-c'], { input: z.subarray(o, o + n) })));
  check('the frames: the tar, frame by frame', [tar.length, idx.size, idx.frames.length > 1], [idx.size, idx.size, true]);
  for (const [path, size, rel] of r.files) {
    const off = Number(rel.split('#')[1]);
    check(`${path.split('/').pop()}: its bytes at its offset`, tar.subarray(off, off + size).equals(path.endsWith('.so') ? side : side2), true);
  }
  // A file of half a frame or more has frames of its own: the small
  // one's frames are none of the big one's.
  const frames = (rel, size) => { const off = Number(rel.split('#')[1]); return [Math.floor(off / idx.frame), Math.floor((off + size - 1) / idx.frame), off % idx.frame]; };
  const [big, small] = [r.files.find((f) => f[0].endsWith('.so')), r.files.find((f) => !f[0].endsWith('.so'))];
  const [b0, b1, bAt] = frames(big[2], big[1]), [s0, s1] = frames(small[2], small[1]);
  check('a big file starts its frame', bAt, 0);
  check('a small file shares no frame with a big one', s1 < b0 || s0 > b1, true);
  check('compressed', z.length < side.length, true);
} finally {
  rmSync(root, { recursive: true, force: true });
}
console.log(failed ? `site-rest-only: ${failed} failed` : 'site-rest-only: ok');
process.exit(failed ? 1 : 0);
