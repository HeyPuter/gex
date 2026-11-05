// What a node-host test needs to run a Windows program on 64-bit Wine,
// natively under gexc and on the page's shim under node
// (hosts/browser/js/run-node.mjs). Two installs, each a skip when not set:
// - wow64(t): a 32-bit program as a WoW64 process, on a WINE64_WOW64=1
//   install (GEX_TEST_WOW64_DIST, wine/dist.sh's WINE64_DIST) and its
//   prefix (GEX_TEST_WOW64_PREFIX);
// - wine64(t): a 64-bit program, on the default install
//   (GEX_TEST_WINE64_DIST) and its prefix (GEX_TEST_WINE64_PREFIX).
//
//   const w = wow64(t);
//   const exe = await w.mingw('rewind');              // fixtures/wow64/rewind.c
//   const n = await w.native(['C:\\rewind.exe'], { files: { 'rewind.exe': exe } });
//   const p = await w.page(['/t/rewind.exe'], { files: { '/t/rewind.exe': exe } });
//   t.check('native', /^rewind ok/m.test(n.out), w.detail(n));
//
// Neither the install nor the prefix is written. The run's copy of them,
// made once per work directory: the install hard-linked (a WoW64 dist with
// GEX_TEST_WOW64_MONO, Wine Mono's unpacked tree, as share/wine/mono), and
// the prefix with services.exe waiting 60 s for a service, not Wine's 10 (a
// translated 64-bit service takes 7-10 s on a busy host); a WoW64 prefix's
// system32 and syswow64 builtins also Wine placeholders, so the loader takes
// each builtin from the install. Each leg copies that prefix again for
// itself.

import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  appendFileSync, closeSync, copyFileSync, existsSync, mkdirSync, openSync, readdirSync, readFileSync,
  renameSync, rmSync, statSync, writeFileSync,
} from 'node:fs';
import { basename, join } from 'node:path';
import { guest, ROOT } from './guest.mjs';
import { Skip } from './t.js';

export { ROOT };
export const FIXTURES = join(import.meta.dirname, '..', 'fixtures', 'wow64');
/** The loader, in the install. */
export const LOADER = 'lib/wine/x86_64-unix/wine';
// A run's lines saying why a process died: the runtime's, and Wine's fatal ones.
const DEATH = /^\[gex\] pid \d+ killed by|^\[gex\] pid \d+: a vfork child|gex: (process|thread) \S+ failed|panicked at|RuntimeError|RangeError/;
const WINE_FATAL = /^wine: |wineserver: |:err:(process|module|virtual|server|sync):/;
/** Wine's environment for a WoW64 process (without WINEGEX_WOW64 the server takes no i386 process). */
export const WINE_ENV = { WINEGEX_WOW64: '1', WINEDEBUG: 'err+all,fixme-all' };
/** Wine's environment for a 64-bit process. */
export const WINE64_ENV = { WINEDEBUG: 'err+all,fixme-all' };

let runs = 0;

/** `argv` with stdout and stderr in one file, in the order written (`2>&1`),
 *  and `input` (if given) on stdin: `{ code, out, timedOut }`, `code` as a
 *  shell gives it. `errFile`: stderr in a file of its own instead, `out`
 *  stdout alone and `err` stderr. */
/** `argv` with its output to `file` (stderr too, or to `errFile`): resolves
 *  `{ code, out, err, timedOut }`. `until`: a RegExp; once the output
 *  matches it the run is ended (SIGTERM), and `until` in the result is the
 *  ms it took (null when it never matched). */
export function run(argv, { cwd = ROOT, env = process.env, timeout = 300000, file, errFile, input, until } = {}) {
  return new Promise((resolve) => {
    const fd = openSync(file, 'w');
    const efd = errFile ? openSync(errFile, 'w') : fd;
    const p = spawn(argv[0], argv.slice(1), { cwd, env, stdio: [input === undefined ? 'ignore' : 'pipe', fd, efd] });
    closeSync(fd);
    if (errFile) closeSync(efd);
    if (input !== undefined) { p.stdin.on('error', () => {}); p.stdin.end(input); }
    let timedOut = false, matched = null;
    const t0 = performance.now();
    const timer = setTimeout(() => { timedOut = true; p.kill('SIGKILL'); }, timeout);
    const watch = until && setInterval(() => {
      const text = readFileSync(file, 'utf8') + (errFile ? readFileSync(errFile, 'utf8') : '');
      if (matched === null && until.test(text)) { matched = Math.round(performance.now() - t0); p.kill('SIGTERM'); }
    }, 1000);
    p.on('error', (e) => { clearTimeout(timer); if (watch) clearInterval(watch); resolve({ code: 127, out: String(e.message), timedOut }); });
    p.on('close', (code, signal) => {
      clearTimeout(timer);
      if (watch) clearInterval(watch);
      const sig = { SIGABRT: 6, SIGKILL: 9, SIGSEGV: 11, SIGTERM: 15 }[signal];
      resolve({ code: code ?? 128 + (sig ?? 0), out: readFileSync(file, 'utf8'), ...(errFile && { err: readFileSync(errFile, 'utf8') }), timedOut, ...(until && { until: matched }) });
    });
  });
}

// --- Wine placeholder DLLs (setupapi fakedll.c's layout) ---

const SIG = Buffer.from('Wine placeholder DLL\0', 'latin1');
const EXT = ['.dll', '.exe', '.drv', '.sys', '.ocx', '.cpl', '.acm', '.ax', '.ds', '.tlb'];

