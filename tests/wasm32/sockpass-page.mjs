// tests/wasm32/sockpass-page.sh's driver: /t/sockpass from the bundle at
// SPTAR in web/nt.html, its argv SPARGV (JSON), the page's query SPQ (the
// Wisp switch), once per MODES ("mp sp": sp is `&single=1`, the
// single-thread build). Prints each run's output; exits 1 if any run did not
// exit 0.
//   node tests/wasm32/sockpass-page.mjs
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
const ROOT = join(dirname(fileURLToPath(import.meta.url)), '../..');
const { openPage, sleep, stopAll } = await import(join(ROOT, 'tests/nt/browser-harness.mjs'));
const argv = encodeURIComponent(process.env.SPARGV || '[]');
let bad = 0;
for (const mode of (process.env.MODES || 'mp sp').split(' ')) {
  const q = `tar=${process.env.SPTAR}&exe=/t/sockpass&cwd=/t&argv=${argv}${process.env.SPQ ?? ''}${mode === 'sp' ? '&single=1' : ''}`;
  const { state, consoleLines } = await openPage(q);
  const end = Date.now() + Number(process.env.SPWAIT || 300000);
  let s;
  while (Date.now() < end) {
    s = await state();
    if (s?.phase === 'done' || s?.phase === 'failed') break;
    await sleep(200);
  }
  const ok = s?.phase === 'done' && s?.status === 0;
  console.log(`${ok ? 'ok  ' : 'FAIL'} sockpass page ${mode}: phase ${s?.phase} status ${s?.status} ${s?.error ?? ''}`);
  console.log((s?.out ?? '').trimEnd().replace(/^/gm, '  '));
  if (!ok) for (const l of consoleLines.filter((l) => /SCM_RIGHTS|panic|rror/.test(l))) console.log('  console: ' + l.slice(0, 400));
  if (!ok) bad++;
  stopAll();
  await sleep(2000);
}
process.exit(bad ? 1 : 0);
