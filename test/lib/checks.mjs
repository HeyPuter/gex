// A script that prints its own checks, one line each (`ok ...` or
// `FAIL ...`), as a test: each line is a check, and so is its exit status
// (0); a `skip ...` line is a failed one (a leg not run). It runs in node
// from this checkout's root, with TMPDIR the test's work directory, GEX_ROOT
// this checkout and GEX_LOG off, plus `env`; `node` (node's own flags) and
// `args` go before and after the script. `timeout` (ms) then SIGKILL, a
// failure; `need`: files (from the checkout's root) whose absence is a skip;
// `summary`: a script that prints no check lines, only a summary last, which
// names its one check (its exit status).
//
//   await checks(t, join(FIXTURES, 'page/lazy-rest.mjs'));

import { spawn } from 'node:child_process';
import { existsSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';

const ROOT = join(import.meta.dirname, '..', '..');
export const FIXTURES = join(import.meta.dirname, '..', 'fixtures');

export async function checks(t, file, { env = {}, timeout = 300000, args = [], node = [], need = [], summary = false } = {}) {
  for (const f of need) if (!existsSync(join(ROOT, f))) t.skip(`no ${f} (make page)`);
  const tmp = join(t.workDir, 'tmp');
  mkdirSync(tmp, { recursive: true });
  // Not this process's IPC channel to the runner (fork's NODE_CHANNEL_FD).
  const { NODE_CHANNEL_FD, NODE_CHANNEL_SERIALIZATION_MODE, ...base } = process.env;
  const r = await new Promise((resolve) => {
    const p = spawn(process.execPath, [...node, file, ...args], { cwd: ROOT, env: { ...base, TMPDIR: tmp, GEX_ROOT: ROOT, GEX_LOG: 'off', ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    let err = '';
    p.stdout.on('data', (d) => { out += d; });
    p.stderr.on('data', (d) => { err = (err + d).slice(-4000); });
    const timer = setTimeout(() => p.kill('SIGKILL'), timeout);
    p.on('close', (code, signal) => { clearTimeout(timer); resolve({ code, signal, out, err }); });
  });
  if (summary) {
    const last = r.out.trim().split('\n').pop().trim();
    t.check(last || `${file.split('/').pop()} said nothing`, r.code === 0 && !!last,
      r.signal ? `killed (${r.signal})` : `exit ${r.code}: ${(r.err || r.out).trim().split('\n').slice(-3).join(' / ')}`);
    return;
  }
  let n = 0;
  for (const l of r.out.split('\n')) {
    const m = /^(ok|FAIL)\s+(.*)$/.exec(l);
    if (m) { t.check(m[2], m[1] === 'ok'); n++; }
    else if (/^skip\b/.test(l)) { t.check(l, false, 'a leg not run'); n++; }
  }
  const tail = (r.err || r.out).trim().split('\n').slice(-3).join(' / ');
  t.check(`${file.split('/').pop()} exits 0`, r.code === 0, r.signal ? `killed (${r.signal}${r.signal === 'SIGKILL' ? ', timed out' : ''})` : `exit ${r.code}: ${tail}`);
  if (!n) t.check('it printed its checks', false, tail);
}