/** The subsystem, resource RVA and resource bytes of the PE at `path`, or null. */
function builtinOf(path) {
  if (!existsSync(path) || !statSync(path).isFile()) return null;
  const b = readFileSync(path);
  if (b.length < 64 || b.toString('latin1', 0, 2) !== 'MZ') return null;
  const o = b.readUInt32LE(0x3c);
  if (o + 24 > b.length || b.toString('latin1', o, o + 4) !== 'PE\0\0') return null;
  const nsect = b.readUInt16LE(o + 6);
  const optSize = b.readUInt16LE(o + 20);
  const opt = o + 24;
  const pe32p = b.readUInt16LE(opt) === 0x20b;
  const sub = b.readUInt16LE(opt + 68);
  const d = opt + (pe32p ? 112 : 96) + 2 * 8;
  const rva = b.readUInt32LE(d);
  const size = b.readUInt32LE(d + 4);
  if (!rva || !size) return { sub, rva: 0, rsrc: Buffer.alloc(0) };
  for (let i = 0; i < nsect; i++) {
    const s = opt + optSize + 40 * i;
    const vsize = b.readUInt32LE(s + 8);
    const va = b.readUInt32LE(s + 12);
    const rawsize = b.readUInt32LE(s + 16);
    const rawptr = b.readUInt32LE(s + 20);
    if (va <= rva && rva < va + Math.max(vsize, rawsize)) {
      // the whole section from the directory on: its entries name RVAs inside it
      const end = va + Math.max(vsize, rawsize);
      const data = b.subarray(rawptr + (rva - va), rawptr + Math.min(rawsize, end - va));
      const out = Buffer.alloc(end - rva);
      data.copy(out);
      return { sub, rva, rsrc: out };
    }
  }
  return { sub, rva: 0, rsrc: Buffer.alloc(0) };
}

/** A placeholder for an exe or DLL, with `info`'s subsystem and resources (GUI and none without). */
function placeholder(isExe, wide, info) {
  const { sub, rva, rsrc } = info ?? { sub: 2, rva: 0, rsrc: Buffer.alloc(0) };
  const lfanew = (64 + SIG.length + 15) & ~15;
  const optSize = wide ? 240 : 224;
  const nsect = rsrc.length ? 1 : 0;
  const hdr = lfanew + 4 + 20 + optSize + 40 * nsect;
  const mem = (hdr + 4095) & ~4095;
  const fil = (hdr + 511) & ~511;
  const raw = (rsrc.length + 511) & ~511;
  const image = rsrc.length ? Math.max(mem, (rva + rsrc.length + 4095) & ~4095) : mem;
  const buf = Buffer.alloc(fil + (rsrc.length ? raw : 0));
  // DOS header
  buf.write('MZ', 0, 'latin1');
  [64, 1, 0, lfanew / 16, 0, 0xffff, 0, 0xb8, 0, 0, 0, lfanew, 0].forEach((v, i) => buf.writeUInt16LE(v, 2 + 2 * i));
  buf.writeUInt32LE(lfanew, 60);
  SIG.copy(buf, 64);
  // file header
  let p = lfanew;
  buf.write('PE\0\0', p, 'latin1');
  let chars = isExe ? 0 : 0x2000;
  if (!wide) chars |= 0x0100; // 32BIT_MACHINE
  buf.writeUInt16LE(wide ? 0x8664 : 0x14c, p + 4);
  buf.writeUInt16LE(nsect, p + 6);
  buf.writeUInt16LE(optSize, p + 20);
  buf.writeUInt16LE(chars | 0x0002, p + 22);
  // optional header
  p += 24;
  const o = p;
  const u16 = (v) => { buf.writeUInt16LE(v, p); p += 2; };
  const u32 = (v) => { buf.writeUInt32LE(v, p); p += 4; };
  const u64 = (v) => { buf.writeBigUInt64LE(BigInt(v), p); p += 8; };
  u16(wide ? 0x20b : 0x10b); buf[p++] = 1; buf[p++] = 0;
  u32(0); u32(0); u32(0); u32(mem); u32(mem);
  if (wide) u64(0x10000000); else { u32(0); u32(0x10000000); }
  u32(4096); u32(512);
  [1, 0, 1, 0, 4, 0].forEach(u16);
  u32(0); u32(image); u32(fil); u32(0); u16(sub); u16(0);
  if (wide) [0, 0, 0, 0].forEach(u64); else [0, 0, 0, 0].forEach(u32);
  u32(0); u32(16);
  if (rsrc.length) { buf.writeUInt32LE(rva, p + 2 * 8); buf.writeUInt32LE(rsrc.length, p + 2 * 8 + 4); }
  p = o + optSize;
  if (rsrc.length) {
    buf.write('.rsrc', p, 'latin1');
    buf.writeUInt32LE(rsrc.length, p + 8);
    buf.writeUInt32LE(rva, p + 12);
    buf.writeUInt32LE(raw, p + 16);
    buf.writeUInt32LE(fil, p + 20);
    buf.writeUInt32LE(0x40000040, p + 36);
    rsrc.copy(buf, fil);
  }
  return buf;
}

/** Every PE file in `dir` (not recursive) a placeholder for `machine`
 *  ('amd64' or 'i386'), taking its builtin's (in `builtins`) subsystem and
 *  resources; wow64*.dll and ntdll.dll kept. Returns how many. */
