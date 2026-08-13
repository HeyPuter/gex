// Loading a statically recompiled closure (program + every library it reaches,
// translated ahead of time) into a page.
import { fetchBytes } from './runtime.js';
import { metadataSection } from '../../shims/browser/js/gex.mjs';

/**
 * Fetch and compile a closure described by a manifest.
 *
 * `base` is the directory the manifest and its modules are served from; the
 * manifest is `{ image, libs, pins }` as `gexc build --closure` writes it.
 * Returns `{ image, libs, pins }` for `runProgram`, or `null` when there is no
 * manifest (a site without a closure runs on the translator instead).
 */
export async function loadClosure(base, { image = false, log = () => {} } = {}) {
  let manifest;
  try {
    const res = await fetch(`${base}/closure.json`);
    if (!res.ok) return null;
    manifest = await res.json();
  } catch {
    return null;
  }

  // `?libs=N` loads only the first N libraries, for bisecting. Slot numbers
  // are absolute and baked in at build time, so leaving some out is safe: the
  // pages they would claim are never claimed and the runtime translator meets
  // that code instead.
  const only = Number(new URLSearchParams(location.search).get('libs') || 0);
  if (only > 0) {
    manifest.libs = manifest.libs.slice(0, only);
    if (manifest.metas) manifest.metas = manifest.metas.slice(0, only);
  }

  // When the build wrote metadata out separately: register a library from its
  // few-KiB meta (which pages it covers) and let `wantLibrary` fetch the module
  // on the branch that lands in it. A manifest without `metas` takes the old
  // fetch-and-compile-everything path below.
  if (manifest.metas && manifest.metas.every((m) => m)) {
    const began = performance.now();
    const libs = [];
    for (let i = 0; i < manifest.libs.length; i += 8) {
      const batch = manifest.libs.slice(i, i + 8);
      const done = await Promise.all(
        batch.map(async (n, j) => ({
          meta: await fetchBytes(`${base}/${encodeURI(manifest.metas[i + j])}`),
          url: `${base}/${encodeURI(n)}`,
          mod: null,
        })),
      );
      libs.push(...done);
    }
    const kib = libs.reduce((n, l) => n + l.meta.length, 0) / 1024;
    log(
      `closure: ${libs.length} libraries registered from ${Math.round(kib)} KiB `
      + `of metadata in ${Math.round(performance.now() - began)}ms, `
      + 'modules fetched on first branch',
    );
    const images = manifest.images ?? {};
    for (const [i, n] of manifest.libs.entries()) {
      if (images[n]) libs[i].program = images[n];
    }
    return { image: null, program: manifest.program, libs, pins: manifest.pins };
  }
  const AT_A_TIME = 4;
  const libs = [];
  const began = performance.now();
  for (let i = 0; i < manifest.libs.length; i += AT_A_TIME) {
    const batch = manifest.libs.slice(i, i + AT_A_TIME);
    const done = await Promise.all(
      batch.map(async (n) => {
        const t0 = performance.now();
        // `encodeURI`, not `encodeURIComponent`: a soname like `libstdc++.so.6`
        // has path-legal characters that `encodeURIComponent` escapes anyway,
        // and Vite's dev server does not decode `%2B` back -- the request then
        // misses and the SPA fallback answers `index.html` with status 200.
        const bytes = await fetchBytes(`${base}/${encodeURI(n)}`);
        const fetched = performance.now();
        // Compiled once here and structured-cloned to every Worker: a
        // `WebAssembly.Module` clones as itself with the engine sharing the code
        // behind it, so a Worker needs only the module and the `gex.meta`
        // section saying which pages it covers.
        const mod = await WebAssembly.compile(bytes);
        log(
          `closure ${n}: ${(bytes.length / 1048576).toFixed(1)} MiB, `
          + `${Math.round(fetched - t0)}ms fetching, `
          + `${Math.round(performance.now() - fetched)}ms compiling`,
        );
        return { meta: metadataSection(bytes), mod };
      }),
    );
    libs.push(...done);
  }
  log(`closure ready: ${libs.length} libraries in ${Math.round(performance.now() - began)}ms`);

  // The image module is one program's own text, so it may only stand in for
  // pid 1 when pid 1 *is* that program -- handing a shell apt-get's module
  // starts it inside another program's code and it exits without a syscall.
  // `image` is the path about to run as pid 1; check it against the program the
  // manifest says the module was built from rather than trust it, since the
  // failure is silent. A manifest with no image module is the `--shared-image`
  // shape, where the program's text is an ordinary library in `libs`.
  let img = null;
  if (image && manifest.image) {
    const want = typeof image === 'string' ? image : manifest.program;
    if (manifest.program && want !== manifest.program) {
      log(`closure: image module is ${manifest.program}, not ${want} -- running without it`);
    } else {
      img = await fetchBytes(`${base}/${encodeURI(manifest.image)}`);
    }
  }
  // `images` maps a library file to the program whose image it is, so the shim
  // can decide per process whether the pages at `PIE_BASE` belong to it.
  const images = manifest.images ?? {};
  for (const [i, n] of manifest.libs.entries()) {
    if (images[n]) libs[i].program = images[n];
  }
  return { image: img, program: manifest.program, libs, pins: manifest.pins };
}
