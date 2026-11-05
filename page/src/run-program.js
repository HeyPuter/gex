// `runProgram` (runtime.js) as a preset over the runtime API (page/runtime):
// a System with the kernel's own pid 1 and the program its first spawn (an
// orphan it leaves is reaped there, as under a real init), its files an
// untarred bundle laid into
// the root before boot, the windows class its `wl` asks for, MesaGl for
// `gl: 'compositor'`, the page's network, and the program's terminal or
// pipes bridged to `term`. What the API does not offer (an ejected build's
// units, fill and lazy cache, a closure, the exec switches) goes to the
// system's preset hooks (system.js `#preset`).

import { System } from '../runtime/src/system.js';
import { MemoryFilesystem, OPFSFilesystem } from '../runtime/src/fs.js';
import { filesystemValue } from './start-options.js';
import { CompositorWindows } from '../runtime/src/windows/compositor.js';
import { SingleCanvas } from '../runtime/src/windows/single.js';
import { MesaGl } from '../runtime/src/gl.js';
import { Gamepads, WineControllers } from '../runtime/src/input.js';
import { INTERNAL } from '../runtime/src/internal.js';
import { guestArch, audioFrom } from './runtime.js';
import { pageParams } from './page-env.js';

/** The environment the program gets when the caller gives none: Debian's for
 *  root, with sbin (dpkg looks there for ldconfig). */
const DEFAULT_ENV = ['PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin', 'HOME=/root'];

const defined = (o) => Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined && v !== null));

/**
 * Options, as runProgram has always taken them:
 *   elf, args           the program's bytes and its argv (args[0] its path)
 *   files, dirs, modes, hard, symlinks, lazy, mtimes, xattrs
 *                       the bundle (tar.js `untar`'s parts, lazy files
 *                       `[path, size, url, head]`)
 *   stream              the bundle as it is unpacked instead (app.js
 *                       `streamBundle`): { parts, has(path), index }, laid at
 *                       boot part by part over the above; `elf` is a file of
 *                       it. With an index (a promise of its bytes or null),
 *                       the program starts once that is laid
 *   env, cwd, uid       the program's; env empty is Debian's for root
 *   filesystem          required: 'memory' or start-options.js
 *                       `filesystemValue`'s { kind, ... }: the runtime's
 *                       MemoryFilesystem or OPFSFilesystem
 *   tty                 a terminal (default) or pipes
 *   wl, wlScale         a canvas (the scene compositor) or an element (the
 *                       windows laid out in it, bare-desktop.js)
 *   onDesktop(d)        the element's desktop (dom-compositor.js), when made
 *   hostX               'rootful': the X server one fullscreen screen
 *   gl                  'compositor': the guest's GL on the windows' Mesa
 *   glLazy, glSource, glBackend, d3d    MesaGl's start, files, driver, D3D
 *   network, onNetEvent the guest's sockets (page/src/net.js `Sockets`) and their events
 *   playAudio           /dev/dsp to the speakers (default true)
 *   onFilesLoaded()     the bundle is in the kernel: its bytes can go
 *   onSystem(s)         the running system's parts: { system, host,
 *                       kernel, compositor, wlChannel, startGl }
 *   closure, closureBuildNumbered, fill, lazyCache, restBundle, identities,
 *   stateful, providerSources, preModules, lateUnits, heartbeat,
 *   recompileObjects, link   the processes' host's (runtime.js
 *                       `makeProcessHost`)
 * Resolves `{ status, host, system }` when the program ends, and the system
 * with it. The program is the system's first spawn, a child of the kernel's
 * pid 1.
 */
