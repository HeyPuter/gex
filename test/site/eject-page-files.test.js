// site/build-eject-page.sh carries everything page/public/gex has that an
// ejected page loads: the x86 shared shim, kernel and seed, and the X server
// (gex/xwayland/, and gex/xwayland-st/ when built). Without the X server a
// site's X11 program stopped at "x11 not available". A .wasm is compared with
// its stripped form (build-eject-page.sh strips names and debug sections
// unless GEX_KEEP_NAMES=1); a directory (the builds' licenses/) file by file.

import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, rmSync, statSync } from 'node:fs';
import { join } from 'node:path';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

const ROOT = join(import.meta.dirname, '..', '..');

export default async function (t) {
  const pub = join(ROOT, 'page/public/gex');
  if (!existsSync(join(pub, 'gex_browser.x86.shared.wasm'))) t.skip('no page/public/gex x86 shim (make assets)');
  if (!existsSync(join(ROOT, 'page/node_modules/.bin/vite'))) t.skip('no page/node_modules (npm ci in page/)');
  const keep = !!process.env.GEX_KEEP_NAMES;
  if (!keep && spawnSync('wasm-tools', ['--version']).status !== 0) t.skip('no wasm-tools (cargo install wasm-tools)');
  const out = join(t.workDir, 'eject-page-files');
  rmSync(out, { recursive: true, force: true });
  const r = spawnSync('sh', [join(ROOT, 'site/build-eject-page.sh'), join(out, 'page')], { cwd: ROOT, encoding: 'utf8', maxBuffer: 64 << 20 });
  if (!t.check('build-eject-page.sh', r.status === 0, (r.stdout + r.stderr).trim().split('\n').slice(-5).join(' / '))) return;
  const page = join(out, 'page');
  for (const f of ['app.mjs', 'gex/gex_browser.x86.shared.wasm', 'gex/gex_kernel.x86.wasm', 'gex/seed.x86.shared.wasm']) t.check(`page has ${f}`, existsSync(join(page, f)));
  const same = (a, b) => existsSync(b) && Buffer.compare(readFileSync(a), readFileSync(b)) === 0;
  const tree = (a, b) => readdirSync(a).every((n) => (statSync(join(a, n)).isDirectory() ? tree(join(a, n), join(b, n)) : same(join(a, n), join(b, n))));
  let n = 0;
  for (const d of ['xwayland', 'xwayland-st']) {
    if (!existsSync(join(pub, d))) continue;
    for (const name of readdirSync(join(pub, d))) {
      const f = join(pub, d, name);
      const g = join(page, 'gex', d, name);
      let ok;
      if (statSync(f).isDirectory()) ok = existsSync(g) && tree(f, g);
      else if (name.endsWith('.wasm') && !keep) {
        const cmp = join(out, 'cmp.wasm');
        execFileSync('wasm-tools', ['strip', '-d', '^name$', '-d', '^\\.debug_.*', '-d', '^external_debug_info$', '-d', '^sourceMappingURL$', f, '-o', cmp]);
        ok = same(cmp, g);
      } else ok = same(f, g);
      t.check(`gex/${d}/${name} is the build's`, ok, 'missing or stale');
      n++;
    }
  }
  if (!existsSync(join(pub, 'xwayland'))) t.say('no page/public/gex/xwayland here: the X server is not checked (make page)');
  t.say(`${n} X server files checked`);
  rmSync(out, { recursive: true, force: true });
}
