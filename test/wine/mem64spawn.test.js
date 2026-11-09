// A 64-bit program the page starts (System.run, never an execve) gets its
// memory flavour as one Wine execs does: an image only a 64-bit memory can
// load (no relocations, based above the lowered memory's code space) goes
// on in one, at its base; so does one the environment asks for
// (GEX_MEM64=1), placed top down above 4 GiB; a plain relocatable one stays
// on the default flavour. The server is up first, so the loader never
// execs itself again (which would examine the program anyway). In the
// memory64 flavour [2 GiB, 4 GiB), gex's own space, shows as reserved, and
// the available virtual memory leaves it out.
import { bootWine, WINE } from '../lib/wine.mjs';

export const meta = { hosts: ['browser'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 1800 };

const parse = (out) => {
  const m = /^exeplace base (?:0x)?([0-9a-fA-F]+) avail (\d+) total (\d+) free (\d+) largest (\d+) mid ([0-9a-f]+)/m.exec(out);
  return m && { base: BigInt(`0x${m[1]}`), avail: BigInt(m[2]), total: BigInt(m[3]), free: BigInt(m[4]), largest: BigInt(m[5]), mid: parseInt(m[6], 16) };
};
const G4 = 1n << 32n, BASE = 0x140000000n, MEM_RESERVE = 0x2000;

export default async function (t) {
  const w = await bootWine(t, { wow64: true });
  await w.put('exeplace.exe');
  await w.put('exeplace-fixed.exe');
  const server = await w.startServer();
  const go = async (exe, env = {}) => {
    const r = await t.timeout(w.sys.run([WINE, `C:\\${exe}`], { env: { ...w.env, ...env }, cwd: '/pfx/drive_c' }), 300000, exe);
    const out = r.stdout.replace(/\r/g, '');
    const p = parse(out);
    t.say(`${exe} ${JSON.stringify(env)}: ${p ? `base 0x${p.base.toString(16)} avail ${p.avail} total ${p.total} free ${p.free} largest ${p.largest} at 2.25 GiB ${p.mid.toString(16)}` : `code ${r.code}`}`);
    return [p, `code ${r.code} stdout ${JSON.stringify(out.slice(-300))} stderr ${JSON.stringify(r.stderr.slice(-600))}`];
  };
  // memory64: gex's space reserved and out of the available
  const m64 = (p) => p && p.mid === MEM_RESERVE && p.avail + (2n << 30n) <= p.total;

  const [plain, pd] = await go('exeplace.exe');
  t.check('a relocatable program stays on the default flavour (below 4 GiB)', plain && plain.base < G4, pd);
  const [fixed, fd] = await go('exeplace-fixed.exe');
  t.check('a program only a 64-bit memory can load gets one, at its base, and the available leaves out gex\'s space', m64(fixed) && fixed.base === BASE, fd);
  const [asked, ad] = await go('exeplace.exe', { GEX_MEM64: '1' });
  t.check('a program the environment asks a 64-bit memory for gets one, top down above 4 GiB', m64(asked) && asked.base >= G4 && asked.base > BASE, ad);
  server.kill();
  await w.sys.shutdown();
}
