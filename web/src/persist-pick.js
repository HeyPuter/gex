// Which files a persist snapshot looks at (persist-worker.js), apart from the
// kernel so that it can be tested alone (web/test/persist-pick.mjs).

/** path -> meta for a snapshot of `root`: the files under it the program
 *  wrote (`written`, from the kernel's `k_fs_written`), and the ones stored
 *  before (`known`) that are still there, marked `keep` when not written
 *  since: their stored copy stands and they are not read again. A file
 *  restored from an earlier snapshot is one of these, whatever its mtime.
 *  `meta(path)` is asked about those paths only, so a file the program left
 *  alone is never looked at, let alone read (a lazy one fetched). A stored
 *  path missing from the result was deleted. */
export function pick(root, written, known, meta) {
  const out = new Map();
  const under = `${root}/`;
  for (const p of written) {
    if (!p.startsWith(under) || out.has(p)) continue;
    const m = meta(p);
    if (m?.file) out.set(p, m);
  }
  for (const p of known) {
    if (out.has(p)) continue;
    const m = meta(p);
    if (m?.file) out.set(p, { ...m, keep: true });
  }
  return out;
}
