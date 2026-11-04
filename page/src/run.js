// `runProgram`, the page's: one program as pid 1 of a system of its own.
// Here and not in runtime.js, which every Worker takes in: through it a
// Worker would take in the runtime API's System (run-program.js), and that
// takes in runtime.js again, a cycle of Workers a bundler refuses.

import { SP_CODE } from '../../hosts/browser/js/mode.mjs';
import { singleMode } from './runtime.js';

/**
 * Run one program, `opts.args[0]`, as pid 1 of a system of its own, its
 * terminal `term` (an xterm, or anything with its `write`, `onData`,
 * `onResize`, `rows` and `cols`): a preset over the runtime API
 * (run-program.js), in either build. Resolves `{ status }` when the program
 * ends.
 */
export async function runProgram(term, opts) {
  // Said once, so a person (and the smoke test) can tell which build ran; by
  // the page's preset, not the runtime API, which says nothing by default.
  // On its terminal too, as the page's old single-thread run said it.
  if ((SP_CODE && singleMode())) {
    console.info(`[gex] single-thread build: the system runs in a Worker, no SharedArrayBuffer (crossOriginIsolated=${globalThis.crossOriginIsolated})`);
    term.writeln?.('gex: single-thread build (no SharedArrayBuffer): every guest thread in one Worker');
  }
  const { runPreset } = await import('./run-program.js');
  return runPreset(term, opts);
}

