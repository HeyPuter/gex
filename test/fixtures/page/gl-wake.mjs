/**
 * Does a guest's GL submit post no wake-up message to a device that waits on
 * the channel itself?
 *
 * The device (`serveGl`) runs on one thread and its clients (`GlProxy`) on
 * others, each with its own structured clone of the channel. A device whose
 * thread has `Atomics.waitAsync` serves requests from the control word and
 * needs no `{kind: 'gl'}` message per submit; a client that cannot tell posted
 * one for every submit, a message task on the host's thread each time.
 *
 * This serves a channel from this thread with a stand-in device and runs a
 * client in a Worker, given the channel as a message: the client's batches
 * must all be answered and its wake callback never called. Then the same
 * without `Atomics.waitAsync` on the device's side, served by the wake-ups
 * alone: every submit wakes, and all are answered.
 *
 * Usage: node test/fixtures/page/gl-wake.mjs
 */
import { Worker, isMainThread, parentPort } from 'node:worker_threads';
import { pathToFileURL as gexPathToFileURL } from 'node:url';
import { join as gexJoin } from 'node:path';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || gexJoin(import.meta.dirname, '..', '..', '..');
const gexUrl = (p) => gexPathToFileURL(gexJoin(GEX_ROOT, p)).href;
const { makeGlChannel, serveGl, GlProxy } = await import(gexUrl('hosts/browser/js/gl-bridge.mjs'));

const SUBMITS = 200;
const RING = 4096;

if (isMainThread) {
  const run = async (direct) => {
    const channel = makeGlChannel();
    let served = 0;
    // The stand-in device: an empty reply to every batch.
    const device = {
      submit(memory, ring) {
        new Uint32Array(memory.buffer, ring, 8)[4] = 0;
        served++;
        return 0;
      },
    };
    const waitAsync = Atomics.waitAsync;
    if (!direct) Atomics.waitAsync = undefined;
    let serve;
    try {
      serve = serveGl(device, channel);
    } finally {
      Atomics.waitAsync = waitAsync;
    }
    const w = new Worker(new URL(import.meta.url));
    const done = new Promise((resolve, reject) => {
      w.on('message', (m) => {
        if (m.kind === 'gl') serve();
        else if (m.kind === 'done') resolve(m);
      });
      w.on('error', reject);
    });
    w.postMessage({ channel });
    const r = await done;
    serve.stop();
    await w.terminate();
    return { ...r, served };
  };
  const a = await run(true);
  if (a.failed || a.served !== SUBMITS) throw new Error(`waitAsync device: ${a.served} of ${SUBMITS} batches served, ${a.failed} failed`);
  if (a.wakes !== 0) throw new Error(`waitAsync device: the client posted ${a.wakes} wake-ups for ${SUBMITS} submits (want 0)`);
  const b = await run(false);
  if (b.failed || b.served !== SUBMITS) throw new Error(`message-driven device: ${b.served} of ${SUBMITS} batches served, ${b.failed} failed`);
  if (b.wakes !== SUBMITS) throw new Error(`message-driven device: ${b.wakes} wake-ups for ${SUBMITS} submits (want ${SUBMITS})`);
  console.log(`ok: ${SUBMITS} submits, 0 wake-ups to a waitAsync device, ${b.wakes} to a message-driven one`);
} else {
  parentPort.once('message', ({ channel }) => {
    let wakes = 0;
    const proxy = new GlProxy(channel, () => {
      wakes++;
      parentPort.postMessage({ kind: 'gl' });
    });
    const memory = new WebAssembly.Memory({ initial: 1, maximum: 1, shared: true });
    const head = new Uint32Array(memory.buffer, RING, 8);
    let failed = 0;
    for (let i = 0; i < SUBMITS; i++) {
      head[1] = 64; // capacity
      head[2] = 16; // tail: a batch of 16 bytes
      if (proxy.submit(memory, RING, 1) !== 0) failed++;
    }
    parentPort.postMessage({ kind: 'done', wakes, failed });
  });
}
