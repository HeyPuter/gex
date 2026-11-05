// Just enough tar to unpack a root filesystem in one request. Handles what GNU
// tar emits for a directory tree: ustar, the two GNU long-name extensions, the
// `prefix` field (how ustar stores a path over 100 characters), and a pax
// extended header's path, link target and extended attributes (`tar --xattrs`:
// `SCHILY.xattr.user.*`, how Wine's DOS attributes travel in a prefix).

const BLOCK = 512;

function str(bytes, off, len) {
  let end = off;
  const stop = off + len;
  while (end < stop && bytes[end] !== 0) end++;
  // A copy, not a view: TextDecoder refuses a view of a SharedArrayBuffer,
  // which is what a bundle shared with a Worker is (page/src/nt-page.js).
  return new TextDecoder().decode(bytes.slice(off, end));
}

function octal(bytes, off, len) {
  const s = str(bytes, off, len).trim();
  if (!s) return 0;
  // GNU writes sizes over 8 GB in base 256 with the high bit set. Nothing
  // here is that large, but reading it as octal would be silently wrong.
  if (bytes[off] & 0x80) {
    let n = 0n;
    for (let i = off + 1; i < off + len; i++) n = (n << 8n) | BigInt(bytes[i]);
    return Number(n);
  }
  return parseInt(s, 8) || 0;
}

/**
 * Each entry of a tar, in order: `visit({ name, type, size, body, off,
 * link, ext })`, its name and link target with the long-name and pax
 * headers before it applied (`ext`: that pax header's records), `body`
 * where its bytes start, `off` its header. A true answer stops the walk.
 * Nothing is copied but the names.
 */
export function tarEntries(buf, visit) {
  const b = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
  let off = 0;
  let longName = null;
  let longLink = null;
  // The pax extended header for the next entry: { path, linkpath, xattrs }.
  let pax = null;
  while (off + BLOCK <= b.length) {
    // Two zero blocks end the archive; one is enough to know we are done.
    if (b[off] === 0) {
      off += BLOCK;
      continue;
    }
    const size = octal(b, off + 124, 12);
    const type = String.fromCharCode(b[off + 156] || 0x30);
    const body = off + BLOCK;
    const next = body + Math.ceil(size / BLOCK) * BLOCK;
    if (type === 'x') {
      pax = paxRecords(b.subarray(body, body + size));
    } else if (type === 'K') {
      // GNU long link target, for the next entry.
      longLink = str(b, body, size);
    } else if (type === 'L') {
      // GNU long name: this entry's body is the name of the next entry.
      longName = str(b, body, size);
    } else if (type !== 'g') {
      let name = str(b, off, 100);
      const prefix = str(b, off + 345, 155);
      if (prefix) name = `${prefix}/${name}`;
      if (longName !== null) name = longName;
      let link = longLink ?? str(b, off + 157, 100);
      longName = longLink = null;
      const ext = pax;
      pax = null;
      if (ext?.path !== undefined) name = ext.path;
      if (ext?.linkpath !== undefined) link = ext.linkpath;
      if (visit({ name, type, size, body, off, link, ext })) return;
    }
    off = next;
  }
}

/** An empty unpacked tree, as `untar` returns it. */
function tree() {
  return {
    files: Object.create(null),
    dirs: [],
    links: Object.create(null),
    hard: Object.create(null),
    // Only the modes that differ from the default: a file's execute bit --
    // which dpkg checks: "'sh' not found in PATH or not executable" for a
    // `/bin/sh` that is plainly there -- and a directory's other than 0755.
    // Keeps this to the few hundred binaries.
    modes: Object.create(null),
    // Each file's modification time (seconds), for a page that hands them on
    // (nt-page's `mtimes=1`).
    mtimes: Object.create(null),
    // Extended attributes (`user.*`), path to { name: bytes }: what the pax
    // headers carry, for `gex_set_xattr`.
    xattrs: Object.create(null),
  };
}

/** A guest path for an entry's name: absolute under `root`, without the
 *  leading `./` tar adds; null for the archive's own top. */
