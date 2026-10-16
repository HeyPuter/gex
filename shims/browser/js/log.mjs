// What gex says on the console by default, and the switch for the rest.
//
// By default the page runtime says only: a process starting (and the file it
// runs), a library loaded for the first time in this session, and what went
// wrong. Everything else -- Worker hand-offs, fork costs, per-process exit
// numbers, unit hits, the shim's narration -- is debug output behind
// `GEX_LOG`: `?log=1` on the page (or `extra.log`, which every Worker is
// handed), the GEX_LOG environment variable under Node, as natively.
//
//   GEX_LOG=1 (or all)   everything, as before
//   GEX_LOG=debug        the narration (Worker hand-offs, fork costs, the shim's lines)
//   GEX_LOG=record       the `[gex-record]` lines (tools/eject/record-w3.mjs)
//   GEX_LOG=unit         a line per function-tier unit placed in a process
//   GEX_LOG=a,b          several of those
//   GEX_LOG=off          not even the default lines (tests that compare a
//                        program's output with its native run)

// The page's own `?log=` (a Worker's `location` is its script's, so Workers
// are handed it in their start message instead).
let fromQuery;
function query() {
  if (fromQuery === undefined) {
    fromQuery = null;
    try {
      if (typeof window !== 'undefined' && window.location?.search) fromQuery = new URLSearchParams(window.location.search).get('log');
    } catch {}
  }
  return fromQuery;
}

function value() {
  const v = globalThis.GEX_LOG ?? globalThis.process?.env?.GEX_LOG ?? query();
  return v === undefined || v === null || v === '' || v === '0' ? null : String(v);
}

/** `GEX_LOG=off` (or `none`): the default lines are off too. */
export function logQuiet() {
  const v = value();
  return v === 'off' || v === 'none';
}

/** Whether debug output of kind `what` (or any, without one) is on. */
export function logOn(what) {
  const v = value();
  if (v === null || v === 'off' || v === 'none') return false;
  if (v === '1' || v === 'all') return true;
  const kinds = v.split(',');
  return what === undefined ? kinds.length > 0 : kinds.includes(what);
}

/** Set from a Worker's start message (`extra.log`): a browser Worker sees
 *  neither the page's globals nor its query. */
export function setLog(v) {
  if (v !== undefined && v !== null && globalThis.GEX_LOG === undefined) globalThis.GEX_LOG = String(v);
}
