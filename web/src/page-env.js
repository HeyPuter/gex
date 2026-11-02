// Where the runtime's switches and files are, for a page that is not gex's
// own. gex's test pages (nt.html, index.html) take their switches from the
// URL, and their files from the site root (/gex/...). An ejected program's
// library (app.js `start()`) lives in the user's page, whose URL is the
// user's: the library sets `GEX_PAGE_SEARCH` from its own options (never the
// URL) and `GEX_ASSET_BASE` to where its files are, and everything below
// reads them through these.

/** The runtime's `?name=` switches: the library's, else the page URL's. */
export function pageSearch() {
  return globalThis.GEX_PAGE_SEARCH ?? globalThis.location?.search ?? '';
}

/** `pageSearch()` parsed. */
export function pageParams() {
  return new URLSearchParams(pageSearch());
}

/** A site-root path (`/gex/x.wasm`): the file's own URL when the runtime
 *  package lists its files (`GEX_ASSET_FILES`, path -> URL: each file named
 *  where the page's bundler put it), else under the library's base, when it
 *  has one; the path as it is otherwise. */
export function assetUrl(path) {
  const rel = path.replace(/^\//, '');
  const listed = globalThis.GEX_ASSET_FILES?.[rel];
  if (listed) return listed;
  const base = globalThis.GEX_ASSET_BASE;
  // A base may be a path (`/`), which a Worker resolves against its own
  // script's URL: the same site.
  return base ? new URL(rel, new URL(base, globalThis.location?.href)).href : path;
}

/** The listed files under `dir` (`gex/cgen/`), by their path below it, or
 *  null when the package lists none: then they are under `assetUrl(dir)`. */
export function assetFiles(dir) {
  const files = globalThis.GEX_ASSET_FILES;
  if (!files) return null;
  const at = dir.replace(/^\//, '');
  const out = {};
  for (const [k, v] of Object.entries(files)) if (k.startsWith(at)) out[k.slice(at.length)] = v;
  return out;
}
