// The translation cache across page loads: where the
// global kernel's saved module store is kept. OPFS in a browser (a Worker
// or the page), a directory under Node (GEX_XCACHE_DIR, for run-node).
// One file per fingerprint; the name is the fingerprint.

const hasOpfs = () => !!globalThis.navigator?.storage?.getDirectory;
const nodeDir = () => (typeof process !== 'undefined' ? process.env?.GEX_XCACHE_DIR : undefined);

/** The saved bytes for `name`, or null. */
export async function xcRead(name) {
  try {
    if (hasOpfs()) {
      const dir = await navigator.storage.getDirectory();
      const fh = await dir.getFileHandle(name);
      return new Uint8Array(await (await fh.getFile()).arrayBuffer());
    }
    const d = nodeDir();
    if (d) {
      const fs = await import('node:fs/promises');
      return new Uint8Array(await fs.readFile(`${d}/${name}`));
    }
  } catch {
    // Nothing saved yet.
  }
  return null;
}

/** Replace what is saved for `name`. */
export async function xcWrite(name, bytes) {
  if (hasOpfs()) {
    const dir = await navigator.storage.getDirectory();
    const fh = await dir.getFileHandle(name, { create: true });
    // A sync handle where there is one (a Worker): no copy of the whole file
    // into a swap file first.
    if (fh.createSyncAccessHandle) {
      const h = await fh.createSyncAccessHandle();
      try {
        h.truncate(0);
        h.write(bytes, { at: 0 });
        h.flush();
      } finally {
        h.close();
      }
    } else {
      const w = await fh.createWritable();
      await w.write(bytes);
      await w.close();
    }
    return true;
  }
  const d = nodeDir();
  if (d) {
    const fs = await import('node:fs/promises');
    await fs.mkdir(d, { recursive: true });
    const tmp = `${d}/${name}.${process.pid}.${Date.now()}`;
    await fs.writeFile(tmp, bytes);
    await fs.rename(tmp, `${d}/${name}`);
    return true;
  }
  return false;
}

/** A 64-bit change detector over bytes (FNV-1a on 32-bit words, two lanes), as hex. */
export function xcHash(...parts) {
  let a = 0x811c9dc5;
  let b = 0x01000193 ^ 0x5bd1e995;
  for (const d of parts) {
    const u = d instanceof Uint8Array ? d : new TextEncoder().encode(String(d));
    const n = u.byteOffset % 4 ? 0 : u.length >>> 2;
    const w = new Uint32Array(u.buffer, u.byteOffset, n);
    for (let i = 0; i < n; i++) {
      a = Math.imul(a ^ w[i], 0x01000193);
      b = Math.imul(b ^ w[i] ^ (b >>> 15), 0x2c1b3c6d);
    }
    for (let i = n * 4; i < u.length; i++) {
      a = Math.imul(a ^ u[i], 0x01000193);
      b = Math.imul(b ^ u[i], 0x2c1b3c6d);
    }
    a = Math.imul(a ^ u.length, 0x01000193);
  }
  return (a >>> 0).toString(16).padStart(8, '0') + (b >>> 0).toString(16).padStart(8, '0');
}

// Saves under way, across a page's processes: a store save is one process's
// copy of the global kernel's store, written out after that process has
// published its exit. Pid 1's exit stops every Worker (host.mjs `stop`), so a
// write still going in another process was cut off, and the kernel refused pid
// 1's own save while that one was being taken. The gate is two words of shared
// memory the host makes and every process Worker is given:
//   [0] processes inside a save (`xcEnter` to `xcLeave`), with XC_STOP set
//       once the host is stopping, after which no save starts;
//   [1] of those, the ones taking or writing the store's bytes.
// The host waits for [0] to empty before it ends the Workers; a process whose
// save found nothing to take while [1] was not empty waits for those writes and
// asks again, so what it added after their copy is saved too.
export const XC_STOP = 1 << 30;

/** The gate, or undefined where memory cannot be shared (one process). */
export function xcGate() {
  try {
    return new Int32Array(new SharedArrayBuffer(8));
  } catch {
    return undefined;
  }
}

/** Into a save; false once the host is stopping. */
export function xcEnter(gate) {
  for (;;) {
    const v = Atomics.load(gate, 0);
    if (v & XC_STOP) return false;
    if (Atomics.compareExchange(gate, 0, v, v + 1) === v) return true;
  }
}

/** Out of a save. */
export function xcLeave(gate) {
  Atomics.sub(gate, 0, 1);
}

/** How many processes are inside a save. */
export function xcBusy(gate) {
  return gate ? Atomics.load(gate, 0) & ~XC_STOP : 0;
}

export const xcSleep = (ms) => new Promise((r) => setTimeout(r, ms));
