// An init given to boot() runs as pid 1 and owns the processes:
// the page spawns nothing, the init's children run and it collects them and
// their orphans, and when it ends the system stops.
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 120 };

export default async function (t) {
  const { check, eq, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new Filesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  // The init, with arguments and an environment as `spawn` takes them: its
  // child says what it was given, exits 7 and leaves a grandchild that
  // exits 5 after it.
  const cmd = 'echo "args $0 $INIT_SAYS"; sh -c "sleep 0.3; exit 5" > /dev/null 2>&1 & echo $! > /tmp/orphan; exit 7';
  check('boot checks its arguments as spawn does', (() => { try { sys.boot(['rt-probe'], { nope: 1 }); return false; } catch (e) { return /boot: no option nope/.test(e.message); } })());
  await timeout(sys.boot(['/bin/rt-probe', 'init', cmd], { env: { PATH: '/bin:/usr/bin', INIT_SAYS: 'hello' } }), 60000, 'boot');
  eq('init is pid 1', await timeout(sys.init.started, 10000, 'init'), 1);
  let threw = null;
  try { sys.spawn(['true']); } catch (e) { threw = e; }
  check('spawn throws under an init', threw && /has an init/.test(threw.message), String(threw));

  // What init says, line by line.
  const said = [];
  (async () => {
    let rest = '';
    for (const r = sys.init.stdout.getReader(); ;) {
      const { value, done } = await r.read();
      if (done) break;
      rest += new TextDecoder().decode(value);
      const lines = rest.split('\n');
      rest = lines.pop();
      said.push(...lines);
    }
  })();
  const until = async (f, what) => {
    for (let i = 0; i < 200 && !f(); i++) await new Promise((r) => setTimeout(r, 50));
    check(what, f(), JSON.stringify(said));
  };
  await until(() => said.includes('init 1'), 'init runs');
  await until(() => said.some((l) => l.startsWith('started ')), 'init starts its child');
  const child = said.find((l) => l.startsWith('started ')).split(' ')[1];
  await until(() => said.includes('args sh hello'), 'the init has its arguments and environment');
  await until(() => said.includes(`reaped ${child} 7`), 'init collects its child');
  const orphan = (await fs.readFile('/tmp/orphan', { encoding: 'utf-8' })).trim();
  await until(() => said.includes(`reaped ${orphan} 5`), 'and the orphan it left');

  // init ends: so does the system.
  sys.init.kill('SIGTERM');
  for (let i = 0; i < 200 && sys.state.phase === 'running'; i++) await new Promise((r) => setTimeout(r, 50));
  eq('init ending stops the system', [sys.state.phase, sys.state.init], ['stopped', { code: 143, signal: 'SIGTERM' }]);
}
