/**
 * Do a Worker's fetch notes reach its host only, and in order?
 *
 * fetch-note.mjs says what a Worker fetches and installs. A process's Worker
 * hands it to its host (`setNoteSink`, worker.mjs) and never opens the
 * 'gex-fetch' channel, since every open channel is handed every other one's
 * messages; a realm with no host (a page's own Worker) still says it on the
 * channel. This runs both in worker threads, counting the channels each
 * opens:
 *
 *   - with a sink: every note in the sink, a fetch's start before its end,
 *     no channel opened, nothing on the channel;
 *   - without: the same notes on the channel, in order, one channel opened.
 *
 * Usage: node check-fetch-note.mjs
 */
import { Worker, isMainThread, workerData, parentPort } from 'node:worker_threads';

const N = 50;

if (isMainThread) {
  const heard = [];
  const ch = new BroadcastChannel('gex-fetch');
  ch.onmessage = (ev) => heard.push(ev.data);
  const run = (sink) => new Promise((resolve, reject) => {
    const w = new Worker(new URL(import.meta.url), { workerData: { sink } });
    w.once('message', resolve);
    w.once('error', reject);
  });
  let bad = 0;
  const check = (what, ok, more = '') => {
    if (!ok) bad++;
    console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}${ok ? '' : ` ${more}`}`);
  };
  const order = (notes) => {
    const started = new Set();
    for (const n of notes) {
      if (n.phase === 'start') started.add(n.id);
      else if (n.phase === 'end' && !started.has(n.id)) return false;
    }
    return true;
  };
  const a = await run(true);
  // A turn for anything the channel would still deliver.
  await new Promise((r) => setTimeout(r, 200));
  check('with a sink: every note in the sink', a.sunk.length === 2 * N + 1, `(${a.sunk.length} of ${2 * N + 1})`);
  check('with a sink: each start before its end', order(a.sunk));
  check('with a sink: no channel opened', a.opened === 0, `(${a.opened})`);
  check('with a sink: nothing on the channel', heard.length === 0, `(${heard.length})`);
  const b = await run(false);
  await new Promise((r) => setTimeout(r, 200));
  check('without: every note on the channel', heard.length === 2 * N + 1, `(${heard.length} of ${2 * N + 1})`);
  check('without: each start before its end', order(heard));
  check('without: one channel opened', b.opened === 1, `(${b.opened})`);
  ch.close();
  console.log(bad ? `${bad} failed` : 'fetch notes go to the host, or on the channel without one');
  process.exit(bad ? 1 : 0);
} else {
  let opened = 0;
  const Real = globalThis.BroadcastChannel;
  globalThis.BroadcastChannel = class extends Real {
    constructor(name) {
      super(name);
      opened++;
    }
  };
  const { noteFetch, noteStatus, setNoteSink } = await import('./fetch-note.mjs');
  const sunk = [];
  if (workerData.sink) setNoteSink((m) => sunk.push(m));
  for (let i = 0; i < N; i++) noteFetch(`unit:${i}`, 10)(10);
  noteStatus({ kind: 'module', path: '/lib/x.so' });
  parentPort.postMessage({ sunk, opened });
}
