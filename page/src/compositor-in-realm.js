// The scene compositor in the single-thread build's system Worker
// (compositor-worker.js's `inRealm`): it reads every guest's pixels out of
// memories only that Worker has. The page keeps the canvas's listeners
// (compositor.js, with `makeWorker` a stand-in that relays) and sends what it
// would post to a compositor Worker; what that Worker would post back goes to
// `post`.

/** `post(m, transfer)`: to the page's compositor. Resolves `{ channel, stats,
 *  send, compositor }`: the Wayland channel the processes' host is given, the
 *  counters the page reads (`stats()`, a copy), the page's messages in
 *  (`send`), and what the host calls on a compositor. */
export async function openCompositor(width, height, scale, post) {
  globalThis.GEX_COMPOSITOR_IN_REALM = true;
  const [{ inRealm }, { makeWlChannel, setOutputSize }] = await Promise.all([
    // gex-eject's `"compositor": false` builds drop it (runtime.js, GEX_COMPOSITOR).
    (typeof __GEX_COMPOSITOR__ === 'undefined' || __GEX_COMPOSITOR__)
      ? import('./compositor-worker.js')
      : Promise.reject(new Error('this build has no compositor')),
    import('../../hosts/browser/js/wl-bridge.mjs'),
  ]);
  const channel = makeWlChannel();
  setOutputSize(channel, width, height, Math.max(1, Math.round(scale ?? 1)));
  const stats = new Int32Array(4);
  const toWorker = inRealm(post);
  // The page's own stats and channel mean nothing here: these are the ones
  // the guests write.
  const send = (m) => toWorker(m?.kind === 'init' ? { ...m, channel, stats: stats.buffer } : m);
  return {
    channel,
    stats: () => stats.slice(),
    send,
    compositor: {
      addMemory: (token, memory, kernel) => toWorker({ kind: 'memory', token, memory, kernel }),
      dropMemory: (token) => toWorker({ kind: 'drop-memory', token }),
      stop: () => toWorker({ kind: 'stop' }),
    },
  };
}
