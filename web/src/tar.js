// Just enough tar to unpack a root filesystem in one request. Handles what GNU
// tar emits for a directory tree: ustar, the two GNU long-name extensions, and
// the `prefix` field (how ustar stores a path over 100 characters).

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
 * Unpack a tar into `{ files, dirs, links, hard, modes }`.
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
  // Only the modes that differ from the 0644 default, i.e. the execute bit --
  // which dpkg checks: "'sh' not found in PATH or not executable" for a
  // `/bin/sh` that is plainly there. Keeps this to the few hundred binaries.
  const modes = Object.create(null);
  const hard = Object.create(null);
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
    } else if (type === '2') {
      const p = path(name);
      const target = str(b, off + 157, 100);
      if (p) links[p] = target;
    } else if (type === '1') {
      // A hard link's target is relative to the root of the archive, not the
      // link's directory (a symlink's is). Resolving it like a symlink would
      // lose `/usr/bin/perl` and break every `#!/usr/bin/perl` script.
      const p = path(name);
      const target = str(b, off + 157, 100);
      if (p && target) hard[p] = `/${target.replace(/^\.\//, '')}`;
    } else if (type === '0' || type === '\0' || type === '7') {
      const p = path(name);
      if (p) {
        // A view, not a copy. A 650 MB image copied file by file is 650 MB
        // of page memory on top of the 650 MB just fetched, and the tab ran
        // out before the first window appeared. The bytes are read once more
        // when they are written into the guest's filesystem, and nothing
        // mutates them in between.
        files[p] = b.subarray(body, body + size);
        const mode = octal(b, off + 100, 8) & 0o7777;
        if (mode & 0o111) modes[p] = mode;
      }
    }
    off = next;
  }
  return { files, dirs, links, hard, modes };
}
