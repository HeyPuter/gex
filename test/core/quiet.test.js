// A system made with no debug switches says nothing for ordinary work: no
// line at the console's log, info, warning or error level (browser), none on
// the process's output (node). Programs run with pipes and on a terminal, a
// pipeline ends, a program is killed. A program that crashes is still said.
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 180 };

// What gex itself says: its own prefixes. Anything else at those levels on
// the page is the browser's or vite's, not the runtime's.
const GEX = /\[(gex|pty|xwm|trap)|gex-console|gex:/;

export default async function (t) {
  const { eq, timeout } = t;
  const browser = typeof document !== 'undefined';
  const noise = async () => {
    const lines = browser ? await t.logs(/^(log|info|warning|error) /, { levels: true }) : await t.logs(/./);
    return lines.filter((l) => GEX.test(l));
  };
  // t.debug(): only what the run needs (vite's Workers, the arch); no `log`.
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new Filesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin:/usr/bin', HOME: '/root' };

  eq('a program with pipes', (await sys.run(['sh', '-c', 'echo hi; ls /bin | wc -l > /dev/null'], { env })).stdout, 'hi\n');
  const sh = sys.spawn(['sh'], { pty: { cols: 80, rows: 24 }, env });
  let said = '';
  sh.pty.addEventListener('data', (e) => { said += new TextDecoder().decode(e.data); });
  sh.pty.write('echo on-the-pty; ls / > /dev/null; sleep 10 & kill $!; wait; exit\n');
  eq('a terminal session ends', await timeout(sh.exited, 30000, 'the terminal'), { code: 0, signal: null });
  t.check('and said its line', said.includes('on-the-pty'), JSON.stringify(said.slice(-200)));
  const before = await noise();
  eq('nothing said for it', before, []);

  // A pipeline's ordinary end: `yes` dies of SIGPIPE.
  eq('a pipeline ends', (await sys.run(['sh', '-c', 'yes | head -1'], { env })).stdout, 'y\n');
  await new Promise((r) => setTimeout(r, 500));
  const after = (await noise()).slice(before.length);
  t.known('nothing said for a SIGPIPE', after.length === 0, JSON.stringify(after.slice(0, 3)),
    'the crash report for SIGPIPE goes quiet with kc/sigpipe-quiet bd0f8073');
  // A real crash is still said: the default keeps errors.
  const seg = await sys.run(['rt-probe', 'trap'], { env });
  t.check('a program that traps dies of a signal', !!seg.signal, JSON.stringify(seg));
  await new Promise((r) => setTimeout(r, 500));
  const crash = (await noise()).slice(before.length + after.length);
  t.check('and its crash is said', crash.some((l) => /signal|trap/.test(l)), JSON.stringify(crash.slice(0, 3)));
  await sys.shutdown();
}
