// A Worker's own instance of the global kernel, for the page's helpers that
// write or read the guest's filesystem while it runs (overlay-worker.js,
// persist-worker.js, drop-worker.js). The helper calls `k_fs_*` with its
// paths and bytes in a scratch "process" memory of its own; a Worker and not
// the page because the kernel's locks may wait, and the page's thread cannot.

import { kernelThread, lazyFetch, processMemories } from '../../shims/browser/js/lazy-fetch.mjs';

/**
 * Instantiate the kernel `spec` ({module, memory}: runtime.js's
 * `globalKernel()`) on the kernel stack whose top is `stackTop` (a
 * `host.takeKernelStack` slice, or a function of the exports giving one, such
 * as a `helperStackTop`), with its thread-local block at the top of it.
 * Returns {k: the exports, mem: the scratch memory}.
 */
export async function helperKernel(spec, stackTop, pages = 16) {
  // The process memory as the kernel build declares it: shared (the page's
  // build) or not; a memory import's sharedness is part of its type.
  let inst;
  let mem;
  for (const shared of [true, false]) {
    mem = new WebAssembly.Memory({ initial: pages, maximum: 65535, shared });
    try {
      inst = await WebAssembly.instantiate(spec.module, {
        env: { memory: spec.memory, ...processMemories(mem), lazy_fetch: lazyFetch(spec.memory) },
      });
      break;
    } catch (e) {
      if (!shared) throw e;
    }
  }
  const k = inst.exports;
  const top = typeof stackTop === 'function' ? stackTop(k) : stackTop;
  if (top && k.__stack_pointer) kernelThread(k, top);
  return { k, mem };
}
