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

/** A site-root path (`/gex/x.wasm`) under the library's base, when it has
 *  one; the path as it is otherwise. */
export function assetUrl(path) {
  const base = globalThis.GEX_ASSET_BASE;
  return base ? new URL(path.replace(/^\//, ''), base).href : path;
}
