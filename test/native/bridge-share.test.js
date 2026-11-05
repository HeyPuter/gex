// Bridged dispatch slots share entries in `blocks` (gex_emit::dense::
// bridge_words: one entry per 256 bridged slots, each slot an arm of it), and
// the bridge takes its target from the pc the caller dispatched on, never
// from the entry or arm (main.c). Two unit entries called alternately from
// one call site must each run their own code, and a unit's code unmapped
// (its pages retired, their slots cleared) must leave the other unit's
// bridged slots, which share its entry, working. On the page's shim (the SAB
// build), with GEX_LOG=bridges counting the entries written and the
// addresses entered; the host's gcc build gives the expected output. Needs
// gexcc (GEXCC) for the wasm32 leg.
import { join } from 'node:path';
import { native, exec, tail, ROOT } from '../lib/native.mjs';
import { gexcc } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 900 };

export default async function (t) {
  const n = native(t);
  const c = gexcc(t);
  const seed = n.seed();
  const d = n.dir('bridge-share');
  const h = n.dir('bridge-share-host');
  for (const [o, cc] of [[d, n.cc], [h, n.hostcc]]) {
    await cc(['-O2', '-fPIC', '-shared', '-DK=3', '-DC=1', '-Wl,-soname,liba.so', '-o', `${o}/liba.so`, n.src('bridge-share/lib.c')]);
    await cc(['-O2', '-fPIC', '-shared', '-DK=7', '-DC=-2', '-Wl,-soname,libb.so', '-o', `${o}/libb.so`, n.src('bridge-share/lib.c')]);
    await cc(['-O1', '-o', `${o}/main`, n.src('bridge-share/main.c'), `-L${o}`, '-lb', `-Wl,-rpath,${o}`, '-ldl']);
  }
  const want = (await exec([`${h}/main`, h])).stdout.replace(/\n+$/, '');
  const libs = { 'liba.so': `${d}/liba.so`, 'libb.so': `${d}/libb.so` };
  const cache = `${d}/cgen`;
  const run = () => n.g.run(seed, `${d}/main`, [d], {
    files: { ...n.ldso, [libs['liba.so']]: libs['liba.so'], [libs['libb.so']]: libs['libb.so'], [`${d}/main`]: `${d}/main` },
    flags: ['--cgen-prefer'], cwd: d, timeout: 300000,
    env: { GEX_CGEN_CACHE: cache, GEX_ELF_UNITS: 'liba.so,libb.so', GEX_SHARED: '1', GEX_LOG: 'bridges' },
  });
  // A first run finds where the loader maps the libraries; their units are built for those bases.
  await n.units(libs, cache, async () => (await run()).stderr);
  const r = await run();
  const placed = r.stderr.split('\n').filter((l) => /lib[ab]\.so at 0x[0-9a-f]*: wanting its unit/.test(l)).length;
  // The main thread's last line: entries written, addresses entered.
  const last = r.stderr.split('\n').filter((l) => /\[bridges\] pid 1 main thread/.test(l)).at(-1) ?? '';
  const entered = Number(/entered (\d+) addresses/.exec(last)?.[1] ?? 0);
  const written = /, (\d+) bridge entries/.exec(last)?.[1] ?? '?';
  t.eq('output as native', r.guest, want, r.signal === 'SIGKILL' ? 'hung' : r.stderr.split('\n').filter((l) => /trap|rror/.test(l)).slice(0, 5).join(' / '));
  t.check(`exits 0, units placed ${placed} times (2), ${entered} addresses entered (at least 6) through ${written} bridge entry writes`,
    r.code === 0 && placed === 2 && entered >= 6, `exit ${r.code}`);

  // A call-out watching one arm of a shared entry, then a publish naming that
  // entry (its sibling arms take the bridge): the watch must stay on, or the
  // guest returning there runs the bridge and the call-out never ends. Driven
  // from JS (Gex.testBridgeWatch), since an ELF unit's call-out returns
  // mid-function, never through a bridged arm.
  const w = await exec([process.execPath, '--input-type=module', '-e',
    `const { Gex } = await import(${JSON.stringify(join(ROOT, 'hosts/browser/js/gex.mjs'))}); for (const f of Gex.testBridgeWatch()) console.log(f);`],
  { env: { ...n.env, GEX_TEST_BRIDGE_WATCH: '1' } });
  t.check(`a call-out's watch outlives publishes into its entry`, w.code === 0 && !(w.stdout + w.stderr).trim(), tail(w.stdout + w.stderr));

  // Units off, a native wasm32 program: its module goes in through the
  // function table's log all the same. An exec from a second thread keeps
  // that thread's Worker, and its `blocks` already holds the entry the new
  // program's bridges take arms of, so dispatch reaches the bridge (and
  // `gex.lookup`) before any miss: lookup must catch the Worker up on the
  // log, or the slot resolves to `resolve` for ever (cgen.rs `behind`). The
  // hang was intermittent, so a few runs.
  const et = `${c.dir('bridge-share')}/execthreads`;
  await c.build(['-O2', '-o', et, n.src('bridge-share/execthreads.c')]);
  for (const i of [1, 2, 3]) {
    const e = await n.g.run(seed, et, [], { files: { '/t/execthreads': et }, flags: ['--cwd', '/t'], argv0: './execthreads', cwd: d, timeout: 120000, env: { GEX_SHARED: '1' } });
    t.check(`exec from a second thread with units off (run ${i})`, e.code === 0 && /exec from a second thread: 4 of 4/.test(e.stdout),
      `exit ${e.code}${e.signal === 'SIGKILL' ? ' (hung)' : ''}: ${tail(e.stdout)}`);
  }
  c.clean();
  n.clean();
}
