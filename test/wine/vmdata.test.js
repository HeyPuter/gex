// A Wine run's VmData stays under 8 GiB. Every WebAssembly memory counts its
// whole size towards its process's data segment, and Chrome sets
// RLIMIT_DATA on the renderer (8 to 64 GiB by the machine's RAM), so a page
// over 8 GiB dies of it on small machines. rpccom.exe runs COM through RPC,
// so it brings up the most of Wine's processes (the server, services,
// winedevice, plugplay, rpcss). The renderer's VmData is sampled every
// 500 ms while it runs; the peak is the bound, the samples' largest RSS is
// said with it. Browser only: Node's own reservations for wasm memories put
// its VmData far past what Chrome's renderer counts (42 GiB for this run),
// and no limit applies there.
import { bootWine, exitStats, exitLineOf } from '../lib/wine.mjs';

export const meta = { hosts: ['browser'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 900, alone: true };

const MAX_GIB = 8;
const KIB_PER_GIB = 1 << 20;

export default async function (t) {
  const w = await bootWine(t, { debug: { log: 'debug' } });
  let peak = null;
  let rss = 0;
  let done = false;
  const sampler = (async () => {
    while (!done) {
      const s = await t.procStatus();
      if (s) { peak = Math.max(peak ?? 0, s.data); rss = Math.max(rss, s.rss); }
      await new Promise((r) => setTimeout(r, 500));
    }
  })();
  const r = await t.time('rpccom', () => w.run('rpccom.exe'))
    .catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  done = true;
  await sampler;
  t.check('rpccom: exit 0', r.code === 0, `exit ${r.code}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  t.check('rpccom: its exit line', await exitLineOf(t, /rpccom\.exe/), 'none in 60 s');
  await w.sys.shutdown();
  // What each process's memory grew to (the runtime's exit lines), each once.
  const mem = new Map();
  for (const s of await exitStats(t)) mem.set(s.pid.split(':')[0], { cmd: s.cmd.split(' ').slice(0, 2).join(' '), mib: s.memoryMib });
  t.say(`process memories MiB: ${[...mem].map(([pid, m]) => `${pid} ${m.cmd.split('/').pop()} ${m.mib}`).join(', ')}`);
  if (peak === null) return t.skip('no renderer of its own to read (other jobs\' pages ran): run it alone');
  t.record('peak VmData GiB', Number((peak / KIB_PER_GIB).toFixed(2)));
  t.record('peak VmRSS GiB', Number((rss / KIB_PER_GIB).toFixed(2)));
  t.known(`peak VmData ${(peak / KIB_PER_GIB).toFixed(2)} GiB (VmRSS up to ${(rss / KIB_PER_GIB).toFixed(2)} GiB), want at most ${MAX_GIB}`, peak <= MAX_GIB * KIB_PER_GIB, '',
    'every 64-bit Wine process grows its memory to about 2.1 GiB, so eight processes reach about 17 GiB');
}
