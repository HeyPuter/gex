// A script that prints its own checks, one line each (`ok ...` or
// `FAIL ...`), as a test: each line is a check, and so is its exit status
// (0). It runs in node from this checkout's root, with TMPDIR the test's work
// directory, GEX_ROOT this checkout and GEX_LOG off, plus `env`; `timeout`
// (ms) then SIGKILL, a failure.
//
//   await checks(t, join(FIXTURES, 'page/lazy-rest.mjs'));

import { spawn } from 'node:child_process';
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';

const ROOT = join(import.meta.dirname, '..', '..');
export const FIXTURES = join(import.meta.dirname, '..', 'fixtures');

export async function checks(t, file, { env = {}, timeout = 300000, args = [] } = {}) {
  const tmp = join(t.workDir, 'tmp');
  mkdirSync(tmp, { recursive: true });
  // Not this process's IPC channel to the runner (fork's NODE_CHANNEL_FD).
  const { NODE_CHANNEL_FD, NODE_CHANNEL_SERIALIZATION_MODE, ...base } = process.env;
  const r = await new Promise((resolve) => {
    const p = spawn(process.execPath, [file, ...args], { cwd: ROOT, env: { ...base, TMPDIR: tmp, GEX_ROOT: ROOT, GEX_LOG: 'off', ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    let err = '';
    p.stdout.on('data', (d) => { out += d; });
    p.stderr.on('data', (d) => { err = (err + d).slice(-4000); });
    const timer = setTimeout(() => p.kill('SIGKILL'), timeout);
    p.on('close', (code, signal) => { clearTimeout(timer); resolve({ code, signal, out, err }); });
  });
  let n = 0;
  for (const l of r.out.split('\n')) {
    const m = /^(ok|FAIL)\s+(.*)$/.exec(l);
    if (m) { t.check(m[2], m[1] === 'ok'); n++; }
  }
  const tail = (r.err || r.out).trim().split('\n').slice(-3).join(' / ');
  t.check(`${file.split('/').pop()} exits 0`, r.code === 0, r.signal ? `killed (${r.signal}${r.signal === 'SIGKILL' ? ', timed out' : ''})` : `exit ${r.code}: ${tail}`);
  if (!n) t.check('it printed its checks', false, tail);
}
