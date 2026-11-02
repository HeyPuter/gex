// An ejected Wine site (gex-eject: its project/build/web/nt.json and the
// files it names) run through the runtime API alone, as a page without the
// eject's own start() would: the bundle laid with Filesystem (its tar, its
// pieces, its lazy list and rest bundle), the windows handed to the page
// (HostWindows), Mesa when the build has GL, and the build's program
// spawned. Up when a window with the launch file's title shows. Then the
// root kept by IdbPersist across a reload.

import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';
import { HostWindows } from '../../web/runtime/src/windows/host.js';
import { MesaGl } from '../../web/runtime/src/gl.js';
import { IdbPersist } from '../../web/runtime/src/persist/idb.js';

// Alone: a Wine desktop's processes beside other tests' pages ran the
// renderer out of address space.
export const meta = { hosts: ['browser'], arches: ['x86'], images: ['wine-site'], timeout: 1800, alone: true,
  html: `<style>html, body { margin: 0; } #desk { position: relative; width: 1280px; height: 820px; overflow: hidden; }</style>
<div id="desk"></div>` };

const DB = 'rt-wine-site-test';
const KEPT = '/pfx/drive_c/users/kept-by-the-page.txt';

export default async function (t) {
  const { say, check, eq, timeout, until } = t;
  const base = new URL(t.work('wine-site/'), location.href);
  const at = (p) => new URL(String(p).replace(/^\/+/, ''), base).href;
  const nt = await (await fetch(at('project/build/web/nt.json'))).json();
  if (!['wine32', 'w3', 'wine64'].includes(nt.kind)) return t.skip(`a launch file of kind ${nt.kind}`);
  const params = { ...(nt.params ?? {}) };
  const pack = await fetch(at('pack.json')).then((r) => (r.ok ? r.json() : {}), () => ({}));
  const packed = (p) => pack.files?.[`/${String(p).replace(/^\/+/, '')}`] ?? null;
  // A packed file is served compressed (FILE.zst), as the site's page reads it.
  const { ZSTDDecoder } = await import(/* @vite-ignore */ at('vendor/zstddec-stream.mjs'));
  const zstd = new ZSTDDecoder();
  await zstd.init();
  const bytes = async (p) => {
    const e = packed(p);
    const r = await fetch(at(e ? `${p}.zst` : p));
    if (!r.ok) throw new Error(`${p}: ${r.status}`);
    const b = new Uint8Array(await r.arrayBuffer());
    return e ? zstd.decode(b, e.size) : b;
  };

  const phase = t.phase;
  if (phase === 1) await new Promise((r, j) => { const q = indexedDB.deleteDatabase(DB); q.onsuccess = r; q.onerror = j; });
  const sys = new System({
    baseUrl: t.baseUrl,
    // Not the build's units: they were compiled for the runtime the site
    // was built with, and this is the package under test.
    arch: 'x86',
    debug: t.debug(),
  });
  const fs = new Filesystem(sys);
  const tar = 'project/build/web/rootfs.tar';
  fs.addTar(at(packed(tar) ? `${tar}.zst` : tar));
  // The bundle's larger files, kept out of its tar (eject-site.mjs --pieces):
  // the tar holds each empty, its bytes are a piece's.
  if (params.pieces) {
    const list = await (await fetch(at(params.pieces))).json();
    const pieces = await Promise.all(list.pieces.map(([u]) => bytes(u)));
    const files = {};
    for (const [path, i, off, n] of list.files) files[path] = pieces[i].subarray(off, off + n);
    fs.addFiles(files);
    say(`${list.files.length} files from ${list.pieces.length} pieces`);
  }
  // The lazy files (with the rest bundle's, `rest.bin#OFFSET`).
  if (params.lazy) fs.addTar(null, { lazy: at(params.lazy) });
  const persist = new IdbPersist(fs, { db: DB, quiet: 300 });

  const [w, h] = typeof nt.size === 'string' && /^\d+x\d+$/.test(nt.size) ? nt.size.split('x').map(Number) : [1280, 800];
  const hw = new HostWindows(sys, { width: w, height: h, scale: 1 });
  const desk = document.getElementById('desk');
  const wins = [];
  hw.addEventListener('window', (e) => {
    const win = e.window;
    wins.push(win);
    win.canvas.style.position = 'absolute';
    desk.append(win.canvas);
    if (!win.placed) win.place(0, 0);
  });
  const features = nt.features_off ?? [];
  if (!features.includes('gl')) new MesaGl(sys, { d3d: !!nt.d3d });

  const t0 = performance.now();
  await timeout(sys.boot(), 300000, 'boot');
  say(`booted in ${(performance.now() - t0).toFixed(0)} ms`);

  if (phase === 2) {
    eq('a file the page wrote before the reload', await fs.readFile(KEPT, { encoding: 'utf-8' }).catch((e) => `(${e.message})`), 'kept\n');
    await persist.reset();
    await sys.shutdown();
    return;
  }

  const env = Object.fromEntries((nt.env ?? []).map((e) => [e.slice(0, e.indexOf('=')), e.slice(e.indexOf('=') + 1)]));
  Object.assign(env, hw.env);
  const p = sys.spawn([nt.exe, ...(nt.argv ?? [])], { env, cwd: nt.cwd ?? '/' });
  let log = '';
  const keep = () => new WritableStream({ write(b) { log = (log + new TextDecoder().decode(b)).slice(-100000); } });
  p.stdout.pipeTo(keep()).catch(() => {});
  p.stderr.pipeTo(keep()).catch(() => {});
  const title = nt.title ?? null;
  const shown = await t.time('window shown', () => until(() => wins.find((x) => x.kind === 'toplevel' && (!title || x.title === title)),
    `a window${title ? ` titled ${JSON.stringify(title)}` : ''}`, 900000)).then(() => true, () => false);
  if (!shown) {
    await t.saveFile('wine.log', log);
    await t.saveFile('console.log', sys.state.console);
  }
  check(`the program's window shows (${(performance.now() - t0).toFixed(0)} ms from boot)`, shown,
    `windows ${JSON.stringify(wins.map((x) => [x.kind, x.title]))}; log ${JSON.stringify(log.slice(-1500))}; console ${JSON.stringify(sys.state.console.slice(-800))}`);
  await new Promise((r) => setTimeout(r, 5000));
  await t.screenshot('site');

  // The root kept across a reload.
  await fs.writeFile(KEPT, 'kept\n');
  await persist.save();
  await sys.shutdown();
  return t.reload();
}
