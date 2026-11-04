// A program that started and fell over (page/runtime programs.js `abandon`,
// called when its start's promise fails): its Worker has said its end (a
// signal, gex_abandon) already, and the page's 127 for "never started" must
// not overwrite it before pid 1's reaper collects it. With a kernel whose
// table is the test's: the end is said, then the page abandons, then the
// reaper looks, the order a busy page meets; and a program that never
// started is still collected as 127.
import { Programs } from '../../page/runtime/src/programs.js';

export const meta = { hosts: ['node', 'browser'], images: [], timeout: 30 };

const SIGKILL = 9;

export default async function (t) {
  const { eq } = t;
  // pid -> wait status, as the kernel's table keeps it: a later exit sets it
  // again (proc.rs `exit`).
  const table = new Map();
  const kernel = {
    calls: async (list) => list.map(({ f, a }) => {
      if (f === 'k_exit') table.set(a[0], a[1] << 8);
      // pid in the high 32 bits, wait status in the low 32; zero for none.
      if (f === 'k_wait') return { r: table.has(a[1]) ? (BigInt(a[1]) << 32n) | BigInt(table.get(a[1])) : 0n };
      return { r: 0 };
    }),
  };
  const pr = new Programs(kernel, null, {});
  table.set(2, SIGKILL);
  await pr.abandon(2);
  eq('a program that fell over keeps its own end', table.get(2), SIGKILL);
  await pr.abandon(3);
  eq('one that never started ends 127', table.get(3), 127 << 8);
}