export async function runPreset(term, {
  elf, args, files = {}, dirs = [], modes = {}, hard = {}, symlinks = {}, lazy = [], mtimes, xattrs, stream = null,
  env = [], cwd = '/', uid, tty = true, filesystem,
  wl = null, wlScale = 1, onDesktop = null, hostX = false,
  gl = null, glLazy = false, glSource = null, glBackend, d3d = false,
  network = null, onNetEvent = null, playAudio = true, onFilesLoaded = null, onSystem = null,
  closure = null, closureBuildNumbered, fill, lazyCache, restBundle, identities, stateful, providerSources,
  preModules, lateUnits, heartbeat, recompileObjects, link,
  // Taken and not needed: the System has its own page-fs channel, and the
  // program's Worker no longer holds the bundle.
  pageFs: _pageFs, transfer: _transfer, wlInput: _wlInput,
  ...rest
}) {
  const unknown = Object.keys(rest);
  if (unknown.length) throw new TypeError(`runProgram: no option ${unknown.join(', ')}`);
  if (!Array.isArray(args) || !args.length) throw new TypeError('runProgram: args, the program\'s argv');
  if (gl && gl !== 'compositor') throw new TypeError('runProgram: gl is \'compositor\' (the windows\' Mesa) or nothing');
  const exe = args[0];
  // The bundle names the program itself, or a link on its way (/bin/bash
  // where /bin -> usr/bin): `elf` is then not laid in, or it would make a
  // directory where the link goes.
  const viaLink = (p) => {
    let at = '';
    for (const part of p.split('/').filter(Boolean)) {
      at += `/${part}`;
      if (symlinks[at] !== undefined) return true;
    }
    return false;
  };
  const named = !!stream || files[exe] !== undefined || hard[exe] !== undefined || viaLink(exe);
  if (!elf && !named) throw new Error(`runProgram: no ${exe} in the files`);

  const q = pageParams();
  const sys = new System({
    baseUrl: globalThis.GEX_ASSET_BASE ?? (globalThis.location ? new URL('/', globalThis.location.href).href : undefined),
    // `?xcache=1`: the translated code a load saved, restored before the
    // program runs.
    xcache: !!q.get('xcache') && q.get('xcache') !== '0',
    // The architecture the page chose: the runtime loads while the files
    // are laid out.
    arch: guestArch(),
    // The page's own switches, as its URL or the library set them
    // (page-env.js).
    debug: Object.fromEntries(q),
  });
  const i = sys[INTERNAL];
  // The runtime's routine lines stay as the page had them.
  i.preset.loud = true;
  Object.assign(i.preset.host, defined({
    closure, closureBuildNumbered, fill, lazyCache, restBundle, identities, stateful, providerSources,
    preModules, lateUnits, heartbeat, recompileObjects, link, uid,
  }));
  // A closure's layout (`gexc build --closure`) for each program: its
  // libraries' code expects them where the build placed them.
  Object.assign(i.preset.exec, defined({ recompileObjects, heartbeat, link, pins: closure?.pins }));
  if (glBackend) i.preset.glBackend = glBackend;
  if (playAudio) i.preset.onMessage.push((m) => { if (m.kind === 'audio') audioFrom(m); });
  if (onNetEvent) i.preset.onMessage.push((m) => { if (m.kind === 'net') onNetEvent(m.event); });

  const where = filesystemValue(filesystem);
  if (!where) throw new TypeError(`runProgram: choose where the files are kept: filesystem is 'memory', { kind: 'memory', max } or { kind: 'opfs', name, cache }`);
  const fs = where.kind === 'opfs' ? new OPFSFilesystem(sys, where) : new MemoryFilesystem(sys, where);
  const image = { ...files };
  const imageModes = { ...modes };
  if (!named) {
    image[exe] = elf;
    imageModes[exe] ??= 0o755;
  }
  fs[INTERNAL].addImage({ files: image, modes: imageModes, dirs, lazy, hard, symlinks, mtimes, xattrs, ...(stream ? { stream: stream.parts, streamIndex: stream.index ?? null } : {}) });

  let windows = null;
  if (wl && typeof wl.getContext === 'function') {
    windows = new CompositorWindows(sys, { canvas: wl, scale: wlScale });
  } else if (wl) {
    windows = new SingleCanvas(sys, { container: wl });
    windows[INTERNAL].x.onDesktop = (d, bare) => {
      // Tests and recorders read the desktop here (describe, readout).
      globalThis.__gexCompositor = bare;
      onDesktop?.(d);
    };
  }
  if (windows) windows[INTERNAL].x.rootful = hostX === 'rootful';
  const mesa = gl && windows ? new MesaGl(sys, { eager: !glLazy, d3d, source: glSource }) : null;
  if (network) i.sockets(network);

  // (the files are gone from `files` after the boot)
  const sdl = `${exe.replace(/\/[^/]*$/, '')}/libSDL2-2.0.so.0`;
  const inBundle = (p) => files[p] !== undefined || !!stream?.has(p) || lazy.some(([path]) => path === p);
  await sys.boot();
  // (after the boot: a streamed bundle's files are all in by then)
  const winePads = /\/wine$/.test(exe) && inBundle(sdl);
  // The kernel has the files: nothing here views the bundle's bytes now.
  for (const k of Object.keys(image)) delete image[k];
  onFilesLoaded?.();
  const s = i.sys();
  onSystem?.({ system: sys, host: s.host, kernel: s.kernel, compositor: s.compositor, wlChannel: s.wlChannel, startGl: mesa ? () => mesa.start() : null });

  const pty = tty ? { cols: term.cols ?? 80, rows: term.rows ?? 24 } : undefined;
  const p = sys.spawn(args, defined({ env: env.length ? env : DEFAULT_ENV, cwd, uid, gid: uid, pty }));
  // A Wine with game controllers (SDL beside its loader, wine64-dist.sh):
  // the page's controllers, and Wine's bus for them started once there is one.
  if (winePads) {
    const gamepads = new Gamepads(sys);
    const wine = new WineControllers(sys, gamepads, { wine: exe, env: env.length ? env : DEFAULT_ENV });
    wine.addEventListener('start', (e) => { if (e.code) i.say(`gex: winebus did not start (sc: ${e.code})\n`); });
  }
  const disposers = [];
  if (p.pty) {
    p.pty.addEventListener('data', (e) => term.write(e.data));
    disposers.push(term.onData((d) => p.pty.write(d)));
    disposers.push(term.onResize?.(({ rows, cols }) => p.pty.resize(cols, rows)));
  } else {
    const pump = async (stream) => {
      for (const r = stream.getReader(); ;) {
        const { value, done } = await r.read();
        if (done) return;
        term.write(value);
      }
    };
    pump(p.stdout).catch(() => {});
    pump(p.stderr).catch(() => {});
    const w = p.stdin.getWriter();
    const enc = new TextEncoder();
    disposers.push(term.onData((d) => { w.write(typeof d === 'string' ? enc.encode(d) : d).catch(() => {}); }));
  }
  term.focus?.();
  try {
    const { code } = await p.exited;
    // The run is the program's: the system stops with it, once what it
    // wrote last has come (unless a program it left holds its output open:
    // a second at most).
    await Promise.race([p[INTERNAL].drained(), new Promise((r) => setTimeout(r, 1000))]);
    return { status: code, host: s.host, system: sys };
  } finally {
    for (const d of disposers) d?.dispose?.();
    await sys.shutdown();
  }
}
