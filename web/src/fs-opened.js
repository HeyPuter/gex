// The files a run has opened in the global kernel's filesystem (TmpFs's
// `opened`, shims/kernel `k_fs_opened`): what gex-eject's W3 recorder
// (tools/eject/record-w3.mjs) keeps as the run's read set, so that the site
// can ship those in its bundle and serve the rest as lazy files
// (docs/eject.md, "The bundle split").
//
// A Worker asks, not the page: the kernel's locks may wait
// (memory.atomic.wait), which the main thread may not.

/** One ask at a time: every one runs on the same kernel stack slot
 *  (fs-opened-worker.js), so two at once would share a stack. */
let asking = Promise.resolve();

/** The opened paths, sorted; `spec` is runtime.js's `globalKernel()`. */
export function openedFiles(spec) {
  const ask = asking.then(() => askOnce(spec));
  asking = ask.catch(() => {});
  return ask;
}

function askOnce(spec) {
  if (!spec?.module || !spec?.memory) return Promise.reject(new Error('no global kernel (no SharedArrayBuffer)'));
  const worker = new Worker(new URL('./fs-opened-worker.js', import.meta.url), { type: 'module' });
  return new Promise((resolve, reject) => {
    worker.onmessage = (e) => {
      worker.terminate();
      if (e.data.error) reject(new Error(e.data.error));
      else resolve(e.data.paths);
    };
    worker.onerror = (e) => { worker.terminate(); reject(new Error(e.message)); };
    worker.postMessage({ module: spec.module, memory: spec.memory });
  });
}
