// Wine programs one after another in one System, with no wait between them:
// each next one starts while the last one's server may be ending (it stays
// a few seconds after its last client, then shuts its own processes down).
// A program that starts in that window and finds the server gone starts
// one of its own, and runs with that server's door, not the old one's
// (gex.mjs `#doorLive`, ntdll wineserver_relaunch).
import { bootWine } from '../lib/wine.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 2400 };

const allOk = /^(ok [^\n]*\n)+0 failures\n?$/;
const RUNS = ['sync', 'mtx', 'crtmem', 'apc', 'tls', 'cxx', 'fiber', 'qpcmono', 'sync', 'mtx', 'crtmem', 'apc', 'tls', 'cxx'];

export default async function (t) {
  const w = await bootWine(t);
  let n = 0;
  for (const name of RUNS) {
    const r = await t.time(`${++n} ${name}`, () => w.run(`${name}.exe`, [], { timeout: 120000 }))
      .catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
    t.check(`${n} ${name}: exit 0, its output`, r.code === 0 && allOk.test(r.stdout), `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-200))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-3).join(' / '))}`);
    if (r.code === -1) break;
  }
  await w.sys.shutdown();
}
