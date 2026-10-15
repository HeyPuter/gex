// What a Worker fetches while the program runs -- a lazy file's chunk, a unit
// of translated code -- said to the page (app.js `onProgress`'s `fetch`), so
// it can show what the program is waiting for; and what it starts (`noteStatus`:
// a program, an image's unit), for the page's start-up status. A BroadcastChannel: the
// Worker that fetches is blocked in a synchronous request, and the page hears
// the start while it waits. Nothing listens unless a page asked.

const channel = typeof BroadcastChannel === 'function' ? new BroadcastChannel('gex-fetch') : null;
// Under Node an open channel keeps the event loop alive: a run with no Workers to terminate
// (the single-thread build, run-node.mjs) printed its output and never exited.
channel?.unref?.();
let next = 0;

/** A fetch starts: `what` (a URL or a unit cache path), `bytes` expected (0
 *  if not known). Returns the end call: `done(bytesGot)`, which says it once. */
export function noteFetch(what, bytes = 0) {
  if (!channel) return () => {};
  const id = `${globalThis.name ?? ''}:${next++}:${Math.random().toString(36).slice(2, 8)}`;
  try { channel.postMessage({ id, what, bytes, phase: 'start' }); } catch {}
  let ended = false;
  return (got = 0) => {
    if (ended) return;
    ended = true;
    try { channel.postMessage({ id, what, bytes: got, phase: 'end' }); } catch {}
  };
}

/** Something the program reached, for the page's start-up status (app.js
 *  `onProgress`'s `boot` and `log`): `{ kind: 'unit', key, path, hit,
 *  program }` when an image's function-tier unit is installed in a process
 *  (`program`: the process's command line, when the image is its .exe). */
export function noteStatus(status) {
  if (!channel) return;
  try { channel.postMessage({ status, t: performance.now() + performance.timeOrigin }); } catch {}
}
