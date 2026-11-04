// Many short programs at once, each run() to its end (System.run): 32 calls
// to the page (/dev/gex-host) with a long answer read through a pipe beside
// them, and 64 programs that write a line and end, in 12 rounds each. What
// each wrote is all there when it has ended, under a loaded host too
// (run-exit-race: one in a few hundred came back empty).
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';

// Longer in the single-thread build: one thread runs every process.
export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 300, singleTimeout: 600 };

export default async function (t) {
  const { check, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin:/usr/bin', HOME: '/root' };
  const run = (argv) => timeout(sys.run(argv, { env }), 60000, argv.join(' '));
  const seen = new Set();
  sys.hostCalls.register('add', ({ a, b }) => { seen.add(a); return a + b; });
  sys.hostCalls.register('big', () => 'x'.repeat(300000));
  const ask = (i) => run(['sh', '-c', `exec 3<>/dev/gex-host; printf '%s\\000%s' add '{"a":${i},"b":1000}' >&3; cat <&3`]);
  let bad = [];
  for (let round = 0; round < 12; round++) {
    // And beside them one answer longer than a part, read through a pipe.
    const big = run(['sh', '-c', "exec 3<>/dev/gex-host; printf 'big\\000' >&3; cat <&3 | wc -c"]);
    seen.clear();
    const got = await Promise.all(Array.from({ length: 32 }, (_, i) => ask(i)));
    got.forEach((r, i) => {
      if (r.code !== 0 || r.stdout !== `\x00${1000 + i}`) bad.push({ round, i, called: seen.has(i), ...r });
    });
    const b = await big;
    if (b.code !== 0 || b.stdout.trim() !== String(1 + 300002)) bad.push({ round, i: 'big', ...b });
  }
  // And programs that write a line and end at once, 64 at a time: what one
  // wrote is all there when it has ended.
  let lost = [];
  for (let round = 0; round < 12; round++) {
    const got = await Promise.all(Array.from({ length: 64 }, (_, i) => run(['echo', `line ${i}`])));
    got.forEach((r, i) => {
      if (r.code !== 0 || r.stdout !== `line ${i}\n`) lost.push({ round, i, ...r });
    });
  }
  check(`12 rounds of 64 short programs at once, each one's output whole (${lost.length} not${lost.length ? `: ${JSON.stringify(lost.slice(0, 3))}` : ''})`, lost.length === 0);
  const shown = bad.slice(0, 3).map((b) => JSON.stringify({ ...b, stdout: b.stdout.slice(0, 40) })).join('; ');
  check(`12 rounds of 32 calls at once and a long answer, each answered (${bad.length} not${bad.length ? `: ${shown}` : ''})`, bad.length === 0);
  await sys.shutdown();
}