export function placeholders(dir, machine, builtins) {
  let n = 0;
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    const low = name.toLowerCase();
    if (!EXT.some((e) => low.endsWith(e)) || !statSync(path).isFile()) continue;
    if (low.startsWith('wow64') || low === 'ntdll.dll') continue;
    const info = builtinOf(join(builtins, low)) ?? builtinOf(join(builtins, name));
    rmSync(path);
    writeFileSync(path, placeholder(low.endsWith('.exe'), machine === 'amd64', info));
    n++;
  }
  return n;
}

// --- the run's install and prefix ---

/** `src` copied to `to`, hard links where it can (`cp -rLl`). */
function linkCopy(src, to) {
  try {
    execFileSync('cp', ['-rLl', src, to], { stdio: 'ignore' });
  } catch {
    rmSync(to, { recursive: true, force: true });
    execFileSync('cp', ['-rL', src, to], { stdio: 'ignore' });
  }
}

/** A throw when `prefix` is not one wine/dist.sh made for `dist`: a dist
 *  with winegstreamer (its media, -wgf) has a prefix with it installed and
 *  registered (wine/dist.sh's pfs64 for that dist), one without has none.
 *  The pairing is dist.sh's (WINE64_DIST and WINE64_PREFIX from one run). */
function paired(dist, prefix) {
  const media = existsSync(join(dist, 'lib/wine/x86_64-windows/winegstreamer.dll'));
  const has = existsSync(join(prefix, 'drive_c/windows/system32/winegstreamer.dll'));
  if (media !== has)
    throw new Error(`${prefix} is not ${dist}'s prefix: the install ${media ? 'has' : 'has no'} winegstreamer, the prefix ${has ? 'has' : 'has none'} (take WINE64_DIST and WINE64_PREFIX from one wine/dist.sh run)`);
}

function prepare(t, dist, prefix, mono, kind, native = null) {
  paired(dist, prefix);
  const key = createHash('sha256').update([dist, statSync(dist).mtimeMs, prefix, statSync(prefix).mtimeMs, mono ?? '', kind, native ?? ''].join('\n')).digest('hex').slice(0, 16);
  const top = join(t.workDir, kind, key);
  if (existsSync(join(top, '.done'))) return top;
  const tmp = `${top}.tmp-${process.pid}`;
  rmSync(tmp, { recursive: true, force: true });
  mkdirSync(tmp, { recursive: true });
  linkCopy(dist, join(tmp, 'dist'));
  if (mono) {
    const m = join(tmp, 'dist/share/wine/mono');
    mkdirSync(join(tmp, 'dist/share/wine'), { recursive: true });
    rmSync(m, { recursive: true, force: true });
    linkCopy(mono, m);
  }
  // `native`: a native PE core's files (wine/dist.sh's WINE64_NATIVE) in
  // place of the install's own, or beside them
  if (native) {
    const pe = join(tmp, 'dist/lib/wine/x86_64-windows');
    for (const f of readdirSync(native)) {
      rmSync(join(pe, f), { force: true });
      copyFileSync(join(native, f), join(pe, f));
    }
  }
  execFileSync('cp', ['-a', prefix, join(tmp, 'pfx')]);
  if (kind === 'wow64') {
    const win = join(tmp, 'pfx/drive_c/windows');
    placeholders(join(win, 'system32'), 'amd64', join(dist, 'lib/wine/x86_64-windows'));
    placeholders(join(win, 'syswow64'), 'i386', join(dist, 'lib/wine/i386-windows'));
  }
  appendFileSync(join(tmp, 'pfx/system.reg'), '\n[System\\\\ControlSet001\\\\Control] 1791289000\n"ServicesPipeTimeout"="60000"\n');
  writeFileSync(join(tmp, '.done'), '');
  try {
    renameSync(tmp, top);
  } catch {
    // another test made it first
    rmSync(tmp, { recursive: true, force: true });
  }
  return top;
}

export function wow64(t, { mono = false } = {}) {
  const env = process.env;
  const distSrc = env.GEX_TEST_WOW64_DIST || t.skip('GEX_TEST_WOW64_DIST: a WINE64_WOW64=1 install (wine/dist.sh)');
  const pfxSrc = env.GEX_TEST_WOW64_PREFIX || t.skip('GEX_TEST_WOW64_PREFIX: its WoW64 prefix');
  if (!existsSync(join(distSrc, 'lib/wine/i386-windows'))) t.skip(`${distSrc} has no lib/wine/i386-windows (WINE64_WOW64=1)`);
  // wine/dist.sh's WINE64_PREFIX, the session prefix: the bare wineboot
  // prefix beside it has no display driver, and no window opens
  if (!existsSync(join(pfxSrc, 'drive_c/windows/syswow64/winewayland.drv')))
    throw new Error(`${pfxSrc} has no syswow64/winewayland.drv: not wine/dist.sh's WINE64_PREFIX (pfs64-*), the session prefix`);
  const monoSrc = mono ? (env.GEX_TEST_WOW64_MONO || t.skip('GEX_TEST_WOW64_MONO: Wine Mono, both halves, unpacked')) : null;
  return session(t, { kind: 'wow64', distSrc, pfxSrc, monoSrc, wineEnv: WINE_ENV, mount: '/wine32', mingwTriple: 'i686' });
}

