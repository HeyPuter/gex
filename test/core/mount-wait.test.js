// A program's call on a slow mount waits in that program alone: the others
// go on meanwhile, two programs can wait on it at once, and each gets its own
// answer (README "Mounts": "in its own thread only").
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { MemFs } from '../fixtures/memfs.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 120 };

export default async function (t) {
  const { check, eq, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  const slow = new MemFs({ '/a': 'first\n', '/b': 'second\n' }, { delay: 1500 });
  fs.mount('/mnt/slow', slow);
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin:/usr/bin', HOME: '/root' };
  // Two programs on the mount, each call of theirs 1.5 s.
  const t0 = performance.now();
  const a = sys.run(['cat', '/mnt/slow/a'], { env });
  const b = sys.run(['cat', '/mnt/slow/b'], { env });
  // Meanwhile, programs that never touch it.
  await new Promise((r) => setTimeout(r, 300));
  const t1 = performance.now();
  const quick = [];
  for (let i = 0; i < 3; i++) quick.push(await timeout(sys.run(['sh', '-c', `echo quick ${i}`], { env }), 30000, 'quick'));
  const quickMs = performance.now() - t1;
  const [ra, rb] = await timeout(Promise.all([a, b]), 60000, 'the slow reads');
  const bothMs = performance.now() - t0;
  eq('the others ran meanwhile', quick.map((r) => r.stdout), ['quick 0\n', 'quick 1\n', 'quick 2\n']);
  // Each slow cat is at least four calls (stat, open, read, close) of 1.5 s:
  // three quick programs that waited on one would take as long as it did.
  check(`and did not wait for it (${quickMs.toFixed(0)} ms for three, the slow ones ${bothMs.toFixed(0)} ms)`, quickMs < bothMs / 2, '');
  eq('each slow read has its own answer', [ra.stdout, rb.stdout], ['first\n', 'second\n']);
  // Both waited at once, not one after the other.
  check(`the two waited together (${bothMs.toFixed(0)} ms)`, bothMs < 2 * slow.calls.filter((c) => c.endsWith(' /a')).length * 1500, JSON.stringify(slow.calls));
  await sys.shutdown();
}
