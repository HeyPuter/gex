// The page's store Worker for the kernel's block store (an `OPFSFilesystem`
// in the default build; hosts/browser/js/blocks.mjs). It holds the one store
// file's sync access handle in the origin private file system and answers
// the slots in the kernel's memory (`BLOCKS_CTL`): reads straight into the
// kernel's memory, writes out of it. Every request is synchronous: it waits
// on the slots, never yields.
//
// In: `{ memory, ctl, name, verify }`. Out: `{ kind: 'ready', ok, why }`.
import { blocksVerify, handleOp, openHandle, sharedReads, storeDir, STORE_FILE, SLOT0, SLOTW, SLOTS, ASKED, DONE } from '../../hosts/browser/js/blocks.mjs';

onmessage = async ({ data: { memory, ctl, name, verify } }) => {
  blocksVerify(verify);
  let h;
  try {
    const root = await navigator.storage.getDirectory();
    const dir = await root.getDirectoryHandle(storeDir(name), { create: true });
    h = await openHandle(await dir.getFileHandle(STORE_FILE, { create: true }));
  } catch (e) {
    postMessage({ kind: 'ready', ok: false, why: String(e?.message ?? e) });
    return;
  }
  const shared = sharedReads(h);
  const c = () => new Int32Array(memory.buffer, ctl, 1024);
  Atomics.store(c(), 1, 1);
  postMessage({ kind: 'ready', ok: true });
  for (;;) {
    const v = c();
    const seen = Atomics.load(v, 0);
    for (let i = 0; i < SLOTS; i++) {
      const s = SLOT0 + i * SLOTW;
      if (Atomics.load(v, s) !== ASKED) continue;
      const op = v[s + 1], dst = v[s + 2] >>> 0, len = v[s + 3] >>> 0;
      const off = (v[s + 10] >>> 0) + (v[s + 11] >>> 0) * 0x100000000;
      let r = -1;
      try {
        r = handleOp(h, shared, memory, op, off, dst, len);
      } catch (e) {
        console.error(`[blocks] op ${op} at ${off}+${len}: ${e?.message ?? e}`);
      }
      const w = c();
      w[s + 4] = r;
      Atomics.store(w, s, DONE);
      Atomics.notify(w, s);
    }
    Atomics.wait(c(), 0, seen, 5000);
  }
};
