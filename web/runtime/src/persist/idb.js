// `IdbPersist`: a Persist whose store is an IndexedDB database of the page's
// origin, one record per path (persist.js).

import { Persist } from '../persist.js';

const STORE = 'nodes';

function req(r) {
  return new Promise((resolve, reject) => {
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

function done(tx) {
  return new Promise((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error ?? new Error('IndexedDB: the transaction was aborted'));
  });
}

/** A Persist store over the IndexedDB database `name`. */
export class IdbStore {
  #name;
  #db = null;

  constructor(name) {
    if (typeof indexedDB === 'undefined') throw new Error('IdbPersist: this page has no IndexedDB');
    this.#name = String(name);
  }

  #open() {
    if (this.#db) return this.#db;
    const r = indexedDB.open(this.#name, 1);
    r.onupgradeneeded = () => {
      if (!r.result.objectStoreNames.contains(STORE)) r.result.createObjectStore(STORE, { keyPath: 'path' });
    };
    this.#db = req(r).then((db) => {
      // Another page deleting it (reset) closes it here.
      db.onversionchange = () => { db.close(); this.#db = null; };
      return db;
    });
    return this.#db;
  }

  async load() {
    const db = await this.#open();
    const tx = db.transaction(STORE, 'readonly');
    return req(tx.objectStore(STORE).getAll());
  }

  async store({ put = [], drop = [] }) {
    const db = await this.#open();
    const tx = db.transaction(STORE, 'readwrite');
    const s = tx.objectStore(STORE);
    for (const p of drop) s.delete(p);
    for (const r of put) s.put(r);
    await done(tx);
  }

  async reset() {
    const db = await this.#db?.catch(() => null);
    db?.close();
    this.#db = null;
    await req(indexedDB.deleteDatabase(this.#name));
  }
}

export class IdbPersist extends Persist {
  /** `fs`'s root kept in the IndexedDB database `db` (one per tree: a
   *  different tree under the same name would be laid with another's
   *  changes). The rest of the options are Persist's. */
  constructor(fs, { db, ...opts } = {}) {
    if (!db) throw new TypeError('IdbPersist: name its database ({ db })');
    super(fs, new IdbStore(db), opts);
  }
}