/** 64-bit Wine for a 64-bit program; `door` its WINEGEX_DOOR (the LP64
 *  door call, as the page has it; 0: none; null: not set). `native`: the install with the
 *  native PE core GEX_TEST_WINE64_NATIVE in place of its own (a skip
 *  without it). `mem64`: the memory64 flavour's install and prefix,
 *  GEX_TEST_WINE64_M64_DIST and _PREFIX. */
export function wine64(t, { door = 1, native = false, mem64 = false } = {}) {
  const env = process.env;
  const [D, P] = mem64 ? ['GEX_TEST_WINE64_M64_DIST', 'GEX_TEST_WINE64_M64_PREFIX'] : ['GEX_TEST_WINE64_DIST', 'GEX_TEST_WINE64_PREFIX'];
  const distSrc = env[D] || t.skip(`${D}: a 64-bit Wine install (wine/dist.sh${mem64 ? ', WINE64_MEM64=1' : ''})`);
  const pfxSrc = env[P] || t.skip(`${P}: its prefix (wine/dist.sh's WINE64_PREFIX)`);
  const nativeSrc = native ? (env.GEX_TEST_WINE64_NATIVE || t.skip('GEX_TEST_WINE64_NATIVE: a native PE core (wine/dist.sh\'s WINE64_NATIVE)')) : null;
  return session(t, { kind: mem64 ? 'wine64-m64' : native ? 'wine64-pe' : 'wine64', distSrc, pfxSrc, nativeSrc, wineEnv: { ...WINE64_ENV, ...(door !== null && { WINEGEX_DOOR: String(door) }) }, mount: '/wine64', mingwTriple: 'x86_64' });
}

