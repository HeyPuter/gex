// Does the proxy stall on its own, with no guest in the way?
//
//   node web/test/wisp-probe.mjs [ws://127.0.0.1:5111/] [host] [path]
//
// `apt-get update` stalls mid-fetch (~100 kB in, dead for 318s, then fast).
// Everything on the guest's side has been excluded. This drives the same proxy
// and client library with no gex: a stall here is the proxy's or library's, not
// the runtime's.
import { ClientConnection } from '../node_modules/@mercuryworkshop/wisp-js/src/client/connection.mjs';

// `--spin` keeps the event loop as busy as gex's Worker does. The plain probe
// moves 11.4 MiB in 1.2s; gex stalls 318s on the same fetch. If starving the
// loop the same way reproduces the stall, that is the mechanism. Two flavours:
// `--spin` posts through a MessageChannel (as now), `--spin=timer` uses
// setTimeout(_, 1) (as before).
const spin = process.argv.includes('--spin') ? 'turn'
  : process.argv.includes('--spin=timer') ? 'timer'
  : null;
const argv = process.argv.filter((a) => !a.startsWith('--spin'));

const url = argv[2] ?? 'ws://127.0.0.1:5111/';
const host = argv[3] ?? 'deb.debian.org';
const path = argv[4] ?? '/debian/dists/bookworm/main/binary-arm64/Packages.gz';

const conn = new ClientConnection(url);
await new Promise((resolve, reject) => {
  conn.onopen = resolve;
  conn.onerror = reject;
});

const began = performance.now();
const at = () => ((performance.now() - began) / 1000).toFixed(1);
let bytes = 0;
let messages = 0;
let last = performance.now();
let worst = 0;
let spinTurns = 0;

const stream = conn.create_stream(host, 80, 'tcp');
stream.onmessage = (data) => {
  const now = performance.now();
  const gap = now - last;
  // Only gaps worth a line. Silence is the signal; a rate is what hid it.
  if (messages && gap > 1000) {
    console.log(`  nothing for ${(gap / 1000).toFixed(1)}s after ${messages} messages, `
      + `${(bytes / 1048576).toFixed(1)} MiB`);
  }
  worst = Math.max(worst, messages ? gap : 0);
  last = now;
  messages += 1;
  bytes += data.length ?? data.byteLength;
};
stream.onclose = () => {
  console.log(`done at ${at()}s: ${messages} messages, ${(bytes / 1048576).toFixed(1)} MiB, `
    + `worst gap ${(worst / 1000).toFixed(1)}s, `
    + `${Math.round(bytes / ((performance.now() - began) / 1000) / 1024)} kB/s`
    + (spin ? `, ${spinTurns} spins` : ''));
  process.exit(0);
};

if (spin) {
  const { MessageChannel } = await import('node:worker_threads');
  const tick = new MessageChannel();
  tick.port1.on('message', () => {
    spinTurns += 1;
    if (spin === 'timer') setTimeout(() => tick.port2.postMessage(0), 1);
    else tick.port2.postMessage(0);
  });
  tick.port2.postMessage(0);
  console.log(`spinning the event loop as ${spin}`);
}

console.log(`asking ${host}${path} through ${url} at ${at()}s`);
stream.send(new TextEncoder().encode(
  `GET ${path} HTTP/1.0\r\nHost: ${host}\r\nConnection: close\r\n\r\n`,
));

// A bound, so a stall that never ends still reports rather than hanging here.
setTimeout(() => {
  console.log(`gave up at ${at()}s: ${messages} messages, ${(bytes / 1048576).toFixed(1)} MiB, `
    + `worst gap ${(worst / 1000).toFixed(1)}s`);
  process.exit(1);
}, 400_000);
