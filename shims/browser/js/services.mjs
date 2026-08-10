// The Worker that is never a guest.
//
// A guest Worker that parks inside wasm is not running its event loop, so
// anything delivered to it by `postMessage` cannot arrive until it wakes --
// which is why every `poll` currently hands the stack back to JavaScript
// instead of sleeping, and why `apt-get update` burns 217 seconds of CPU
// against 115 seconds of clock spinning on the pipes between apt and its
// methods.
//
// The fix is that no wait may depend on a message reaching a *guest* Worker.
// Everything a guest waits for has to land in the kernel's shared memory and
// be announced with `k_wake`, exactly as one guest writing a pipe already is.
// Something still has to own the things that are only callbacks -- the
// terminal, the WebSocket -- and that is this: a Worker with a kernel
// instance that never runs guest code, so it is always awake to receive them
// and always able to put them where a parked guest will find them.
//
// It holds a process memory of its own purely because the kernel imports one
// and `k_pipe_write` reads its bytes from there. Nothing guest-related lives
// in it; it is a staging buffer.
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
    // The kernel declares `env.process` with the minimum a real guest needs,
    // so the import has to match even though nothing here is a guest.
    const process = new WebAssembly.Memory({ initial: 0x05000000 / 65536 });
    const inst = await WebAssembly.instantiate(mod, {
      env: { memory: msg.kernel.memory, process },
    });
    k = inst.exports;
    // A stack of this Worker's own, for the same reason every other Worker
    // needs one: a wasm global belongs to an instance, they all start at the
    // same address, and the memory underneath is shared.
    if (msg.kernelStack) k.__stack_pointer.value = msg.kernelStack | 0;
    stage = new Uint8Array(process.buffer);
    post({ kind: 'ready' });
    return;
  }
  if (!k) return;
  if (msg?.kind === 'write') {
    // Into the kernel, then a wake: whoever is parked on the other end is
    // woken by the same counter another guest's write would have bumped.
    const bytes = msg.bytes;
    let off = 0;
    while (off < bytes.length) {
      const n = Math.min(bytes.length - off, 0x10000);
      stage.set(bytes.subarray(off, off + n), STAGE_AT);
      const wrote = k.k_pipe_write(msg.pipe, STAGE_AT, n);
      if (wrote <= 0) break;
      off += wrote;
    }
    k.k_wake();
    post({ kind: 'wrote', id: msg.id, n: off });
    return;
  }
  if (msg?.kind === 'close') {
    k.k_pipe_close_end(msg.pipe, 1);
    k.k_wake();
    return;
  }
};
