import { defineConfig } from 'vite';
import { fileURLToPath } from 'node:url';

// Cross-origin isolation, which is what a `SharedArrayBuffer` costs.
//
// The page needs one for two separate reasons. The global kernel -- the
// process table, the filesystem and the pipes every guest process shares --
// is a wasm module every Worker instantiates against one shared memory, and
// there is no other kind of memory two Workers can both hold. And a process
// whose *threads* are Workers needs its own memory shared between them.
//
// Without these headers `SharedArrayBuffer` is not a constructor at all, and
// the failure is at the first `new WebAssembly.Memory({shared: true})` --
// nowhere near the feature that wanted it.
const ISOLATED = {
  'Cross-Origin-Opener-Policy': 'same-origin',
  'Cross-Origin-Embedder-Policy': 'require-corp',
};

export default defineConfig({
  server: { port: 5173, host: true, headers: ISOLATED },
  preview: { headers: ISOLATED },
  build: { target: 'es2022', chunkSizeWarningLimit: 2048 },
  // A guest process is a Worker, and `worker.mjs` is a module -- it imports
  // `gex.mjs` and awaits at the top level to work out whether it is under a
  // page or under Node. The default `iife` output cannot express either.
  worker: { format: 'es' },
  resolve: {
    alias: {
      // The page runs the same `gex.mjs` the tests do, from the shim it
      // belongs to, rather than a copy that could drift from it.
      '@gex': fileURLToPath(new URL('../shims/browser/js', import.meta.url)),
    },
  },
});