function session(t, { kind, distSrc, pfxSrc, monoSrc = null, nativeSrc = null, wineEnv: WENV, mount, mingwTriple }) {
  const g = guest(t);
  const env = process.env;
  const gexc = env.GEXC_X86 || env.GEXC || join(ROOT, 'target/x86/release/gexc');
  const top = prepare(t, distSrc, pfxSrc, monoSrc, kind, nativeSrc);
  const dist = join(top, 'dist');
  const prefix = join(top, 'pfx');
  const work = join(t.workDir, `${kind}-runs`, `${process.pid}-${++runs}`);
  rmSync(work, { recursive: true, force: true });
  mkdirSync(work, { recursive: true });
  const { NODE_CHANNEL_FD, NODE_CHANNEL_SERIALIZATION_MODE, ...base } = env;
  // A failed check keeps this test's runs (`clean` leaves them).
  let failed = false;
  const check = t.check.bind(t);
  t.check = (what, cond, ...more) => { if (!cond) failed = true; return check(what, cond, ...more); };

  /** A copy of the run's prefix (or `from`), `name`, with `files` (`{ name
   *  in C:\: host path }`) in drive_c and `userReg` appended to user.reg. */
  const pfx = (name, files = {}, { userReg = '', from = prefix } = {}) => {
    const p = join(work, name);
    rmSync(p, { recursive: true, force: true });
    execFileSync('cp', ['-a', from, p]);
    for (const [to, f] of Object.entries(files)) copyFileSync(f, join(p, 'drive_c', to));
    if (userReg) appendFileSync(join(p, 'user.reg'), userReg);
    return p;
  };
  let loaderText, progsFailed;
  const w = {
    dist,
    prefix,
    work,
    gexc,
    loader: join(dist, LOADER),
    /** fixtures/wow64/NAME.c (`src`: another source) built with MinGW gcc
     *  for the session's arch (-O1, `flags`, then `libs`; `x64`:
     *  x86_64-w64-mingw32-gcc in any session). */
    async mingw(name, { flags = [], libs = [], out = `${name}.exe`, x64 = false, src = join(FIXTURES, `${name}.c`) } = {}) {
      const exe = join(work, out);
      mkdirSync(join(exe, '..'), { recursive: true });
      const r = await run([g.tool(`${x64 ? 'x86_64' : mingwTriple}-w64-mingw32-gcc`), '-O1', ...flags, '-o', exe, src, ...libs], { file: `${exe}.log` });
      if (r.code !== 0) throw new Error(`mingw ${name}: ${r.out.trim().split('\n').slice(-5).join(' / ')}`);
      return exe;
    },
    pfx,
    /** A host tool's path (a skip without it). */
    tool: g.tool,
    /** Whether the i386 side is native wasm32 (i386-windows/winepe.wasm), as
     *  32-bit Wine's is, not translated. */
    i386Native: existsSync(join(dist, 'lib/wine/i386-windows/winepe.wasm')),
    /** gexc itself (`args`, `env` on top of the host's) logged to `log`; a
     *  throw with its tail when it fails. */
    async gexcRun(args, { env: more = {}, log = 'gexc', timeout = 1800000 } = {}) {
      if (!existsSync(gexc)) t.skip(`${gexc} is not built`);
      const r = await run([gexc, ...args], { env: { ...base, ...more }, timeout, file: join(work, `${log}.log`) });
      if (r.code !== 0) throw new Error(`gexc ${args.slice(0, 2).join(' ')}: ${r.out.trim().split('\n').slice(-3).join(' / ')}`);
      return r.out;
    },
    /** The flavours of unit the processes of `loader` look up, natively and
     *  on the page (gexc cgen-flavour, the runtime's own answer), each the
     *  cgen-image switches that build it (`[]` the plain one). */
    async flavours(loader = w.loader) {
      const n = (await w.gexcRun(['cgen-flavour', loader], { log: 'flavour' })).trim();
      const p = (await w.gexcRun(['cgen-flavour', '--page', loader], { log: 'flavour-page' })).trim();
      return [...new Set([n, p])].map((f) => f.split(/\s+/).filter(Boolean));
    },
    /** The provider modules into `cache`: the plain ones and each flavour's
     *  (a provider's header folds what its flavour folds). */
    async providers(cache) {
      for (const k of [[], ...(await w.flavours()).filter((f) => f.length)])
        await w.gexcRun(['cgen-image', '--providers', ...k], { env: { GEX_CGEN_CACHE: cache }, log: 'providers' });
      return cache;
    },
    /** `image`'s unit at `base` into `cache`, in each flavour and each of
     *  `modes` (env for gexc: by default the precompiled unit, then the
     *  guarded one); `more` cgen-image's own switches. */
    async unit(image, cache, { base = '0x400000', modes = [{ GEX_CGEN_PRECOMPILE: '1' }, { GEX_CGEN_GUARD: '1' }], more = [], loader } = {}) {
      for (const k of await w.flavours(loader))
        for (const m of modes)
          await w.gexcRun(['cgen-image', image, '--pe-base', base, ...k, ...more, '--jobs', '4'], { env: { GEX_CGEN_CACHE: cache, ...m }, log: 'unit' });
      return cache;
    },
    /**
     * Wine on gexc with `args` (after the loader): in a copy of the prefix
     * (`prefix`: another to copy) with `files` in C:\ and `userReg` added
     * to its user.reg (`setup`: called with its path, then), from its
     * drive_c (or `cwd`). `wineEnv` on top of
     * the session's Wine environment (each a --env), `flags` gexc run's
     * own (`wayland`: a compositor of its own, no output; `{ size, out }`:
     * its output's size, and its frames as PPMs at `out`-NNN.ppm), `env` gexc's, `loader` another
     * loader, `split` stdout and stderr apart (run's `errFile`). A skip
     * when gexc is not built. Resolves run's result plus `pfx`, the prefix
     * it used.
     */
    async native(args, { files = {}, wineEnv = {}, flags = [], env: more = {}, prefix: from = prefix, userReg, setup, wayland = false, loader = w.loader, cwd, timeout = 300000, name = 'native', split = false, until } = {}) {
      if (!existsSync(gexc)) t.skip(`${gexc} is not built`);
      const p = pfx(`${name}-pfx`, files, { userReg, from });
      await setup?.(p);
      if (wayland) {
        const wl = join(work, `${name}-wl`);
        rmSync(wl, { recursive: true, force: true });
        mkdirSync(wl);
        // (--wayland names the socket, in a directory of the run's own)
        flags = [...flags, '--wayland', join(wl, 'wayland-0'), '--wayland-size', wayland.size ?? '640x480', '--wayland-out', wayland.out ?? 'null'];
      }
      const we = { WINEPREFIX: p, ...WENV, ...wineEnv };
      const argv = [gexc, 'run', ...flags, ...Object.entries(we).flatMap(([k, v]) => ['--env', `${k}=${v}`]), loader, ...args.map(String)];
      // Not the host's display: gexc --wayland sets WAYLAND_DISPLAY and
      // XDG_RUNTIME_DIR to its own compositor only when they are not given.
      const { WAYLAND_DISPLAY, XDG_RUNTIME_DIR, DISPLAY, ...host } = base;
      const r = await run(argv, { cwd: cwd ?? join(p, 'drive_c'), env: { ...host, ...more }, timeout, until, file: join(work, `${name}.log`), ...(split && { errFile: join(work, `${name}.err`) }) });
      return { ...r, pfx: p };
    },
    /**
     * Wine on the page's x86 shared shim with `args`: the install at
     * the session's mount, /wine32 or /wine64 (`dist`: another; `loader`: the host's loader, by default
     * the install's; `rel` the loader's path in the install, LOADER by
     * default), a copy of the prefix at /pfx with
     * `pfxFiles` in drive_c and `userReg` added to its user.reg (`setup`:
     * called with its path, then), an empty /t, `files` (`{ guest path: host path
     * }`), `dirs` more (`{ guest: host }`), `flags` run-node's own (before
     * the seed), `wineEnv` on top of the session's Wine environment, `env` node's, `cwd` the
     * guest's (default /t), `prefix` another prefix to copy (pagePrefix's
     * template). `single`: the single-thread build, as a page without
     * SharedArrayBuffer runs it (GEX_SINGLE=1, JSPI); `mem64` the memory64
     * shim and seed (GEX_MEM64=1); `split` stdout and stderr apart (run's
     * `errFile`); `rss` the run's peak RSS (KiB, node and its Workers) as
     * the result's `rss`. Resolves run's result plus `pfx`.
     */
    async page(args, { files = {}, pfxFiles = {}, userReg, dirs = {}, flags = [], wineEnv = {}, env: more = {}, dist: d = dist, loader: ld, rel = LOADER, cwd = '/t', setup, timeout = 300000, name = 'page',
      prefix: from = prefix, single = false, mem64 = false, split = false, rss = false } = {}) {
      const flavour = mem64 ? 'x86.m64.shared' : 'x86.shared';
      const seed = g.pub(`seed.${flavour}.wasm`);
      g.pub(`gex_browser.${flavour}.wasm`);
      const p = pfx(`${name}-pfx`, pfxFiles, { userReg, from });
      await setup?.(p);
      const tdir = join(work, `${name}-t`);
      rmSync(tdir, { recursive: true, force: true });
      mkdirSync(tdir);
      const we = { WINEPREFIX: '/pfx', ...WENV, ...wineEnv };
      const node = single ? [...(typeof WebAssembly.Suspending !== 'function' ? ['--experimental-wasm-jspi'] : []),
        '--import', 'data:text/javascript,delete globalThis.SharedArrayBuffer'] : [];
      const rssFile = join(work, `${name}.rss`);
      const peak = rss ? ['python3', '-c', 'import ctypes,resource,subprocess,sys; rc=subprocess.call(sys.argv[2:], preexec_fn=lambda: ctypes.CDLL(None).prctl(1, 9)); open(sys.argv[1],"w").write("%d\\n" % resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss); sys.exit(rc)', rssFile] : [];
      rmSync(rssFile, { force: true });
      // (the run's own kill reaches node too: its parent's death kills it)
      const argv = [...peak, process.execPath, ...node, '--max-old-space-size=8192', join(ROOT, 'hosts/browser/js/run-node.mjs'),
        '--dir', `${mount}=${d}`, '--dir', `/pfx=${p}`, '--dir', `/t=${tdir}`,
        ...Object.entries(dirs).flatMap(([to, from]) => ['--dir', `${to}=${from}`]),
        ...Object.entries(files).flatMap(([to, from]) => ['--file', `${to}=${from}`]),
        ...Object.entries(we).flatMap(([k, v]) => ['--env', `${k}=${v}`]),
        '--cwd', cwd, '--argv0', `${mount}/${rel}`, ...flags, seed, ld ?? join(d, rel), ...args.map(String)];
      const r = await run(argv, { cwd: ROOT, env: { ...base, GEX_ARCH: 'x86', GEX_SHARED: '1', ...(single && { GEX_SINGLE: '1' }), ...(mem64 && { GEX_MEM64: '1' }), ...more },
        timeout, file: join(work, `${name}.log`), ...(split && { errFile: join(work, `${name}.err`) }) });
      return { ...r, pfx: p, ...(rss && existsSync(rssFile) && { rss: Number(readFileSync(rssFile, 'utf8')) }) };
    },
    /** The page's prefix template, made once per work directory from the
     *  session's prefix (wine/page-prefix.sh: the session applied offline,
     *  the install marked current). */
    pagePrefix() {
      const out = join(top, 'page-pfx');
      if (existsSync(join(top, '.page-pfx-done'))) return out;
      const tmp = `${out}.tmp-${process.pid}`;
      const r = spawnSync(join(ROOT, 'wine/page-prefix.sh'), [prefix, dist, tmp], { encoding: 'utf8' });
      if (r.status !== 0) throw new Error(`wine/page-prefix.sh: ${String(r.stderr).trim().split('\n').slice(-3).join(' / ')}`);
      try {
        renameSync(tmp, out);
        writeFileSync(join(top, '.page-pfx-done'), '');
      } catch {
        // another test made it first
        rmSync(tmp, { recursive: true, force: true });
      }
      return out;
    },
    /** Whether the x86_64 side is the native PE core
     *  (x86_64-windows/winepe.wasm), its DLLs skeletons. */
    nativeCore: existsSync(join(dist, 'lib/wine/x86_64-windows/winepe.wasm')),
    /** Whether the loader has `what` (a Unix library's name it was linked with). */
    loaderHas(what) {
      loaderText ??= readFileSync(w.loader).toString('latin1');
      return loaderText.includes(what);
    },
    /** fixtures/wine/src's program `exe` for `arch` (the win-progs image);
     *  a throw when it did not build. */
    prog(exe, arch = 'x64') {
      const dir = join(t.workDir, 'win-progs');
      progsFailed ??= JSON.parse(readFileSync(join(t.workDir, 'win-progs.json'), 'utf8')).failed;
      const why = progsFailed[`${arch}/${exe.replace(/\.(exe|dll)$/, '.c')}`] ?? progsFailed[`${arch}/${exe.replace(/\.(exe|dll)$/, '.cpp')}`];
      if (why) throw new Error(`${arch}/${exe} did not build: ${why}`);
      return join(dir, arch, exe);
    },
    /** `src` built with the gexcc-win install GEX_TEST_GEXCC_WIN (a skip
     *  without it) as `out` in the run's directory, beside its .wasm:
     *  `args` the compiler's (g++ for a .cpp). */
    async gexccWin(src, out, args = []) {
      const cc = env.GEX_TEST_GEXCC_WIN || t.skip('GEX_TEST_GEXCC_WIN: a gexcc-win install (toolchain/gexcc/windows/install.sh)');
      const exe = join(work, out);
      mkdirSync(join(exe, '..'), { recursive: true });
      const r = await run([join(cc, 'bin', `x86_64-w64-mingw32-${/\.cpp$/.test(src) ? 'g++' : 'gcc'}`), ...args, '-o', exe, src], { file: `${exe}.build` });
      if (r.code !== 0) throw new Error(`gexcc-win ${basename(src)}: ${r.out.trim().split('\n').slice(-5).join(' / ')}`);
      return exe;
    },
    /**
     * A program case as 64-bit Wine's console tests have it: `exe` (a
     * win-progs name, or a path) and `extra` files in C:\, run natively
     * as C:\EXE with `args`; its exit must be `code` and its whole stdout
     * (CRs out, the last newline off) match `want`. `nullDriver`: Wine's
     * null display driver. `wayland`: the Wayland driver on a compositor
     * of the run's own (`{ size, colors, min, last, skip }`: its frames
     * checked by `frames`). `gl`: the host's GL. `wineEnv` the case's own.
     * `page`: on the page instead (`{ single, mem64, rss, flags }`, page's
     * own), in pagePrefix's prefix, the program C:\EXE in /pfx/drive_c;
     * an mmap ENOMEM ERR line from Wine fails it (the page's space ends at
     * 2 GiB: a Wine that takes it for 4 GiB tries the top for each stack).
     * `env`: node's or gexc's own. Checks it, and resolves the run.
     */
    async nt({ name, code = 0, want, exe, args = [], extra = [], nullDriver = false, wayland, gl = false, wineEnv = {}, timeout = 300000, page, env: more = {} }) {
      let r;
      try {
        const src = exe.includes('/') ? exe : w.prog(exe);
        const files = Object.fromEntries([src, ...extra].map((f) => [basename(f), f]));
        let userReg = nullDriver ? graphics('null') : '';
        if (wayland) userReg += `${graphics('wayland')}\n[Software\\\\Wine\\\\X11 Driver]\n"EmulateModeset"="Y"\n`;
        const out = wayland?.colors ? join(work, name) : undefined;
        // The prefix is made without a display (wine/dist.sh: wineboot with
        // no Wayland libraries): the driver's file, as wineboot would
        // install it.
        const setup = wayland && ((p) => {
          const drv = join(p, 'drive_c/windows/system32/winewayland.drv');
          if (!existsSync(drv)) copyFileSync(join(dist, 'lib/wine/x86_64-windows/winewayland.drv'), drv);
        });
        if (page && (wayland || gl)) throw new Error('the page runs without a display or GL');
        r = page
          ? await w.page([`C:\\${basename(src)}`, ...args], { pfxFiles: files, userReg, prefix: w.pagePrefix(), cwd: '/pfx/drive_c', wineEnv,
            env: { GEX_LOG: 'off', ...more }, single: page.single, mem64: page.mem64, rss: page.rss, flags: page.flags, split: true, timeout, name })
          : await w.native([`C:\\${basename(src)}`, ...args], { files, userReg, setup, wineEnv, flags: gl ? ['--gl'] : [],
            wayland: wayland && { size: wayland.size, out: out ?? 'null' }, env: { GEX_LOG: 'off', ...more }, split: true, timeout, name });
        // (the runtime's own lines, which a GEX_LOG past `off` says on stdout, out)
        r.stdout = r.out.replace(/\r/g, '').split('\n').filter((l) => !l.startsWith('[gex] ')).join('\n').replace(/\n$/, '');
        const oom = page ? (r.err.match(/mmap\(\) error Out of memory/g) ?? []).length : 0;
        let why = r.code !== code ? `exit ${r.code} (want ${code})` : oom ? `${oom} mmap ENOMEM ERR lines` : !new RegExp(`^(?:${want.source})$`).test(r.stdout) ? `its output is not /${want.source.slice(0, 120)}/` : '';
        if (!why && out) why = frames(out, wayland.colors, wayland) ?? '';
        t.check(`${name}: exit ${code}, its output${out ? ', its frames' : ''}`, !why, `${why}; stdout ${JSON.stringify(r.stdout.slice(-300))}; ${w.detail({ ...r, out: '' }, 6)}`);
      } catch (e) {
        if (e instanceof Skip) throw e;
        t.check(`${name}: exit ${code}, its output`, false, String(e?.message ?? e));
      }
      return r;
    },
    /** A run's last lines for a FAIL, without the host's. */
    detail: (r, n = 8) => {
      const lines = (r.err === undefined ? r.out : `${r.out}\n${r.err}`).split('\n');
      // why a process died, when one did: the runtime's own death lines
      // first, else Wine's last fatal ones
      const dead = lines.filter((l) => DEATH.test(l)).slice(0, 2);
      const why = (dead.length ? dead : lines.filter((l) => WINE_FATAL.test(l)).slice(-2)).map((l) => l.slice(0, 300));
      return [`${r.timedOut ? 'timed out; ' : ''}exit ${r.code}`, ...why.map((l) => `why: ${l}`),
        ...lines.filter((l) => l && !/^\[host\]|gex-console-error/.test(l)).slice(-n)].join(' / ');
    },
    /** This test's runs gone (the shared install and prefix stay), unless a check failed. */
    clean() {
      if (failed) t.say(`runs kept: ${work}`);
      else rmSync(work, { recursive: true, force: true });
    },
  };
  return w;
}

