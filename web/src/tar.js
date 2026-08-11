// Just enough tar to unpack a root filesystem.
//
// A rootfs is a few thousand files, and fetching them one at a time would be
// a few thousand requests. One tar is one request, and the format is simple
// enough that reading it costs less than a dependency: 512-byte headers, a
// name, an octal size, and the contents padded to the next block.
//
// Handles what GNU tar emits for a directory tree: ustar and the two GNU
// long-name extensions, plus the `prefix` field, which is how ustar stores a
// path longer than a hundred characters without an extension at all.

const BLOCK = 512;

function str(bytes, off, len) {
  let end = off;
  const stop = off + len;
  while (end < stop && bytes[end] !== 0) end++;
  return new TextDecoder().decode(bytes.subarray(off, end));
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
 * Unpack a tar into `{ files, dirs, links, modes }`.
 *
 * `files` maps an absolute guest path to its bytes, `dirs` lists directories
 * (so an empty one still exists), and `links` maps a symlink's path to its
 * target. Paths are made absolute under `root` and the leading `./` that tar
 * adds is dropped.
 */
export function untar(buf, root = '') {
  const b = new Uint8Array(buf);
  const files = Object.create(null);
  const dirs = [];
  const links = Object.create(null);
  // Only the executable ones. Every file in the guest is 0644 by default, so
  // what has to survive the tar is the execute bit -- and dpkg checks it:
  //
  //   dpkg: warning: 'sh' not found in PATH or not executable
  //
  // for a `/bin/sh` that is plainly there. Keeping only the modes that differ
  // from the default keeps this to the few hundred binaries in a rootfs
  // rather than an entry per file.
  const modes = Object.create(null);
  let off = 0;
  let longName = null;

  const path = (name) => {
    let p = name.replace(/^\.\//, '').replace(/\/+$/, '');
    if (!p) return null;
    return `${root}/${p}`.replace(/\/{2,}/g, '/');
  };

  while (off + BLOCK <= b.length) {
    // Two zero blocks end the archive; one is enough to know we are done.
    if (b[off] === 0) {
      off += BLOCK;
      continue;
    }
    let name = str(b, off, 100);
    const size = octal(b, off + 124, 12);
    const type = String.fromCharCode(b[off + 156] || 0x30);
    const prefix = str(b, off + 345, 155);
    if (prefix) name = `${prefix}/${name}`;
    if (longName !== null) {
      name = longName;
      longName = null;
    }
    const body = off + BLOCK;
    const next = body + Math.ceil(size / BLOCK) * BLOCK;

    if (type === 'L') {
      // GNU long name: this entry's body is the name of the next entry.
      longName = str(b, body, size);
    } else if (type === '5') {
      const p = path(name);
      if (p) dirs.push(p);
    } else if (type === '2' || type === '1') {
      const p = path(name);
      const target = str(b, off + 157, 100);
      if (p) links[p] = target;
    } else if (type === '0' || type === '\0' || type === '7') {
      const p = path(name);
      if (p) {
        files[p] = b.slice(body, body + size);
        const mode = octal(b, off + 100, 8) & 0o7777;
        if (mode & 0o111) modes[p] = mode;
      }
    }
    off = next;
  }
  return { files, dirs, links, modes };
}
