// What a persistence store kept, laid over the tree before boot (fs.js),
// apart from persist.js so the filesystem does not pull in the rest.

/** Records laid over a tree as runProgram takes it (`opts`: files, dirs,
 *  symlinks, hard, lazy, modes, mtimes). */
export function applyRecords(opts, records) {
  const under = (p, at) => p === at || p.startsWith(`${at}/`);
  const gone = records.filter((r) => r.kind === 'gone').map((r) => r.path);
  if (gone.length) {
    const hit = (p) => gone.some((g) => under(p, g));
    for (const o of [opts.files, opts.symlinks, opts.hard, opts.modes, opts.mtimes ?? {}]) {
      for (const p of Object.keys(o)) if (hit(p)) delete o[p];
    }
    opts.dirs = opts.dirs.filter((d) => !hit(d));
    opts.lazy = opts.lazy.filter((l) => !hit(l[0]));
  }
  const taken = new Set();
  const take = (p) => {
    delete opts.files[p];
    delete opts.symlinks[p];
    delete opts.hard[p];
    taken.add(p);
  };
  const sorted = records.filter((r) => r.kind !== 'gone').sort((a, b) => a.path.length - b.path.length);
  const dirs = new Set(opts.dirs);
  for (const r of sorted) {
    if (r.kind === 'file') {
      take(r.path);
      opts.files[r.path] = r.data instanceof Uint8Array ? r.data : new Uint8Array(r.data ?? []);
      opts.modes[r.path] = r.mode ?? 0o644;
      if (r.mtime) (opts.mtimes ??= {})[r.path] = r.mtime;
    } else if (r.kind === 'dir') {
      dirs.add(r.path);
      opts.modes[r.path] = r.mode ?? 0o755;
    } else if (r.kind === 'symlink') {
      take(r.path);
      opts.symlinks[r.path] = r.target;
    } else if (r.kind === 'mode') {
      opts.modes[r.path] = r.mode;
    }
  }
  opts.dirs = [...dirs];
  if (taken.size) opts.lazy = opts.lazy.filter((l) => !taken.has(l[0]));
}
