// An exec'd child does not keep the memory it was forked with: the parent
// fills 384 MiB, forks, and the child execs a program that only sleeps. The
// host's resident size with that program running comes back within 256 MiB
// of the parent's alone. gex ran the new program while the old store, its
// linker and the fork's context still held the parent's copy, so every
// fork-then-exec kept a whole copy of its parent resident until it ended.
// Natively. The parent's size is read as soon as it says it filled (it
// forks 4 s later); the child's is polled for up to 20 s of its 30.
import { spawn } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { native } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 300 };

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export default async function (t) {
  const n = native(t);
  const d = n.dir('exec-frees-copy');
  await n.cc(['-O1', '-static', '-o', `${d}/efc`, n.src('exec-frees-copy/efc.c')]);
  if (!existsSync(n.gexcPath)) t.skip(`${n.gexcPath} is not built`);
  const p = spawn(n.gexcPath, ['run', '--bind', `${d}=/t`, '--env', 'EFC=/t/efc', `${d}/efc`], { env: n.env, stdio: ['ignore', 'pipe', 'pipe'] });
  let out = '';
  p.on('error', (e) => { out += e.message; });
  p.stdout.on('data', (b) => { out += b; });
  p.stderr.on('data', (b) => { out += b; });
  const rss = () => {
    try {
      return Math.floor(Number(/^VmRSS:\s+(\d+)/m.exec(readFileSync(`/proc/${p.pid}/status`, 'utf8'))?.[1]) / 1024);
    } catch {
      return NaN;
    }
  };
  const until = async (line, ms) => {
    for (const end = Date.now() + ms; Date.now() < end; await sleep(20)) if (new RegExp(`^${line}`, 'm').test(out)) return true;
    return false;
  };
  try {
    if (!await until('filled', 240000)) return t.check('the parent filled its memory', false, out.trim().split('\n').slice(-5).join(' / '));
    const before = rss();
    if (!await until("exec'd", 60000)) return t.check('the child exec\'d', false, out.trim().split('\n').slice(-5).join(' / '));
    let after = rss();
    for (const end = Date.now() + 20000; after - before >= 256 && Date.now() < end; await sleep(100)) after = rss();
    t.check(`host RSS ${before} MiB with the parent alone, ${after} MiB with its exec'd child (within 256 MiB)`, after - before < 256, 'the 384 MiB copy kept');
  } finally {
    p.kill('SIGKILL');
  }
  n.clean();
}
