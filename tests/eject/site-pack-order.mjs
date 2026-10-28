#!/usr/bin/env node
// The site bundle's file order (web/site-bundle.mjs `pack`): with a recorded
// run's first reads, those files come first in that order, an identical
// file next to its first-read twin, then the rest in tree order.
//
//   node tests/eject/site-pack-order.mjs

import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pack } from '../../web/site-bundle.mjs';

const root = mkdtempSync(join(tmpdir(), 'gex-pack-order-'));
let failed = 0;
const check = (what, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failed++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}${ok ? '' : `\n  got  ${JSON.stringify(got)}\n  want ${JSON.stringify(want)}`}`);
};
try {
  const x = join(root, 'x');
  for (const d of ['a', 'b']) mkdirSync(join(x, d), { recursive: true });
  const files = { 'a/one': '1', 'a/two': '2', 'a/twin': '3', 'b/three': '3', 'b/four': '4' };
  for (const [p, b] of Object.entries(files)) writeFileSync(join(x, p), b);
  const tree = { x, readOnly: [], dirTimes: [] };
  const list = (dedupe, order) => {
    const tar = join(root, 'out.tar');
    pack(tree, tar, root, dedupe, order);
    return execFileSync('tar', ['-tf', tar], { encoding: 'utf8' }).split('\n').filter((l) => l && !l.endsWith('/') && !['a', 'b'].includes(l));
  };
  const order = ['/b/four', '/a/two', '/b/three'];
  const plain = list(false, order);
  check('first reads first, in read order', plain.slice(0, 3), ['b/four', 'a/two', 'b/three']);
  check('the rest after, every file once', [...plain].sort(), Object.keys(files).sort());
  const dd = list(true, order);
  check('an identical file goes with its first-read twin', dd.slice(0, 2), ['b/four', 'a/two']);
  check('the twins together at the twin\'s rank', dd.slice(2, 4).sort(), ['a/twin', 'b/three']);
  check('the unread file last', dd.slice(4), ['a/one']);
  check('no order: every file', [...list(true, null)].sort(), Object.keys(files).sort());
} finally {
  rmSync(root, { recursive: true, force: true });
}
console.log(failed ? `site-pack-order: ${failed} failed` : 'site-pack-order: ok');
process.exit(failed ? 1 : 0);
