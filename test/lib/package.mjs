// `--package DIR` (run.mjs): the runtime's entry modules under
// page/runtime/src and what page/runtime/build.sh names each in DIR.
export const PACKAGE_ENTRIES = {
  'index.js': 'index.mjs',
  'fs.js': 'fs.mjs',
  'windows/host.js': 'windows-host.mjs',
  'windows/single.js': 'windows-single.mjs',
  'windows/compositor.js': 'windows-compositor.mjs',
  'net/wisp.mjs': 'net-wisp.mjs',
  'audio.js': 'audio.mjs',
  'gl.js': 'gl.mjs',
  'input.js': 'input.mjs',
};

/** The built module for import `source` from `importer` (paths), when it is
 *  one of the runtime's entries under `src` (page/runtime/src/): else null. */
export function packaged(pkg, src, source, importer) {
  if (!importer || !source.startsWith('.')) return null;
  const abs = new URL(source, `file://${importer.split('?')[0]}`).pathname;
  if (!abs.startsWith(src)) return null;
  const to = PACKAGE_ENTRIES[abs.slice(src.length)];
  return to ? `${pkg}/${to}` : null;
}