/** A Wine driver key for user.reg: `userReg` naming the graphics driver. */
export const graphics = (driver) => `\n[Software\\\\Wine\\\\Drivers]\n"Graphics"="${driver}"\n`;

/**
 * The two legs most of these tests are: fixtures/wow64/NAME.c built
 * (`build`: mingw's options), run natively as C:\NAME.exe and on the page as
 * /t/NAME.exe with `args`, each checked for `want` (a RegExp) in its output.
 * `native` and `page`: more of each leg's options (`false`: no such leg).
 */
export async function legs(t, name, { want, args = [], build = {}, native = {}, page = {}, mono = false } = {}) {
  const w = wow64(t, { mono });
  const exe = await w.mingw(name, build);
  if (native) {
    const r = await w.native([`C:\\${name}.exe`, ...args], { files: { [`${name}.exe`]: exe }, ...native });
    t.check(`native: ${want.source}`, want.test(r.out), w.detail(r));
  }
  if (page) {
    const r = await w.page([`/t/${name}.exe`, ...args], { files: { [`/t/${name}.exe`]: exe }, ...page });
    t.check(`page: ${want.source}`, want.test(r.out), w.detail(r));
  }
  w.clean();
}

/**
 * The legs of a program with a unit: fixtures/wow64/NAME.c built, its unit
 * (precompiled and guarded) in a cache with the provider modules and a
 * cache of the provider modules alone; then natively with its unit, on the
 * page with its unit (which must be the one that ran) and on the page on
 * tier-1 (the cache without it), each with `args` and checked for `want`.
 */
