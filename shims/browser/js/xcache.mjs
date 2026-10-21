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
