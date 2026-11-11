// A program that patches its own code one write at a time, its section made
// writable and executable around each write and the code doing it in that
// section (fixtures/native/textpatch: an old MinGW runtime's pseudo-
// relocations): the patched function's last patch is seen, natively and on
// the page; and on the page the code doing the patching is not translated
// again for each patch (translations made while its page was writable and
// executable check their bytes, and are kept when it is made so again),
// which said "came to translation 64 times" and ran each patch through a
// fresh translation of the loop.
import { native, trim, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

const N = 300;
const SUM = `textpatch before 1234567 after ${N - 1}`;
const lines = (out) => out.split('\n').filter((l) => l.startsWith('textpatch ')).join('\n');

export default async function (t) {
  const n = native(t);
  const d = n.dir('textpatch');
  await n.cc(['-O1', '-static', '-o', `${d}/textpatch`, n.src('textpatch/textpatch.c')]);

  const r = await n.gexc(['run', `${d}/textpatch`, String(N)], { timeout: 180000 });
  if (trim(r.stdout) === 'textpatch skip') t.skip('the compiler laid getter out otherwise');
  t.eq('native: the last patch is seen', trim(r.stdout), SUM, tail(r.stderr));

  const g = n.g;
  for (const p of ['gex_browser.x86.shared.wasm', 'gex_kernel.x86.wasm']) g.pub(p);
  // Warm: `getter` and the loop translated before the first patch; cold:
  // only the loop.
  for (const how of ['warm', 'cold']) {
    const p = await g.run(n.seed(), `${d}/textpatch`, how === 'cold' ? [String(N), 'cold'] : [String(N)], { files: { '/g/textpatch': `${d}/textpatch` }, argv0: '/g/textpatch', env: { GEX_SHARED: '1' }, timeout: 300000 });
    t.eq(`page, ${how}: the last patch is seen`, lines(p.stdout), SUM, tail(p.stderr));
    t.check(`page, ${how}: the patching code is not translated again a patch`, !/came to translation \d+ times|retired \d+ times|LIVELOCK/.test(p.stdout + p.stderr), tail(p.stderr));
  }
  n.clean();
}
