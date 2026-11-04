// A program's output read as it ends (page/runtime programs.js `read`): a
// read the kernel answered "empty" while the program was still writing, and
// whose answer came after its exit, is not its output's end. With a kernel
// that answers so on cue: the first read finds the pipe empty and the
// program ends meanwhile, and what it wrote is there for the next read.
import { Programs } from '../../page/runtime/src/programs.js';

export const meta = { hosts: ['node', 'browser'], images: [], timeout: 30 };

const EAGAIN = -11;

export default async function (t) {
  const { eq } = t;
  let ended = false;
  let reads = 0;
  const pipe = [new TextEncoder().encode('last words\n')];
  const kernel = {
    seq: () => 0,
    changed: () => new Promise((r) => setTimeout(r, 10)),
    watch: async () => {},
    kick: () => {},
    async call(f) {
      if (f === 'k_page_spawn') {
        // pagespawn.rs: pid 2, no pty, stdin 3, stdout 4, stderr 5, its
        // three queues, no image span, no exec record or descriptors.
        const b = new Uint8Array(64);
        const v = new DataView(b.buffer);
        [2, -1, 3, 4, 5, 6, 7, 8, 0, 0, 0].forEach((x, i) => v.setInt32(i * 4, x, true));
        return { r: 0, out: [b] };
      }
      if (f === 'k_pipe_read') {
        reads++;
        // The first read: empty, and the program writes and ends before
        // the page has its answer.
        if (reads === 1) { ended = true; return { r: EAGAIN, out: [] }; }
        const b = pipe.shift();
        return b ? { r: b.length, out: [b] } : { r: EAGAIN, out: [] };
      }
      return { r: 0, out: [] };
    },
    calls: async (list) => list.map(() => ({ r: 0 })),
  };
  const pr = new Programs(kernel, null, {});
  await pr.spawn({ h: 1, argv: ['p'] });
  const got = await pr.read(1, 1, () => ended);
  eq('what it wrote before it ended', got && new TextDecoder().decode(got), 'last words\n');
  eq('then its end', await pr.read(1, 1, () => ended), undefined);
}
