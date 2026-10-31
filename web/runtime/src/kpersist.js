// What the programs changed in the root, as records a store keeps
// (persist.js): the tree as seeded (taken before the first program) against
// the tree now, both from the kernel's node list (`k_fs_tree`, no file's
// bytes), and against what the store holds (`known`: path -> stamp). A
// file's bytes are read only when a program wrote it and it changed since it
// was last stored, so a lazy file nobody wrote is never fetched for a
// snapshot.

import { errno } from './kernel.js';

const dec = new TextDecoder();
const ERANGE = -34;
const TREE_ENTRY = 40;
const KINDS = ['file', 'dir', 'symlink', 'socket'];

const within = (p, at) => p === at || p.startsWith(at === '/' ? '/' : `${at}/`);
const parentOf = (p) => p.replace(/\/[^/]*$/, '') || '/';

/** FNV-1a over 32-bit words and the tail: a change detector, not a checksum. */
function hash(d) {
  let h = 0x811c9dc5;
  const words = new Uint32Array(d.buffer, d.byteOffset, d.byteOffset % 4 ? 0 : d.length >>> 2);
  for (let i = 0; i < words.length; i++) h = Math.imul(h ^ words[i], 0x01000193);
  for (let i = words.length * 4; i < d.length; i++) h = Math.imul(h ^ d[i], 0x01000193);
  return (h >>> 0).toString(16);
}

export class Changes {
  #k;
  #files;
  #seed = null;
  #known = new Map();
  #changes = new Map();
  #exclude = [];
  #saves = 0;

  constructor(kernel, files) {
    this.#k = kernel;
    this.#files = files;
  }

  async #tree() {
    for (let cap = 1 << 20; ;) {
      const { r, out: [b] } = await this.#k.call('k_fs_tree', { out: cap });
      if (r === ERANGE) {
        cap = new DataView(b.buffer, b.byteOffset).getUint32(0, true) + (1 << 16);
        continue;
      }
      if (r < 0) throw errno(r, 'the tree');
      const v = new DataView(b.buffer, b.byteOffset, b.length);
      const out = new Map();
      for (let at = 0; at + TREE_ENTRY <= r;) {
        const pl = v.getUint32(at + 32, true), tl = v.getUint32(at + 36, true);
        const path = `/${dec.decode(b.subarray(at + TREE_ENTRY, at + TREE_ENTRY + pl))}`;
        out.set(path, {
          kind: KINDS[b[at]] ?? 'file',
          written: b[at + 1] === 1,
          mode: v.getUint32(at + 4, true) & 0o7777,
          size: Number(v.getBigUint64(at + 8, true)),
          changes: v.getBigUint64(at + 16, true),
          mtime: Number(v.getBigInt64(at + 24, true)),
          target: tl ? dec.decode(b.subarray(at + TREE_ENTRY + pl, at + TREE_ENTRY + pl + tl)) : '',
        });
        at += TREE_ENTRY + pl + tl;
      }
      return out;
    }
  }

  /** Start keeping track: `known`, what the store holds ([path, stamp]);
   *  `exclude`, paths never kept. The seed is the tree now. */
  async start({ known = [], exclude = [] }) {
    this.#seed = await this.#tree();
    this.#known = new Map(known);
    this.#exclude = exclude;
    const [, { r }] = await this.#k.calls([{ f: 'k_fs_watch', a: [{ in: '/' }] }, { f: 'k_fs_saves' }]);
    this.#saves = r;
  }

  /** Whether a program finished a change since the last ask (`k_fs_saves`). */
  async changed() {
    const n = await this.#k.r('k_fs_saves');
    const moved = n !== this.#saves;
    this.#saves = n;
    return moved;
  }

  /** What changed since the store's last: `{ put: [record], drop: [path] }`;
   *  a record is `{ path, kind, stamp, mode, mtime, data, target }`, kind
   *  'file', 'dir', 'symlink', 'mode' (a seeded file's mode only) or 'gone'
   *  (a seeded path deleted, and everything under it). */
  async diff(exclude) {
    if (!this.#seed) throw new Error('persistence is not started');
    if (exclude) this.#exclude = exclude;
    const excluded = (p) => p === '/' || this.#exclude.some((at) => within(p, at));
    const cur = await this.#tree();
    const seed = this.#seed;
    const known = this.#known;
    const put = [];
    const keep = new Set();
    const has = (p, stamp) => known.get(p) === stamp && keep.add(p);
    for (const [p, n] of cur) {
      if (excluded(p) || n.kind === 'socket') continue;
      const s = seed.get(p);
      if (n.kind === 'file') {
        const was = known.get(p);
        // A file restored from the store and chmod'ed since: stored again.
        const rewrite = was?.startsWith('f:') && !was.startsWith(`f:${n.mode.toString(8)}:`);
        if (n.written || rewrite) {
          if (!rewrite && known.has(p) && this.#changes.get(p) === n.changes) { keep.add(p); continue; }
          const data = await this.#files.readRaw(p, n.size);
          if (!data) continue;
          const stamp = `f:${n.mode.toString(8)}:${data.length}:${hash(data)}`;
          this.#changes.set(p, n.changes);
          if (has(p, stamp)) continue;
          put.push({ path: p, kind: 'file', stamp, mode: n.mode, mtime: n.mtime, data });
        } else if (was?.startsWith('f:')) {
          keep.add(p);
        } else {
          const stamp = `m:${n.mode.toString(8)}`;
          if (has(p, stamp)) continue;
          if (s?.kind === 'file' && s.mode !== n.mode) put.push({ path: p, kind: 'mode', stamp, mode: n.mode });
        }
      } else if (n.kind === 'dir') {
        const stamp = `d:${n.mode.toString(8)}`;
        if (has(p, stamp)) continue;
        if (s?.kind === 'dir' && s.mode === n.mode) continue;
        put.push({ path: p, kind: 'dir', stamp, mode: n.mode, mtime: n.mtime });
      } else if (n.kind === 'symlink') {
        const stamp = `l:${n.target}`;
        if (has(p, stamp)) continue;
        if (s?.kind === 'symlink' && s.target === n.target) continue;
        put.push({ path: p, kind: 'symlink', stamp, target: n.target });
      }
    }
    // Deleted: the topmost of what is gone, once.
    for (const p of seed.keys()) {
      if (cur.has(p) || excluded(p)) continue;
      const up = parentOf(p);
      if (up !== '/' && !cur.has(up) && seed.has(up)) continue;
      if (!has(p, 'gone')) put.push({ path: p, kind: 'gone', stamp: 'gone' });
    }
    // Deleted before this load: still gone.
    for (const [p, stamp] of known) if (stamp === 'gone' && !cur.has(p)) keep.add(p);
    const putting = new Set(put.map((r) => r.path));
    const drop = [...known.keys()].filter((p) => !keep.has(p) && !putting.has(p));
    return { put, drop };
  }

  /** The store took a diff: what it now holds. */
  stored({ put = [], drop = [] }) {
    for (const p of drop) {
      this.#known.delete(p);
      this.#changes.delete(p);
    }
    for (const [p, stamp] of put) this.#known.set(p, stamp);
  }
}
