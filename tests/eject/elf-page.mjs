#!/usr/bin/env node
// A Linux program's ejected web build in headless Chromium: the project's
// own serve.mjs, its page (user/index.html, the example gexc eject writes)
// calling app.mjs's start() (docs/eject.md, "The web build").
//
//   node tests/eject/elf-page.mjs PROJECT --expect TEXT [--status N] [--port N] [--chrome PATH]
//
// Checks: the page loads once; the program's output (a <pre> start() puts in
// the container) has TEXT; with --status N, the page says the program
// exited N. Prints PASS or FAIL lines; exits 0 only when all pass.

import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { Cdp, browserWsUrl } from '../../web/test/cdp.mjs';

const av = process.argv.slice(2);
const opt = (n, d) => { const i = av.indexOf(n); return i >= 0 ? av[i + 1] : d; };
const project = resolve(av.find((a, i) => !a.startsWith('--') && !['--expect', '--status', '--port', '--chrome'].includes(av[i - 1])) ?? '');
const expect = opt('--expect', '');
const status = opt('--status', null);
const port = Number(opt('--port', '8751'));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`); if (!ok) failed++; return ok; };

const server = spawn(process.execPath, [join(project, 'serve.mjs'), String(port)], { stdio: ['ignore', 'pipe', 'inherit'] });
await new Promise((r) => server.stdout.once('data', r));
const profile = mkdtempSync(join(tmpdir(), 'gex-elf-leg-'));
const cport = port + 1;
const chrome = spawn(opt('--chrome', process.env.CHROME || '/usr/bin/chromium'), [
  '--headless', '--no-sandbox', '--no-first-run', '--no-default-browser-check', '--remote-allow-origins=*',
  `--remote-debugging-port=${cport}`, `--user-data-dir=${profile}`, 'about:blank',
], { detached: true, stdio: 'ignore' });
const done = () => {
  try { process.kill(-chrome.pid, 'SIGKILL'); } catch {}
  server.kill();
  rmSync(profile, { recursive: true, force: true });
  console.log(failed ? `FAIL ${failed} check(s)` : 'PASS all');
  process.exit(failed ? 1 : 0);
};
let ws = null;
for (let i = 0; i < 80 && !ws; i++) { ws = await browserWsUrl(cport); if (!ws) await sleep(250); }
if (!check(!!ws, 'Chromium started')) done();
const browser = await Cdp.connect(ws);
const { targetId } = await browser.send('Target.createTarget', { url: 'about:blank' });
const { sessionId: page } = await browser.send('Target.attachToTarget', { targetId, flatten: true });
await browser.send('Runtime.enable', {}, page);
await browser.send('Page.enable', {}, page);
const loads = [];
browser.on('Page.frameNavigated', (p) => { if (!p.frame.parentId) loads.push(new URL(p.frame.url).pathname); });
const ev = async (e) => (await browser.send('Runtime.evaluate', { expression: e, returnByValue: true }, page).catch(() => ({})))?.result?.value;
const nav = await browser.send('Page.navigate', { url: `http://127.0.0.1:${port}/` }, page).catch((e) => ({ errorText: String(e?.message ?? e) }));
if (nav?.errorText) check(false, `the page loads (${nav.errorText})`);
let text = '';
let shown = '';
for (let i = 0; i < 120; i++) {
  text = (await ev("[...document.querySelectorAll('pre')].filter((p) => p.id !== 'error').map((p) => p.textContent).join('')")) ?? '';
  shown = (await ev("document.getElementById('error')?.textContent ?? ''")) ?? '';
  if (text.includes(expect) && (status === null || shown)) break;
  await sleep(500);
}
if (!text.includes(expect)) console.log(`info: ${await ev('location.href')}: ${JSON.stringify(((await ev('document.documentElement.outerHTML')) ?? '').slice(0, 600))}`);
check(text.includes(expect), `the program's output has ${JSON.stringify(expect)} (${JSON.stringify(text.slice(0, 200))})`);
if (status !== null) check(shown.includes(`exited (${status})`), `the page says it exited ${status} (${JSON.stringify(shown)})`);
check(loads.length === 1 && loads[0] === '/', `one load of the project's page (${loads.join(' ')})`);
// start()'s progress: whole totals from the manifest, all of them read, and
// the phases in order.
{
  const p = JSON.parse((await ev('JSON.stringify(globalThis.__nt?.progress ?? null)')) ?? 'null');
  const d = p?.download;
  check(!!d && d.total > 0 && d.done === d.total && d.files > 0 && d.filesDone === d.files,
    `start()'s download is whole and all read (${d ? `${d.done} of ${d.total} bytes, ${d.filesDone} of ${d.files} files` : 'no progress'})`);
  const at = p?.at ?? {};
  check(at.boot !== undefined && at.ready !== undefined && at.download <= at.boot && at.boot <= at.ready,
    `its phases in order (${JSON.stringify(at)})`);
}
done();
