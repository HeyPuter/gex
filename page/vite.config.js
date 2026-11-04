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

// GEX_PAGE_MODE=mp|sp: the page carries one build's code (hosts/browser/js/mode.mjs),
// the other's dropped from the bundle, as page/runtime/build.sh does for the
// packages. page/build.sh sets it per dist. Unset (vite dev): both.
const MODE = process.env.GEX_PAGE_MODE || '';
if (MODE && MODE !== 'mp' && MODE !== 'sp') throw new Error(`GEX_PAGE_MODE=${MODE}: mp or sp`);
const pageMode = {
  name: 'gex-page-mode',
  enforce: 'pre',
  load: (id) => MODE && /\/shims\/browser\/js\/mode\.mjs$/.test(id.split('?')[0])
    ? `export const SP_CODE = ${MODE === 'sp'};\nexport const MP_CODE = ${MODE === 'mp'};\n`
    : null,
  // A Worker's chunk is emitted wherever `new Worker(new URL(...))` appears,
  // dead branch or not, so the MP page drops the single-thread system
  // Worker's construction here (kernel.js singleWorker throws before it).
  transform(code, id) {
    if (MODE !== 'mp' || !/\/runtime\/src\/kernel\.js$/.test(id.split('?')[0])) return null;
    const single = "new Worker(new URL('./kernel-single-worker.js', import.meta.url), { type: 'module', name: 'gex-system' })";
    if (!code.includes(single)) this.error('kernel.js no longer starts the single-thread Worker as vite.config.js expects');
    return code.replace(single, 'null');
  },
};

export default defineConfig({
  plugins: [pageMode],
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
  // gex-eject's feature selection (crates/gex-eject/README.md):
  // GEX_FEATURES_OFF=compositor,... builds the page without them, the code
  // behind each constant dropped from the bundle.
  define: {
    __GEX_COMPOSITOR__: JSON.stringify(!(process.env.GEX_FEATURES_OFF || '').split(',').includes('compositor')),
  },
  // page/build.sh builds each flavour's dist from its own staged public
  // tree: GEX_PUBLIC_DIR (default public/) into GEX_OUT_DIR (default dist/).
  publicDir: process.env.GEX_PUBLIC_DIR || 'public',
  build: {
    target: 'es2022',
    chunkSizeWarningLimit: 2048,
    outDir: process.env.GEX_OUT_DIR || 'dist',
  },
  // `worker.mjs` is an ES module with top-level await; the default `iife`
  // worker output cannot express that.
  //
  // Each Worker is one file, its dynamic imports inlined. Split, the chunks a
  // Worker imports lazily take the helpers they share from the Worker's own
  // file, an import cycle (worker -> wl-bridge -> worker). The page starts
  // every guest Worker from one fetched copy of its module graph
  // (worker-graph.js), which a cycle defeats: each Worker then fetched its
  // files again, ~27 MB a desktop start on an origin without an HTTP cache.
  worker: { format: 'es', plugins: () => [pageMode], rolldownOptions: { output: { inlineDynamicImports: true } } },
  resolve: {
    alias: {
      // The page runs the same `gex.mjs` the tests do, from the shim it
      // belongs to, rather than a copy that could drift from it.
      '@gex': fileURLToPath(new URL('../hosts/browser/js', import.meta.url)),
    },
  },
});
