// A new thread's provider stack in a split page process (?winesplit=1) whose
// guest address space is full (provfill.c): the space filled, given back only
// until Wine can make a thread, and the thread's Worker must still get its 1
// MiB provider stack, from the shim's heap above 2 GiB (cgen.rs
// gex_provider_stack), not the full guest space ("no memory for a provider
// stack"). On the page's shim, as a WoW64 process of 64-bit Wine.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('provfill', { flags: ['-Wall'] });
  const r = await w.page(['/t/provfill.exe'], { files: { '/t/provfill.exe': exe }, env: { GEX_WINE32_SPLIT: '1' },
    wineEnv: { WINEDEBUG: 'err+all,fixme-all,-virtual' } });
  for (const l of r.out.split('\n').filter((l) => /^(ok  |FAIL)/.test(l))) t.say(l);
  t.check('page split: provfill ok', /^provfill ok/m.test(r.out), w.detail(r));
  w.clean();
}
