// A panic in the page's shim always reaches the console: a plain one, one
// that panics again inside the panic hook (std drops that one's message; the
// first must stay, and the trap's report must say a second one happened),
// and one with a 4 KiB message (its first 1 KiB also as a line of its own).
// The shim panics on purpose through GEX_DEBUG_PANIC (gex_debug_panic).
import { guest } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 300 };

const CASES = {
  plain: ['gex: panic in the shim: panicked at', 'gex debug panic: plain'],
  nested: ['gex debug panic: outer', 'it panicked again inside the panic report'],
  // The head line: the first 1 KiB, then the length; the whole line after it.
  long: ['bytes; the whole message follows)', 'gex debug panic: long xxxx'],
};

export default async function (t) {
  const g = guest(t);
  g.pub('gex_browser{infix}.shared.wasm');
  g.pub('gex_kernel{infix}.wasm');
  const prog = g.cc({ name: 'nothing', code: 'int main(void) { return 0; }\n' });
  const seed = g.seed();
  for (const [mode, wants] of Object.entries(CASES)) {
    const r = await g.run(seed, prog, [], { env: { GEX_SHARED: '1', GEX_DEBUG_PANIC: mode } });
    const all = r.stdout + r.stderr;
    const missing = wants.find((w) => !all.includes(w));
    t.check(`panic ${mode}`, !missing, `no "${missing}" in the output`);
    if (mode === 'long') {
      const n = all.split('gex debug panic: long').length - 1;
      t.check('the long message as its head and whole', n >= 2, `it came ${n} times`);
    }
  }
}
