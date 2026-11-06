// A program that crashes is reported (System's `crash` event): one a crash
// signal ends (SIGSEGV, SIGABRT), one that traps (rt-probe trap: wasm's
// unreachable, SIGILL), and one that is not pid 1's child. An exit, and a
// signal that is no crash (SIGTERM), are not.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 180 };

export default async function (t) {
  const { check, eq, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  const crashes = [];
  sys.addEventListener('crash', (e) => crashes.push({ pid: e.pid, name: e.name, reason: e.reason, code: e.code, signal: e.signal }));
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin:/usr/bin', HOME: '/root' };
  // The next crash, within 5 s of the program's end.
  const next = async (what) => {
    for (let i = 0; i < 100 && !crashes.length; i++) await new Promise((r) => setTimeout(r, 50));
    check(`${what}: reported`, crashes.length > 0, 'no crash event');
    return crashes.shift() ?? {};
  };

  const segv = await sys.run(['sh', '-c', 'rt-probe kill $$ 11; echo survived'], { env });
  eq('SIGSEGV: the program ends of it', [segv.code, segv.signal], [139, 'SIGSEGV']);
  const c1 = await next('SIGSEGV');
  eq('SIGSEGV: the report', [c1.name, c1.signal, c1.code, c1.reason], ['sh', 11, 139, 'SIGSEGV']);

  const trap = await sys.run(['rt-probe', 'trap'], { env });
  check('a trap: the program ends of a signal', trap.code >= 128, `exit ${trap.code}`);
  const c2 = await next('a trap');
  eq('a trap: its name', c2.name, 'rt-probe');
  check('a trap: a crash signal', [4, 11].includes(c2.signal), JSON.stringify(c2));

  // a grandchild of pid 1's: its parent goes on
  const deep = await sys.run(['sh', '-c', 'sh -c \'rt-probe kill $$ 6; echo survived\'; echo after'], { env });
  check('a grandchild\'s crash: its parent goes on', deep.stdout.endsWith('after\n') && !deep.stdout.includes('survived'), JSON.stringify(deep.stdout));
  const c3 = await next('a grandchild\'s crash');
  eq('a grandchild\'s crash: the report', [c3.signal, c3.code], [6, 134]);
  check('a grandchild\'s crash: its pid', c3.pid > 1, JSON.stringify(c3));

  await sys.run(['sh', '-c', 'exit 3'], { env });
  await sys.run(['sh', '-c', 'rt-probe kill $$ 15; echo survived'], { env });
  await new Promise((r) => setTimeout(r, 500));
  eq('an exit and SIGTERM: no reports', crashes, []);
  await sys.shutdown();
}
