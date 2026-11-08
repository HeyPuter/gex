// Each Wine install picks the prefix made for it (wine/prefix-key.sh, which
// wine/dist.sh stamps its prefixes with): two installs of one Wine checkout,
// one with winegstreamer and one without, share wine.inf's mtime, so a
// prefix found by its .update-timestamp alone fits both. Side by side, with
// a gst prefix and a nogst one stamped by --add, and an older prefix with the
// same .update-timestamp but no .gex-dists:
//   - the gst install, and its twin (.m64 modules beside), pick the gst prefix;
//   - the nogst install picks the nogst prefix;
//   - the older prefix is picked by neither, and --add refuses it;
//   - an install of another checkout (another wine.inf mtime) picks none;
//   - --add again leaves one line per install.
import { existsSync, mkdirSync, readFileSync, rmSync, utimesSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { exec, tail, ROOT } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 60 };

export default async function (t) {
  const P = join(ROOT, 'wine/prefix-key.sh');
  if (!existsSync(P)) t.skip(`no ${P}`);
  const W = join(t.workDir, 'native', `prefix-key-${process.pid}`);
  rmSync(W, { recursive: true, force: true });
  const STAMP = 1794151435;
  const dist = (name, extra, mtime = STAMP) => {
    const d = join(W, name);
    for (const a of ['x86_64', 'i386']) {
      const l = join(d, `lib/wine/${a}-windows`);
      mkdirSync(l, { recursive: true });
      for (const f of ['kernel32.dll', 'user32.dll', 'winewayland.drv', 'ntdll.so', 'libuser32.a', 'user32.dll.wasm', ...extra]) writeFileSync(join(l, f), '');
    }
    mkdirSync(join(d, 'share/wine'), { recursive: true });
    writeFileSync(join(d, 'share/wine/wine.inf'), '; wine.inf\n');
    utimesSync(join(d, 'share/wine/wine.inf'), mtime, mtime);
    return d;
  };
  const gst = dist('dist-gst', ['winegstreamer.dll']);
  const twin = dist('dist-gst-twin', ['winegstreamer.dll', 'user32.dll.wasm.m64']);
  const nogst = dist('dist-nogst', []);
  const other = dist('dist-other', [], STAMP + 60);
  const pfx = (name, stamped) => {
    const p = join(W, name, 'pfx');
    mkdirSync(p, { recursive: true });
    writeFileSync(join(p, '.update-timestamp'), `${STAMP}\n`);
    if (stamped) writeFileSync(join(p, '.gex-dists'), '');
    return p;
  };
  const pGst = pfx('pfs64-x-gex-gst', true);
  const pNogst = pfx('pfs64-x-gex-nogst', true);
  const pOld = pfx('pfs64-x-gex-old', false);
  const run = (...a) => exec(['bash', P, ...a], { timeout: 30000 });
  for (const [p, d] of [[pGst, gst], [pGst, twin], [pNogst, nogst], [pGst, gst]]) {
    const r = await run('--add', p, d);
    t.check(`--add ${p.split('/').at(-2)} ${d.split('/').at(-1)}`, r.code === 0, tail(r.stdout + r.stderr));
  }
  const old = await run('--add', pOld, nogst);
  t.check('--add refuses a prefix without .gex-dists', old.code !== 0 && !existsSync(join(pOld, '.gex-dists')), tail(old.stdout + old.stderr));
  const all = [pGst, pNogst, pOld];
  const picks = async (d) => {
    const r = await run(d, ...all);
    return r.code === 0 ? r.stdout.split('\n').filter(Boolean) : [`exit ${r.code}: ${tail(r.stderr)}`];
  };
  t.eq('the gst install picks the gst prefix', await picks(gst), [pGst]);
  t.eq("the gst install's twin picks the gst prefix", await picks(twin), [pGst]);
  t.eq('the nogst install picks the nogst prefix', await picks(nogst), [pNogst]);
  t.eq("another checkout's install picks none", await picks(other), []);
  const keys = async (d) => (await run(d)).stdout.trim();
  t.check('gst and nogst keys differ', (await keys(gst)) !== (await keys(nogst)));
  t.eq("the gst prefix keeps one key (its twin's is the same)", readFileSync(join(pGst, '.gex-dists'), 'utf8').split('\n').filter(Boolean), [await keys(gst)]);
  rmSync(W, { recursive: true, force: true });
}
