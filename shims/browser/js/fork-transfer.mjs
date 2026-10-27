// What the page and a process's Worker hand on with a fork record.

/**
 * The buffers in a fork record, so `postMessage` transfers them instead of
 * copying. Views on a `SharedArrayBuffer` are skipped -- shared buffers cannot
 * be transferred, and the guest's memory is shared when threads are on; the
 * regions are `slice`d, so each owns its buffer and transferring one cannot
 * detach another.
 */
export function forkTransfer(child) {
  const out = [];
  const add = (v) => {
    if (!ArrayBuffer.isView(v)) return;
    const b = v.buffer;
    if (!(b instanceof ArrayBuffer) || out.includes(b)) return;
    out.push(b);
  };
  for (const r of child?.image ?? []) add(r?.bytes);
  add(child?.vmaBlob);
  add(child?.fds);
  add(child?.pages);
  add(child?.shared);
  return out;
}
