// /dev/gex-gl, by name as well as by open: AArch64, x86-64 and i386
// programs, and a forked child of each. With a GL ring (GEX_GL_CHANNEL=1,
// what `gl: 'compositor'` gives every process) the node must stat as a
// character device, pass access(R_OK|W_OK) and open. Without one it must not
// exist at all. The device the host adds was openable and listed in
// `ls /dev`, but stat and access said ENOENT, so `ls /dev/gex-gl` and any
// probe by name found no GPU.
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 360 };

const ON = 'parent: stat chr, access ok, open ok, fstat chr\nchild: stat chr, access ok, open ok, fstat chr';
const OFF = 'parent: stat no, access no, open no, fstat no\nchild: stat no, access no, open no, fstat no';

export default async function (t) {
  const g = guest(t);
  const seed = g.pub('seed{infix}.shared.wasm');
  g.pub('gex_browser{infix}.shared.wasm');
  // On x86 the i386 program too, built for the 32-bit target.
  const progs = g.arch === 'x86'
    ? { x64: g.cc('gldev.c'), i386: g.cc({ name: 'gldev-i386', code: (await import('node:fs')).readFileSync(new URL('../fixtures/programs/gldev.c', import.meta.url), 'utf8') }, { flags: ['-target', 'x86-linux-musl'] }) }
    : { a64: g.cc('gldev.c') };
  for (const [what, prog] of Object.entries(progs)) {
    for (const gl of ['1', '0']) {
      const r = await g.run(seed, prog, [], { env: { GEX_GL_CHANNEL: gl, GEX_SHARED: '1' } });
      t.eq(`${what}, ${gl === '1' ? 'GL ring: stat, access and open, parent and child' : 'no GL ring: no node'}`, r.guest, gl === '1' ? ON : OFF, tail(r.stderr, 3));
    }
  }
}
