import { defineConfig } from 'vite';
import { fileURLToPath } from 'node:url';

// Cross-origin isolation, which is what a SharedArrayBuffer costs. The page
// needs one for the global kernel's shared memory and for any process whose
// threads are Workers. Without these headers SharedArrayBuffer is not a
// constructor, and the failure is at the first shared WebAssembly.Memory.
const ISOLATED = {
  'Cross-Origin-Opener-Policy': 'same-origin',
  'Cross-Origin-Embedder-Policy': 'require-corp',
};

export default defineConfig({
  server: {
    port: 5173,
    host: true,
    headers: ISOLATED,
    // The runtime lives outside this directory (in the shim), and a guest
    // process is a Worker started from a file there. `vite dev` refuses paths
    // above the project root with a 403, so widen the allow list.
    fs: { allow: [fileURLToPath(new URL('..', import.meta.url))] },
  },
  preview: { headers: ISOLATED },
  // gex-eject's feature selection (docs/eject.md, "Features"):
  // GEX_FEATURES_OFF=compositor,... builds the page without them, the code
  // behind each constant dropped from the bundle.
  define: {
    __GEX_COMPOSITOR__: JSON.stringify(!(process.env.GEX_FEATURES_OFF || '').split(',').includes('compositor')),
  },
  // web/build-all.sh builds each flavour's dist from its own staged public
  // tree: GEX_PUBLIC_DIR (default public/) into GEX_OUT_DIR (default dist/).
  publicDir: process.env.GEX_PUBLIC_DIR || 'public',
  build: { target: 'es2022', chunkSizeWarningLimit: 2048, outDir: process.env.GEX_OUT_DIR || 'dist' },
  // `worker.mjs` is an ES module with top-level await; the default `iife`
  // worker output cannot express that.
  worker: { format: 'es' },
  resolve: {
    alias: {
      // The page runs the same `gex.mjs` the tests do, from the shim it
      // belongs to, rather than a copy that could drift from it.
      '@gex': fileURLToPath(new URL('../shims/browser/js', import.meta.url)),
    },
  },
});