export async function unitLegs(t, name, { want, args = [], build = {}, timeout = 300000 } = {}) {
  const w = wow64(t);
  const exe = await w.mingw(name, build);
  const units = await w.unit(exe, await w.providers(join(w.work, 'units')));
  const tier1 = await w.providers(join(w.work, 'units-t1'));
  const n = await w.native([`C:\\${name}.exe`, ...args], { files: { [`${name}.exe`]: exe }, env: { GEX_CGEN_CACHE: units }, timeout });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const page = (cache, leg) => w.page([`/t/${name}.exe`, ...args], { files: { [`/t/${name}.exe`]: exe }, flags: ['--cgen-prefer'],
    env: { GEX_LOG: 'unit', GEX_CGEN_CACHE: cache }, timeout, name: leg });
  const p = await page(units, 'page');
  t.check('page: ran with its unit', new RegExp(`function tier for .*/${name}\\.exe`).test(p.out), w.detail(p));
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  const p1 = await page(tier1, 'page-t1');
  t.check(`page, tier-1: ${want.source}`, want.test(p1.out), w.detail(p1));
  w.clean();
}

/** A binary PPM (P6) at `path`: `{ w, h, px }`, px its RGB bytes. */
export function ppm(path) {
  const data = readFileSync(path);
  const fields = [];
  let i = 0;
  while (fields.length < 4) {
    while (/\s/.test(String.fromCharCode(data[i]))) i++;
    if (data[i] === 0x23) { i = data.indexOf(0x0a, i); continue; }
    let j = i;
    while (!/\s/.test(String.fromCharCode(data[j]))) j++;
    fields.push(data.subarray(i, j).toString('latin1'));
    i = j;
  }
  return { w: Number(fields[1]), h: Number(fields[2]), px: data.subarray(i + 1) };
}

