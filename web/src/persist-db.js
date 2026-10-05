// The IndexedDB side of persist.js: one database per page (its name the
// caller's), with `files` (guest path -> bytes) and `meta` (guest path ->
// "size:hash", what the persist worker compares a snapshot against).

export function openDb(name) {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(name, 1);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains('files')) db.createObjectStore('files');
      if (!db.objectStoreNames.contains('meta')) db.createObjectStore('meta');
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

export function txDone(tx) {
  return new Promise((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error ?? new Error('transaction aborted'));
  });
}

/** Every stored file, as [path, Uint8Array]. */
export async function readAll(db) {
  const tx = db.transaction('files', 'readonly');
  const req = tx.objectStore('files').openCursor();
  const out = [];
  await new Promise((resolve, reject) => {
    req.onsuccess = () => {
      const c = req.result;
      if (!c) return resolve();
      out.push([c.key, c.value instanceof Uint8Array ? c.value : new Uint8Array(c.value)]);
      c.continue();
    };
    req.onerror = () => reject(req.error);
  });
  return out;
}
