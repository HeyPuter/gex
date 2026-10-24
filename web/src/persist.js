// A guest directory that outlives the page (an ejected program's saves and
// settings), kept in IndexedDB.
//
// - `restore(db, root)`, before the program starts: the files stored last
//   time, to lay over the bundle's.
// - `persist(db, root, spec)`, once it runs: a Worker (persist-worker.js)
//   snapshots `root` from the global kernel's filesystem about a second
//   after the program finishes a change under it (a close after a write, a
//   rename, an unlink), and also every 30 s, when the page is hidden and
//   when it is left, storing what changed. A reload does not wait for the
//   last of these, so the first is what keeps a save.
//
// What a snapshot sees is whatever the program has written so far; a save
// half-written at the instant of a snapshot is stored half-written, and the
// next snapshot fixes it (IDBFS's syncfs has the same window).

import { openDb, readAll } from './persist-db.js';
import { logOn } from '@gex/log.mjs';

export async function restore(dbName, root) {
  try {
    const db = await openDb(dbName);
    const all = await readAll(db);
    db.close();
    const prefix = `${root.replace(/\/+$/, '')}/`;
    return all.filter(([p]) => p.startsWith(prefix));
  } catch (e) {
    console.warn(`[persist] nothing restored: ${e?.message ?? e}`);
    return [];
  }
}

export function persist(dbName, root, spec, { every = 30000, onSync = null } = {}) {
  if (!spec?.module || !spec?.memory) {
    console.warn('[persist] no global kernel (no SharedArrayBuffer): saves will not outlive the page');
    return null;
  }
  const worker = new Worker(new URL('./persist-worker.js', import.meta.url), { type: 'module', name: 'persist' });
  let seq = 0;
  let busy = false;
  const waiters = new Map();
  worker.onmessage = (e) => {
    const m = e.data;
    if (m.error && !('synced' in m)) console.warn(`[persist] ${m.error}`);
    if (m.auto) {
      if ((m.wrote || m.removed) && logOn('debug')) console.log(`[persist] after a save: stored ${m.wrote} file(s) (${m.bytes} bytes), removed ${m.removed}; ${m.files} under ${root}`);
      onSync?.(m);
    }
    if ('synced' in m) {
      busy = false;
      if (m.error) console.warn(`[persist] snapshot failed: ${m.error}`);
      else if ((m.wrote || m.removed) && logOn('debug')) console.log(`[persist] stored ${m.wrote} file(s) (${m.bytes} bytes), removed ${m.removed}; ${m.files} under ${root}`);
      onSync?.(m);
      waiters.get(m.synced)?.(m);
      waiters.delete(m.synced);
    }
  };
  worker.postMessage({ spec: { module: spec.module, memory: spec.memory }, root, db: dbName });
  const sync = () => {
    // One snapshot at a time; a request while one runs is left to it.
    if (busy) return Promise.resolve(null);
    busy = true;
    const id = ++seq;
    const done = new Promise((resolve) => waiters.set(id, resolve));
    worker.postMessage({ sync: id });
    return done;
  };
  const timer = setInterval(sync, every);
  addEventListener('pagehide', sync);
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) sync();
  });
  return {
    sync,
    stop() {
      clearInterval(timer);
      worker.terminate();
    },
  };
}
