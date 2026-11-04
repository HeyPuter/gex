// What a Worker fetches while the program runs -- a lazy file's chunk, a unit
// of translated code -- said to the page (app.js `onProgress`'s `fetch`), so
// it can show what the program is waiting for; and what it starts (`noteStatus`:
// a program, an image's unit), for the page's start-up status. A BroadcastChannel: the
// Worker that fetches is blocked in a synchronous request, and the page hears
// the start while it waits. Nothing listens unless a page asked.
//
// A process's Worker says it to its host instead (`setNoteSink`), which says
// it on the channel for the page (host.mjs `relayNote`): every open channel
// is also handed every other one's messages, and with a channel in each of
// 30 to 60 Workers each note was cloned to and woke every one of them.

let channel;
let sink = null;
let next = 0;

/** Where this realm's notes go instead of the channel: a Worker's port to
 *  its host (worker.mjs). */
export function setNoteSink(f) {
  sink = f;
}

function post(m) {
  if (sink) return sink(m);
  if (channel === undefined) {
    channel = typeof BroadcastChannel === 'function' ? new BroadcastChannel('gex-fetch') : null;
    // Under Node an open channel keeps the event loop alive: a run with no Workers to terminate
    // (the single-thread build, run-node.mjs) printed its output and never exited.
    channel?.unref?.();
  }
  channel?.postMessage(m);
}

/** A fetch starts: `what` (a URL or a unit cache path), `bytes` expected (0
 *  if not known), `owner` the fetching process's .exe ('' if not known yet).
 *  Returns the end call: `done(bytesGot)`, which says it once. */
export function noteFetch(what, bytes = 0, owner = '') {
  if (!sink && typeof BroadcastChannel !== 'function') return () => {};
  const id = `${globalThis.name ?? ''}:${next++}:${Math.random().toString(36).slice(2, 8)}`;
  try { post({ id, what, bytes, owner, phase: 'start' }); } catch {}
  let ended = false;
  return (got = 0) => {
    if (ended) return;
    ended = true;
    try { post({ id, what, bytes: got, phase: 'end' }); } catch {}
  };
}

/** Something the program reached, for the page's start-up status (app.js
 *  `onProgress`'s `boot` and `log`): `{ kind: 'unit', key, path, hit,
 *  program }` when an image's function-tier unit is installed in a process,
 *  `{ kind: 'module', path, program }` when a native wasm32 module (a Wine
 *  builtin) is (`program`: the process's command line, when the image is
 *  its .exe). */
export function noteStatus(status) {
  if (!sink && typeof BroadcastChannel !== 'function') return;
  try { post({ status, t: performance.now() + performance.timeOrigin }); } catch {}
}
