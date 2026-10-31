// A wineserver ends with processes of its own still alive (services,
// winedevice and the rest that wineboot starts, some of them in a door call
// or waiting for the door's lock as it goes): they end with it, as on Linux
// when their pipes close, and the next program starts a server of its own
// and runs. The host poisons a dead server's door, so a call waiting for its
// lock gives up and goes by the pipe (host.mjs, gex.mjs `#doorLockSlow`).
import { bootWine } from '../lib/wine.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 900 };

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export default async function (t) {
  const w = await bootWine(t);
  const say = (r) => `exit ${r.code}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`;
  const fail = (e) => ({ code: -1, stdout: '', stderr: String(e.message) });
  const r1 = await t.time('first', () => w.run('hello_args.exe', ['a', 'b c'], { timeout: 240000 })).catch(fail);
  t.check('the first program: exit 42', r1.code === 42, say(r1));
  // Its server, which stays a few seconds after its last client and then
  // shuts its own processes down, and those processes.
  const alive = w.sys.describe().pids;
  const t0 = Date.now();
  while (w.sys.describe().pids.length && Date.now() - t0 < 90000) await sleep(250);
  const left = w.sys.describe().pids;
  t.record('server and its processes ended ms', Date.now() - t0);
  t.check(`the server and its processes end (${alive.length} alive after the program)`, alive.length > 0 && left.length === 0,
    alive.length ? `still alive after 90 s: ${JSON.stringify(left)} (of ${JSON.stringify(alive)})` : 'nothing was alive after the program');
  const r2 = await t.time('after', () => w.run('hello_args.exe', ['a', 'b c'], { timeout: 240000 })).catch(fail);
  t.check('a program after it, with a server of its own: exit 42', r2.code === 42, say(r2));
  await w.sys.shutdown();
}