function entryPath(root, name) {
  const p = name.replace(/^\.\//, '').replace(/\/+$/, '');
  if (!p) return null;
  return `${root}/${p}`.replace(/\/{2,}/g, '/');
}

/** One entry into tree `t`: its header at `hdr[off..]`, its bytes `body`
 *  (a regular file's; kept, not copied). */
function addEntry(t, root, { name, type, link, ext }, hdr, off, body) {
  const keepXattrs = (p) => {
    if (p && ext && ext.xattrs) t.xattrs[p] = ext.xattrs;
  };
  if (type === '5') {
    const p = entryPath(root, name);
    if (p) t.dirs.push(p);
    // The archive's own top (`./`) names the directory it unpacks into,
    // which exists already: only its time is taken.
    else if (/^(\.\/?)?$/.test(name)) t.mtimes[root || '/'] = octal(hdr, off + 136, 12);
    // its time too: a cache built over a directory (KDE's ksycoca) is
    // stale when the directory looks newer
    if (p) t.mtimes[p] = octal(hdr, off + 136, 12);
    keepXattrs(p);
    // A directory's mode, when it is not the VFS's 0755: a read-only one
    // (no write bits) is what Wine reports as FILE_ATTRIBUTE_READONLY, and
    // the Windows shell reads a folder's desktop.ini names only then.
    const mode = octal(hdr, off + 100, 8) & 0o7777;
    if (p && mode && mode !== 0o755) t.modes[p] = mode;
  } else if (type === '2') {
    const p = entryPath(root, name);
    if (p) t.links[p] = link;
  } else if (type === '1') {
    // A hard link's target is relative to the root of the archive, not the
    // link's directory (a symlink's is). Resolving it like a symlink would
    // lose `/usr/bin/perl` and break every `#!/usr/bin/perl` script.
    const p = entryPath(root, name);
    if (p && link) t.hard[p] = `/${link.replace(/^\.\//, '')}`;
  } else if (type === '0' || type === '\0' || type === '7') {
    const p = entryPath(root, name);
    if (p) {
      t.files[p] = body;
      const mode = octal(hdr, off + 100, 8) & 0o7777;
      if (mode & 0o111) t.modes[p] = mode;
      t.mtimes[p] = octal(hdr, off + 136, 12);
      keepXattrs(p);
    }
  }
}

/**
 * Unpack a tar into `{ files, dirs, links, hard, modes }`.
 *
 * `files` maps an absolute guest path to its bytes, `dirs` lists directories
 * (so an empty one still exists), and `links` maps a symlink's path to its
 * target. Paths are made absolute under `root` and the leading `./` that tar
 * adds is dropped.
 */
export function untar(buf, root = '') {
  const b = new Uint8Array(buf);
  const t = tree();
  // A file's bytes are a view, not a copy. A 650 MB image copied file by
  // file is 650 MB of page memory on top of the 650 MB just fetched, and the
  // tab ran out before the first window appeared. The bytes are read once
  // more when they are written into the guest's filesystem, and nothing
  // mutates them in between.
  tarEntries(b, (e) => addEntry(t, root, e, b, e.off, b.subarray(e.body, e.body + e.size)));
  return t;
}

/**
 * A tar unpacked as it arrives (`push` each decoded piece, in order; `end`
 * once all are in), for a bundle laid into the guest while it downloads.
 * Each regular file's bytes are copied once out of the pieces, into an
 * array of their own, so a piece is free once pushed; `take()` hands over
 * what has been unpacked since the last call, as `untar`'s parts. The
 * archive's entries come out in its order, the same entries `untar` makes.
 */
export class TarStream {
  #root;
  #t = tree();
  #hdr = new Uint8Array(BLOCK);
  #hdrAt = 0;
  // The entry whose body is coming: its header fields, the bytes so far.
  #entry = null;
  #body = null;
  #bodyAt = 0;
  #skip = 0;
  #longName = null;
  #longLink = null;
  #pax = null;
  #bytes = 0;
  #files = 0;
  #ended = false;

  constructor(root = '') {
    this.#root = root;
  }

  /** Directories and links passed over unread (their headers' names not
   *  decoded): an archive with an index names them all already (app.js
   *  `streamBundle`), and its tens of thousands of links were half of this
   *  parser's time, at the archive's end, on the page's main thread. */
  skipLinks = false;

  /** The files' bytes unpacked and not yet taken. */
  get pending() { return this.#bytes; }

  /** Regular files unpacked so far, in all. */
  get files() { return this.#files; }

  /** A decoded piece of the archive (a view the caller may reuse after). */
  push(part) {
    if (this.#ended) throw new Error('tar: a piece after the end');
    let at = 0;
    while (at < part.length) {
      if (this.#skip) {
        const n = Math.min(this.#skip, part.length - at);
        this.#skip -= n;
        at += n;
        continue;
      }
      if (this.#body) {
        const n = Math.min(this.#body.length - this.#bodyAt, part.length - at);
        this.#body.set(part.subarray(at, at + n), this.#bodyAt);
        this.#bodyAt += n;
        at += n;
        if (this.#bodyAt === this.#body.length) this.#bodyDone();
        continue;
      }
      const n = Math.min(BLOCK - this.#hdrAt, part.length - at);
      this.#hdr.set(part.subarray(at, at + n), this.#hdrAt);
      this.#hdrAt += n;
      at += n;
      if (this.#hdrAt === BLOCK) {
        this.#hdrAt = 0;
        this.#header();
      }
    }
  }

  /** The archive is all in: what is left over is an error. */
  end() {
    if (this.#body || this.#hdrAt) throw new Error('tar: the archive ends inside an entry');
    this.#ended = true;
  }

  /** What was unpacked since the last `take`. */
  take() {
    const t = this.#t;
    this.#t = tree();
    this.#bytes = 0;
    return t;
  }

  #header() {
    const b = this.#hdr;
    // Two zero blocks end the archive; one is enough to know we are done.
    if (b[0] === 0) return;
    const size = octal(b, 124, 12);
    const type = String.fromCharCode(b[156] || 0x30);
    const pad = Math.ceil(size / BLOCK) * BLOCK - size;
    if (this.skipLinks && (type === '1' || type === '2' || type === '5')) {
      this.#longName = this.#longLink = this.#pax = null;
      this.#skip = size + pad;
      return;
    }
    const e = { size, type, pad, hdr: b.slice() };
    if (type === 'x' || type === 'K' || type === 'L' || type === 'g' || type === '0' || type === '\0' || type === '7') {
      this.#entry = e;
      this.#body = new Uint8Array(size);
      this.#bodyAt = 0;
      if (!size) this.#bodyDone();
      return;
    }
    // No body to keep (directories, links, devices).
    this.#entry = e;
    this.#bodyDone(true);
    this.#skip = size + pad;
  }

  #bodyDone(none = false) {
    const e = this.#entry;
    const body = none ? null : this.#body;
    this.#entry = null;
    this.#body = null;
    if (!none) this.#skip = e.pad;
    const { type } = e;
    if (type === 'x') {
      this.#pax = paxRecords(body);
    } else if (type === 'K') {
      this.#longLink = str(body, 0, body.length);
    } else if (type === 'L') {
      this.#longName = str(body, 0, body.length);
    } else if (type !== 'g') {
      const b = e.hdr;
      let name = str(b, 0, 100);
      const prefix = str(b, 345, 155);
      if (prefix) name = `${prefix}/${name}`;
      if (this.#longName !== null) name = this.#longName;
      let link = this.#longLink ?? str(b, 157, 100);
      this.#longName = this.#longLink = null;
      const ext = this.#pax;
      this.#pax = null;
      if (ext?.path !== undefined) name = ext.path;
      if (ext?.linkpath !== undefined) link = ext.linkpath;
      addEntry(this.#t, this.#root, { name, type, link, ext }, b, 0, body);
      if (body) {
        this.#bytes += body.length;
        this.#files++;
      }
    }
  }
}

/**
 * A pax extended header's records (`LEN key=value\n`, LEN counting the whole
 * record): the path, the link target and the `user.` extended attributes,
 * GNU tar's (`SCHILY.xattr.NAME`, raw bytes) and libarchive's
 * (`LIBARCHIVE.xattr.NAME`, base64). Values are bytes, not text.
 */
function paxRecords(rec) {
  const out = {};
  let at = 0;
  while (at < rec.length) {
    let sp = at;
    while (sp < rec.length && rec[sp] !== 0x20) sp++;
    const len = parseInt(new TextDecoder().decode(rec.slice(at, sp)), 10);
    if (!(len > 0) || at + len > rec.length) break;
    const end = at + len - 1; // the record's newline
    let eq = sp + 1;
    while (eq < end && rec[eq] !== 0x3d) eq++;
    const key = new TextDecoder().decode(rec.slice(sp + 1, eq));
    const value = rec.slice(eq + 1, end);
    if (key === 'path') out.path = new TextDecoder().decode(value);
    else if (key === 'linkpath') out.linkpath = new TextDecoder().decode(value);
    else if (key.startsWith('SCHILY.xattr.user.')) (out.xattrs ??= {})[key.slice(13)] = value;
    else if (key.startsWith('LIBARCHIVE.xattr.user.')) {
      const raw = atob(new TextDecoder().decode(value));
      (out.xattrs ??= {})[decodeURIComponent(key.slice(17))] = Uint8Array.from(raw, (c) => c.charCodeAt(0));
    }
    at += len;
  }
  return out;
}