/**
 * The frames gexc's compositor wrote (--wayland-out PREFIX: PREFIX-NNN.ppm,
 * binary P6) checked for a picture: one frame with at least `min` pixels of
 * every colour of `colors` ('R,G,B', exact; 'R,G,B:N' wants N of it).
 * `skip` ('WxH') leaves out the frames of that size (another surface's,
 * such as a GL child's); `last` wants each of the last N frames to have
 * them instead (a picture that has to stay). Null when it passes, else why.
 */
export function frames(prefix, colors, { min = 1000, skip, last = 0 } = {}) {
  const want = colors.map((a) => {
    const [c, n] = a.split(':');
    return { c, rgb: Buffer.from(c.split(',').map(Number)), n: n ? Number(n) : min };
  });
  const dir = join(prefix, '..');
  const ppm = (f) => {
    const data = readFileSync(join(dir, f));
    // magic, width, height, maxval; '#' comments
    const fields = [];
    let i = 0;
    while (fields.length < 4) {
      while (/\s/.test(String.fromCharCode(data[i]))) i++;
      if (data[i] === 0x23) { i = data.indexOf(0x0a, i); continue; }
      let j = i;
      while (!/\s/.test(String.fromCharCode(data[j]))) j++;
      fields.push(data.subarray(i, j).toString('latin1'));
      i = j;
    }
    return { size: `${fields[1]}x${fields[2]}`, px: data.subarray(i + 1) };
  };
  // a colour's three bytes met across two pixels count too, which these
  // distinct colours make rare enough not to matter
  const count = (px, rgb) => { let n = 0; for (let i = px.indexOf(rgb); i >= 0; i = px.indexOf(rgb, i + 1)) n++; return n; };
  let list = readdirSync(dir).filter((f) => f.startsWith(`${basename(prefix)}-`) && f.endsWith('.ppm')).sort().map((f) => ({ f, ...ppm(f) }));
  if (skip) list = list.filter((x) => x.size !== skip);
  if (last) {
    const tail = list.slice(-last);
    for (const x of tail) {
      const got = want.map((c) => [c, count(x.px, c.rgb)]);
      if (!got.every(([c, n]) => n >= c.n)) return `frame ${x.f} (of the last ${tail.length}) lacks a colour: ${got.map(([c, n]) => `${c.c}=${n}>=${c.n}`).join(' ')}`;
    }
    return tail.length === last ? null : `${list.length} frames, want at least ${last}`;
  }
  const best = new Map(want.map((c) => [c, 0]));
  for (const x of list) {
    const got = want.map((c) => [c, count(x.px, c.rgb)]);
    for (const [c, n] of got) best.set(c, Math.max(best.get(c), n));
    if (got.every(([c, n]) => n >= c.n)) return null;
  }
  return `no frame of ${list.length} has enough of each colour; best: ${[...best].map(([c, n]) => `${c.c}=${n}>=${c.n}`).join(' ')}`;
}
