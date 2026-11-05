// One process reaching another's memory, natively (gexc run):
//   vmpeer       ReadProcessMemory/WriteProcessMemory on a child (reads,
//                writes, a write over its code, a partial copy across a
//                decommitted page, a read-only page, an exited child);
//                "quick" makes its first access while the child is still
//                starting, five times;
//   dbgflag      a debuggee's PEB BeingDebugged, created under a debugger
//                and attached to;
//   winedbg      the system debugger attaching to a process whose exception
//                nothing handles;
//   remotestart  threads started in another process (with a native i386
//                side, "native32": its native function's value refused).
// On the page's shim (no process_vm_readv/writev) only vmpeer and vmpeer
// quick, whose copies the target process makes itself.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 2400 };

export default async function (t) {
  const w = wow64(t);
  const exes = {};
  for (const p of ['vmpeer', 'dbgflag', 'remotestart']) exes[`${p}.exe`] = await w.mingw(p);
  const native = (args, timeout, name) => w.native(args, { files: exes, timeout: timeout * 1000, name });
  const vmpeer = (leg, r) => {
    const done = /^vmpeer done, 0 failed/m.test(r.out) && /^child exit 55/m.test(r.out);
    const hung = !/^FAIL|^vmpeer done/m.test(r.out);
    const last = r.out.split('\n').filter((l) => l.startsWith('ok')).pop()?.slice(5) ?? '';
    t.check(leg, done, hung ? `hung after '${last}'; ${w.detail(r, 3)}` : w.detail(r));
  };
  vmpeer('native, vmpeer', await native(['C:\\vmpeer.exe'], 150, 'vmpeer'));
  for (let i = 1; i <= 5; i++) vmpeer(`native, vmpeer quick ${i}`, await native(['C:\\vmpeer.exe', 'quick'], 90, `quick-${i}`));
  const d = await native(['C:\\dbgflag.exe'], 300, 'dbgflag');
  t.check('native, dbgflag', /^dbgflag done, 0 failed/m.test(d.out), w.detail(d));
  const g = await native(['C:\\vmpeer.exe', 'crash'], 180, 'winedbg');
  t.check('native, winedbg attaches', /^WineDbg attached to pid/m.test(g.out) && !/Can't attach|gex failed/.test(g.out), w.detail(g));
  const r = await native(['C:\\remotestart.exe', ...(w.i386Native ? ['native32'] : [])], 180, 'remotestart');
  t.check('native, remotestart', /^remotestart done, 0 failed/m.test(r.out), w.detail(r));
  const files = { '/t/vmpeer.exe': exes['vmpeer.exe'] };
  vmpeer('page, vmpeer', await w.page(['/t/vmpeer.exe'], { files, timeout: 200000 }));
  vmpeer('page, vmpeer quick', await w.page(['/t/vmpeer.exe', 'quick'], { files, timeout: 200000, name: 'page-quick' }));
  w.clean();
}
