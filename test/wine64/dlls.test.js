// 64-bit Wine's DLLs from console programs under gexc, with the door
// (fixtures/wine/src): an RPC call into services.exe, a cross-apartment COM
// call and DispCallFunc (rpccom); crypt32 verifying a signature over
// authenticated attributes as encoded, not in DER order (authattrs);
// DirectWrite's glyph bounds, alpha texture and outline from its Unix side
// (dwrite); and 24 DLLs of a thousand functions each loaded, run and
// unloaded one after another, as a plugin scan does (dllcycle, bigdll.c's
// plugNN).
import { wine64 } from '../lib/winerun.mjs';
import { expect } from '../fixtures/wine/bigdll.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 2400 };

const allOk = /(ok [^\n]*\n)+0 failures/;

export default async function (t) {
  const w = wine64(t);
  await w.nt({ name: 'rpccom', want: allOk, exe: 'rpccom.exe' });
  await w.nt({ name: 'authattrs', want: allOk, exe: 'authattrs.exe' });
  if (t.check('dwrite: the loader has dwrite.so', w.loaderHas('dwrite.so'))) await w.nt({ name: 'dwrite', want: allOk, exe: 'dwrite.exe' });
  const plugs = Array.from({ length: 24 }, (_, i) => w.prog(`plug${String(i).padStart(2, '0')}.dll`));
  await w.nt({ name: 'dllcycle', want: new RegExp(`dllcycle 24 x ${expect(1000, 24)}`), exe: 'dllcycle.exe', args: ['24', '1'], extra: plugs });
  w.clean();
}
