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
  const AT_A_TIME = 4;
  const libs = [];
  const began = performance.now();
  for (let i = 0; i < manifest.libs.length; i += AT_A_TIME) {
    const batch = manifest.libs.slice(i, i + AT_A_TIME);
    const done = await Promise.all(
      batch.map(async (n) => {
        const t0 = performance.now();
        const bytes = await fetchBytes(`${base}/${encodeURIComponent(n)}`);
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
  const img = image ? await fetchBytes(`${base}/${encodeURIComponent(manifest.image)}`) : null;
  return { image: img, libs, pins: manifest.pins };
}
