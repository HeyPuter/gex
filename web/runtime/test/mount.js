// fs.mount: the page's own filesystems in the guest's tree, before boot and
// while it runs.
import { System } from '../src/index.js';
import { Filesystem } from '../src/fs.js';
import { main, say, check, eq, timeout, testDebug } from './lib.js';
import { MemFs } from './memfs.js';

main(async () => {
  const sys = new System({ baseUrl: '/', debug: testDebug() });
  const fs = new Filesystem(sys);
  fs.addTar('/__rt/bb.tar.zst');
  const big = new Uint8Array(1 << 20).map((_, i) => (i * 7) & 0xff);
  const early = new MemFs({ '/hello': 'hello from the page\n', '/sub/deep.txt': 'deep\n', '/big': big });
  fs.mount('/mnt/early', early);
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin:/usr/bin', HOME: '/root' };
  const sh = async (cmd) => (await timeout(sys.run(['sh', '-c', cmd], { env }), 30000, cmd));
  const out = async (cmd) => (await sh(cmd)).stdout;

  eq('the first program reads a mount made before boot', await out('cat /mnt/early/hello'), 'hello from the page\n');
  eq('cat', await out('cat /mnt/early/hello'), 'hello from the page\n');
  eq('ls', (await out('ls /mnt/early')).split('\n').filter(Boolean), ['big', 'hello', 'sub']);
  eq('a listing\'s types', (await out('ls -l /mnt/early | grep -c "^d"')).trim(), '1');
  eq('a nested file', await out('cat /mnt/early/sub/deep.txt'), 'deep\n');
  await sh('cat /mnt/early/big > /tmp/big');
  const copy = await fs.readFile('/tmp/big');
  check('1 MiB read in chunks', copy.length === big.length && copy.every((v, i) => v === big[i]), `${copy.length} bytes`);
  eq('stat size', (await out('wc -c < /mnt/early/big')).trim(), String(big.length));

  // Writes reach the provider.
  const w = await sh('echo written > /mnt/early/new && echo more >> /mnt/early/new && mkdir /mnt/early/d && mv /mnt/early/new /mnt/early/d/moved && rm /mnt/early/hello && ls /mnt/early/d');
  eq('write, append, mkdir, mv, rm', [w.code, w.stdout], [0, 'moved\n']);
  eq('the provider has them', [early.text('/d/moved'), early.nodes.has('/hello')], ['written\nmore\n', false]);
  eq('a missing file', (await sh('cat /mnt/early/nope')).code, 1);
  eq('rmdir of a full directory', (await sh('rmdir /mnt/early/d')).code !== 0, true);
  eq('mv across mounts copies', (await sh('mv /mnt/early/d/moved /tmp/moved && cat /tmp/moved && ls /mnt/early/d | wc -l')).stdout, 'written\nmore\n0\n');
  eq('the page reads through it', await fs.readFile('/mnt/early/sub/deep.txt', { encoding: 'utf-8' }), 'deep\n');
  await fs.writeFile('/mnt/early/from-page', 'page wrote\n');
  eq('the page writes through it', early.text('/from-page'), 'page wrote\n');

  // A mount after boot reaches a shell already running.
  const p = sys.spawn(['sh'], { pty: { cols: 80, rows: 24, encoding: 'utf-8' }, env: { ...env, PS1: '$ ' } });
  let screen = '';
  p.pty.addEventListener('data', (e) => { screen += e.data; });
  await p.started;
  p.pty.write('echo up\n');
  for (let i = 0; i < 200 && !/up\r?\n/.test(screen); i++) await new Promise((r) => setTimeout(r, 25));
  const late = new MemFs({ '/late.txt': 'mounted late\n' }, { delay: 400 });
  await fs.mount('/mnt/late', late).ready;
  p.pty.write('cat /mnt/late/late.txt\n');
  // A slow provider blocks only the call waiting on it.
  const t0 = performance.now();
  const quick = await out('echo quick');
  const tq = performance.now() - t0;
  for (let i = 0; i < 400 && !/mounted late/.test(screen); i++) await new Promise((r) => setTimeout(r, 25));
  check('a running shell sees a later mount', /mounted late/.test(screen), JSON.stringify(screen.slice(-200)));
  check(`another program runs while a slow provider answers (${tq.toFixed(0)} ms)`, quick === 'quick\n' && tq < 1500);
  p.pty.write('exit\n');
  await timeout(p.exited, 10000, 'exit');

  // Unmounted: the tree's own directory shows again.
  await fs.unmount('/mnt/late');
  eq('unmounted', (await sh('ls /mnt/late | wc -l')).stdout.trim(), '0');
  eq('mounts()', fs.mounts().map((m) => m.path), ['/mnt/early']);
  say(JSON.stringify({ calls: early.calls.length }));
  await sys.shutdown();
});
