// A batch of kernel calls as the page asks for them (kernel.js `calls`), run
// on a kernel instance: the kernel-call Worker's (kernel-worker.js) and the
// single-thread build's system Worker's (kernel-single-worker.js). The
// arguments' forms are kernel-worker.js's.

const enc = new TextEncoder();

/** Room in the scratch memory `mem`, from its start, for `n` bytes. */
function ensure(mem, n) {
  const have = mem.buffer.byteLength;
  if (n > have) mem.grow(Math.ceil((n - have) / 65536));
}

/** A batch's arguments laid out in `mem` and its results read back. */
function batch(k, mem) {
  let top = 0;
  const place = (n) => {
    const at = top;
    top = (top + n + 15) & ~15;
    ensure(mem, top + 16);
    return at;
  };
  const results = [];
  const transfer = [];
  return {
    /** One call's function and arguments, and where its outputs go. */
    prep({ f, a = [] }) {
      const fn = k[f];
      if (typeof fn !== 'function') throw new Error(`the kernel has no ${f}`);
      const args = [];
      const outs = [];
      for (const x of a) {
        if (typeof x === 'number' || typeof x === 'bigint') {
          args.push(x);
        } else if (x.in !== undefined || x.ptr !== undefined) {
          const v = x.in ?? x.ptr;
          const b = typeof v === 'string' ? enc.encode(v) : v;
          const at = place(b.length);
          new Uint8Array(mem.buffer).set(b, at);
          args.push(BigInt(at));
          if (x.in !== undefined) args.push(b.length);
        } else if (x.out !== undefined || x.outPtr !== undefined) {
          const cap = x.out ?? x.outPtr;
          const at = place(cap);
          args.push(BigInt(at));
          if (x.out !== undefined) args.push(cap);
          outs.push({ at, cap, all: x.outPtr !== undefined });
        } else if (x.ref !== undefined) {
          const r = results[x.ref].r;
          args.push(x.big ? BigInt(r) : Number(r));
        } else {
          throw new Error(`${f}: an argument the kernel Worker cannot pass: ${JSON.stringify(x)}`);
        }
      }
      return { f, fn, args, outs };
    },
    /** Its result in; whether the batch stops here. */
    finish({ outs }, { stop }, r) {
      const out = outs.map(({ at, cap, all }) => {
        const n = all ? cap : typeof r === 'number' && r >= 0 && r <= cap ? r : cap;
        const b = new Uint8Array(mem.buffer).slice(at, at + n);
        transfer.push(b.buffer);
        return b;
      });
      results.push({ r, out });
      return !!stop && (typeof r === 'bigint' ? r < 0n : r < 0);
    },
    done: () => ({ value: results, transfer }),
  };
}

/** Run `list` on the kernel's exports `k`, with `mem` its scratch process
 *  memory; every argument is laid out after the last, so what a call wrote
 *  is still there when the batch is done. */
/** Whether the batches read a file's bytes (`k_fs_read`): a lazy file's may
 *  come from a rest bundle, whose frames need the zstd decoder
 *  (lazy-fetch.mjs `lazyReady`), loaded before the read, as it cannot be
 *  waited for in the middle of one. */
export function readsFiles(groups) {
  return groups.some((list) => list.some((c) => c.f === 'k_fs_read'));
}

export function runCalls(k, mem, list) {
  const b = batch(k, mem);
  for (const c of list) {
    const p = b.prep(c);
    if (b.finish(p, c, p.fn(...p.args))) break;
  }
  return b.done();
}

/** `runCalls`, each call made by `invoke(f, args)`, which may answer with a
 *  promise: the single-thread build's, where a call may suspend (a page
 *  directory's answer, page-fs.mjs `pageFsSuspending`). */
export async function runCallsAsync(k, mem, list, invoke) {
  const b = batch(k, mem);
  for (const c of list) {
    const p = b.prep(c);
    if (b.finish(p, c, await invoke(p.f, p.args))) break;
  }
  return b.done();
}
