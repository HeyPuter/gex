// node unit-bases.mjs WORK EXE...: run each of win-progs' x64 programs EXE
// with 64-bit Wine through the runtime API (images `wine64` and `win-progs`
// in WORK) and an empty unit cache. The runtime then says, on this process's
// output, each image it had no function-tier unit for, and its base; the
// `wine64-units` image reads that and builds them.
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

process.env.GEX_NODE_LAZY = '1';
const [work, ...exes] = process.argv.slice(2);
const { System } = await import('../../../web/runtime/src/index.js');
const { Filesystem } = await import('../../../web/runtime/src/fs.js');
const empty = mkdtempSync(join(tmpdir(), 'gex-units-'));
const sys = new System({ baseUrl: pathToFileURL(join(import.meta.dirname, '../../../web/public/')).href, units: pathToFileURL(empty).href, debug: { arch: 'x86' } });
const fs = new Filesystem(sys);
fs.addTar(pathToFileURL(join(work, 'wine64.tar')).href, { lazy: pathToFileURL(join(work, 'wine64-lazy.json')).href });
fs.addTar(pathToFileURL(join(work, 'win-progs.tar')).href, { at: '/progs' });
await sys.boot();
const env = { WINEPREFIX: '/pfx', WINEDEBUG: 'err+all,fixme-all', WINEGEX_DOOR: '1' };
let bad = 0;
for (const exe of exes) {
  await fs.writeFile(`/pfx/drive_c/${exe}`, await fs.readFile(`/progs/x64/${exe}`));
  const r = await sys.run(['/wine64/lib/wine/x86_64-unix/wine', `C:\\${exe}`], { env, cwd: '/pfx/drive_c' });
  if (!r.stdout) bad++;
}
await sys.shutdown();
rmSync(empty, { recursive: true, force: true });
process.stdout.write(sys.state.console, () => process.exit(bad ? 1 : 0));
