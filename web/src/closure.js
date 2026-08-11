// Loading a statically recompiled closure into a page.
//
// A closure is what this project is for: the program and every library it
// reaches, translated ahead of time, so a run translates nothing. Until now
// only the harness page could load one, which meant the *site* -- the thing
// anyone actually opens -- had only ever been measured on the runtime
// translator, the secondary path. The two pages disagreed for days about
// whether `apt-get update` worked at all, and this is one of the reasons why.
//
// Shared rather than copied, so a fix to the fetch policy reaches both.
import { fetchBytes } from './runtime.js';
import { metadataSection } from '../../shims/browser/js/gex.mjs';

/**
 * Fetch and compile a closure described by a manifest.
 *
 * `base` is the directory the manifest and its modules are served from, and
 * the manifest is `{ image, libs, pins }` as `gexc build --closure` writes it.
 *
 * Returns `{ image, libs, pins }` ready to hand to `runProgram`, or `null`
 * when there is no manifest there -- a site without a closure built for it
 * should run exactly as it did before rather than fail to boot.
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

  // A few at a time, not all of them at once.
  //
  // `Promise.all` over the lot had every library in flight together: 694 MB
  // of wasm fetched and handed to `WebAssembly.compile` concurrently, on the
  // main thread, before the guest had run an instruction. A 77 MB closure
  // booted a shell in 1.65s; the 694 MB one never became ready at all, and
  // nothing said whether it was fetching, compiling or installing.
  //
  // Four is a guess; the timings logged below are how to improve on it. What
  // is not a guess is that unbounded was wrong -- the peak is what matters,
  // and the peak was all of it.
  // `?libs=N` loads only the first N libraries.
  //
  // For bisecting. A closure library's slot numbers are absolute and baked in
  // at build time, so leaving some out is safe: the pages they would claim
  // are simply never claimed, and the runtime translator meets that code the
  // way it did before any of this existed. Which makes "which library breaks
  // apt" a binary search over thirty-three, at thirty seconds a question,
  // instead of a ten-minute closure rebuild per guess.
  const only = Number(new URLSearchParams(location.search).get('libs') || 0);
  if (only > 0) {
    manifest.libs = manifest.libs.slice(0, only);
    if (manifest.metas) manifest.metas = manifest.metas.slice(0, only);
  }

  // The metadata alone, when the build wrote it out separately.
  //
  // Registering a library needs which pages it covers and how far into the
  // slot space it reaches -- a few kilobytes. The module is wanted by the
  // branch that lands in it, and `wantLibrary` fetches it then. Fetching and
  // compiling all of it first meant a 239 MB download and a million blocks
  // of wasm compiled before pid 1 ran an instruction: an `ls` enters three
  // of sixty-two libraries and waited for all of them.
  //
  // A manifest without `metas` is one built before this existed, and takes
  // the old path below.
  if (manifest.metas && manifest.metas.every((m) => m)) {
    const began = performance.now();
    const libs = [];
    for (let i = 0; i < manifest.libs.length; i += 8) {
      const batch = manifest.libs.slice(i, i + 8);
      const done = await Promise.all(
        batch.map(async (n, j) => ({
          meta: await fetchBytes(`${base}/${encodeURI(manifest.metas[i + j])}`),
          // Where to get the module, for the host to fetch on demand.
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
        // `encodeURI`, not `encodeURIComponent`.
        //
        // A module is named after its soname, and `libstdc++.so.6` has two
        // characters that are perfectly legal in a URL path and that
        // `encodeURIComponent` escapes anyway. Vite's dev server does not
        // decode `%2B` back, so the request missed and its SPA fallback
        // answered `index.html` -- with status *200*, which is why the `ok`
        // check above let it through and `WebAssembly.compile` reported
        // "expected magic word 00 61 73 6d, found 3c 21 64 6f". Those four
        // bytes are `<!do`.
        //
        // Invisible to every test here, because `web/test/site.mjs` serves
        // through its own static server and that one calls
        // `decodeURIComponent`. The demo anyone actually opens is the dev
        // server, and it was the only thing that broke.
        const bytes = await fetchBytes(`${base}/${encodeURI(n)}`);
        const fetched = performance.now();
        // Compiled once here and structured-cloned to every Worker. A
        // `WebAssembly.Module` clones as itself and the engine shares the
        // code behind it, so a Worker needs only the module and the small
        // `gex.meta` section saying which pages it covers.
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

  // The image module is one program's own text.
  //
  // Only when the caller knows pid 1 *is* that program. Handing a shell the
  // module built for `apt-get` starts it inside another program's code, and
  // it exits without making a syscall -- which is exactly what the first
  // attempt at this did.
  //
  // `image` is the guest path the caller is about to run as pid 1, and the
  // manifest says which program the module was built from. Checked rather
  // than trusted: the failure is silent and looks like a program that decided
  // to do nothing, so a mismatch is worth a line and no image module.
  //
  // A manifest with no image module at all is the `--shared-image` shape: the
  // program's own text is an ordinary library in `libs`, registered for its
  // pages and instantiated by the first branch that lands in one, and the
  // seed stays the module that owns the memory and the tables. Asking for
  // `manifest.image` there fetched the string "null" and 404ed.
  let img = null;
  if (image && manifest.image) {
    const want = typeof image === 'string' ? image : manifest.program;
    if (manifest.program && want !== manifest.program) {
      log(`closure: image module is ${manifest.program}, not ${want} -- running without it`);
    } else {
      img = await fetchBytes(`${base}/${encodeURI(manifest.image)}`);
    }
  }
  // `images` maps a library file to the program whose image it is. The
  // loader passes it through so the shim can decide, per process, whether the
  // pages at `PIE_BASE` belong to the program actually running.
  const images = manifest.images ?? {};
  for (const [i, n] of manifest.libs.entries()) {
    if (images[n]) libs[i].program = images[n];
  }
  return { image: img, program: manifest.program, libs, pins: manifest.pins };
}
