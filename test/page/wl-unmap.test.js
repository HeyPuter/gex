// A Wayland window's destroy reaches the page while its process waits
// (fixtures/programs/wl-unmap-pump.c): a raw client maps a toplevel,
// destroys it and blocks on a futex for 3 s with nothing else running. A
// process's compositor runs on its own scheduler turns, and the first turn of
// a wait made without the kernel lock (`park_free`) skipped the pump, so the
// destroy was published only when the wait ended (a closed menu stayed drawn
// for seconds and took the next click). And a client that keeps running
// after the destroy (`spin`: calls that never wait) reaches no turn at all:
// its requests are pumped when it sends them. Under node with the headless
// compositor (GEX_WL_STUB, GEX_WL_STUB_LOG): the publish without the window
// must come within 1 s of the destroy, not at the wake.
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

/** The first publish with no window after the map, against the destroy and
 *  the wake: an error, or `{ after, wake }` in ms after the destroy. */
function verdict(text) {
  const at = (re) => Number(re.exec(text)?.[1] ?? NaN);
  const m = at(/^mapped t=([\d.]+)/m);
  const d = at(/^destroyed t=([\d.]+)/m);
  const w = at(/^woke t=([\d.]+)/m);
  if (!(m && d && w)) return { error: 'no map, destroy or wake printed' };
  // After the map (the window was published before `mapped`): a pump on the
  // destroy's own send publishes before `destroyed`.
  const gone = [...text.matchAll(/\[wlstub\] publish t=([\d.]+) surfaces=\d+ windows=(\d+)/g)]
    .filter((x) => Number(x[2]) === 0).map((x) => Number(x[1])).filter((x) => x >= m);
  if (!gone.length) return { error: 'no publish without the window' };
  return { after: Math.min(...gone) - d, wake: w - d };
}

export default async function (t) {
  const g = guest(t);
  const seed = g.pub('seed.x86.shared.wasm');
  const prog = g.cc('wl-unmap-pump.c', { glibc: true, flags: ['-static'] });
  for (const single of [false, true]) {
    for (const mode of ['wait', 'spin']) {
      const what = `${single ? 'sp' : 'x86'}, ${mode}`;
      const r = await g.run(seed, prog, [mode], { env: { GEX_WL_STUB: '1', GEX_WL_STUB_LOG: '1', GEX_SHARED: '1' }, single, timeout: 60000 });
      const v = verdict(`${r.stdout}\n${r.stderr}`);
      t.check(`the destroy published ${v.error ?? `${v.after.toFixed(0)} ms after it (the wake ${v.wake.toFixed(0)} ms)`} (${what})`,
        r.code === 0 && !v.error && v.after < 1000, `exit ${r.code}: ${tail(r.stdout + r.stderr, 6)}`);
    }
  }
}
