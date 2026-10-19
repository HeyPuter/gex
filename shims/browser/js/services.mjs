// The Worker that is never a guest: a kernel instance that never runs guest
// code, so it is always awake to receive the things that are only callbacks --
// the terminal, the WebSocket -- and put them into the kernel's shared memory
// where a parked guest will find them. A guest Worker parked inside wasm is not
// running its event loop, so a `postMessage` cannot reach it until it wakes.
//
// It holds a process memory of its own only because the kernel imports one and
// `k_pipe_write` reads bytes from there; it is a staging buffer, nothing more.
import { lazyFetch, processMemories } from './lazy-fetch.mjs';
import { single, sched } from './single.mjs';
let k = null;
let stage = null; // Uint8Array over this Worker's dummy process memory
const STAGE_AT = 0x1000;

const post = (m) => self.postMessage(m);

self.onmessage = async (e) => {
  const msg = e.data;
  if (msg?.kind === 'init') {
    const mod =
      msg.kernel.module instanceof WebAssembly.Module
        ? msg.kernel.module
        : await WebAssembly.compile(msg.kernel.module);
    // The kernel imports `env.process`, so it must be supplied even though
    // nothing here is a guest.
    const process = new WebAssembly.Memory({
      initial: 0x07000000 / 65536,
      maximum: 0xffff0000 / 65536,
      shared: true,
    });
    const inst = await WebAssembly.instantiate(mod, {
      env: { memory: msg.kernel.memory, ...processMemories(process), lazy_fetch: lazyFetch(msg.kernel.memory) },
    });
    k = inst.exports;
    // A stack of this Worker's own: each instance's `__stack_pointer` starts at
    // the same address in the shared memory.
    if (msg.kernelStack) k.__stack_pointer.value = msg.kernelStack | 0;
    stage = new Uint8Array(process.buffer);
    post({ kind: 'ready' });
    return;
  }
  if (!k) return;
  if (msg?.kind === 'write') {
    // Into the kernel, whose pipe notifies whoever waits on its other end.
    const bytes = msg.bytes;
    let off = 0;
    while (off < bytes.length) {
      const n = Math.min(bytes.length - off, 0x10000);
      stage.set(bytes.subarray(off, off + n), STAGE_AT);
      const wrote = k.k_pipe_write(msg.pipe, BigInt(STAGE_AT), n);
      if (wrote <= 0) break;
      off += wrote;
    }
    if (single.on) sched.checkSlots();
    post({ kind: 'wrote', id: msg.id, n: off });
    return;
  }
  if (msg?.kind === 'close') {
    k.k_pipe_close_end(msg.pipe, 1);
    if (single.on) sched.checkSlots();
    return;
  }
};
