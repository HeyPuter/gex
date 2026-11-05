// What a node-host test needs to run a 32-bit Windows program as a WoW64
// process of 64-bit Wine, natively under gexc and on the page's shim under
// node (hosts/browser/js/run-node.mjs): a WINE64_WOW64=1 install
// (GEX_TEST_WOW64_DIST, wine/dist.sh's WINE64_DIST) and its prefix
// (GEX_TEST_WOW64_PREFIX), each a skip when not set.
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
// the prefix with system32's and syswow64's builtins Wine placeholders, so
// the loader takes each builtin from the install, and services.exe waiting
// 60 s for a service, not Wine's 10 (a translated 64-bit service takes 7-10
// s on a busy host). Each leg copies that prefix again for itself.

import { execFileSync, spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  appendFileSync, closeSync, copyFileSync, existsSync, mkdirSync, openSync, readdirSync, readFileSync,
  renameSync, rmSync, statSync, writeFileSync,
} from 'node:fs';
import { join } from 'node:path';
import { guest, ROOT } from './guest.mjs';

export { ROOT };
export const FIXTURES = join(import.meta.dirname, '..', 'fixtures', 'wow64');
/** The loader, in the install. */
export const LOADER = 'lib/wine/x86_64-unix/wine';
// A run's lines saying why a process died: the runtime's, and Wine's fatal ones.
const DEATH = /^\[gex\] pid \d+ killed by|^\[gex\] pid \d+: a vfork child|gex: (process|thread) \S+ failed|panicked at|RuntimeError|RangeError/;
const WINE_FATAL = /^wine: |wineserver: |:err:(process|module|virtual|server|sync):/;
/** Wine's environment for a WoW64 process (without WINEGEX_WOW64 the server takes no i386 process). */
export const WINE_ENV = { WINEGEX_WOW64: '1', WINEDEBUG: 'err+all,fixme-all' };

let runs = 0;

/** `argv` with stdout and stderr in one file, in the order written (`2>&1`),
 *  and `input` (if given) on stdin: `{ code, out, timedOut }`, `code` as a
 *  shell gives it. */
export function run(argv, { cwd = ROOT, env = process.env, timeout = 300000, file, input } = {}) {
  return new Promise((resolve) => {
    const fd = openSync(file, 'w');
    const p = spawn(argv[0], argv.slice(1), { cwd, env, stdio: [input === undefined ? 'ignore' : 'pipe', fd, fd] });
    closeSync(fd);
    if (input !== undefined) { p.stdin.on('error', () => {}); p.stdin.end(input); }
    let timedOut = false;
    const timer = setTimeout(() => { timedOut = true; p.kill('SIGKILL'); }, timeout);
    p.on('error', (e) => { clearTimeout(timer); resolve({ code: 127, out: String(e.message), timedOut }); });
    p.on('close', (code, signal) => {
      clearTimeout(timer);
      const sig = { SIGABRT: 6, SIGKILL: 9, SIGSEGV: 11, SIGTERM: 15 }[signal];
      resolve({ code: code ?? 128 + (sig ?? 0), out: readFileSync(file, 'utf8'), timedOut });
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

function prepare(t, dist, prefix, mono) {
  const key = createHash('sha256').update([dist, statSync(dist).mtimeMs, prefix, statSync(prefix).mtimeMs, mono ?? ''].join('\n')).digest('hex').slice(0, 16);
  const top = join(t.workDir, 'wow64', key);
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
  execFileSync('cp', ['-a', prefix, join(tmp, 'pfx')]);
  const win = join(tmp, 'pfx/drive_c/windows');
  placeholders(join(win, 'system32'), 'amd64', join(dist, 'lib/wine/x86_64-windows'));
  placeholders(join(win, 'syswow64'), 'i386', join(dist, 'lib/wine/i386-windows'));
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
  const g = guest(t);
  const env = process.env;
  const distSrc = env.GEX_TEST_WOW64_DIST || t.skip('GEX_TEST_WOW64_DIST: a WINE64_WOW64=1 install (wine/dist.sh)');
  const pfxSrc = env.GEX_TEST_WOW64_PREFIX || t.skip('GEX_TEST_WOW64_PREFIX: its WoW64 prefix');
  if (!existsSync(join(distSrc, 'lib/wine/i386-windows'))) t.skip(`${distSrc} has no lib/wine/i386-windows (WINE64_WOW64=1)`);
  // wine/dist.sh's WINE64_PREFIX, the session prefix: the bare wineboot
  // prefix beside it has no display driver, and no window opens
  if (!existsSync(join(pfxSrc, 'drive_c/windows/syswow64/winewayland.drv')))
    throw new Error(`${pfxSrc} has no syswow64/winewayland.drv: not wine/dist.sh's WINE64_PREFIX (pfs64-*), the session prefix`);
  const monoSrc = mono ? (env.GEX_TEST_WOW64_MONO || t.skip('GEX_TEST_WOW64_MONO: Wine Mono, both halves, unpacked')) : null;
  const gexc = env.GEXC_X86 || env.GEXC || join(ROOT, 'target/x86/release/gexc');
  const mingw = g.tool('i686-w64-mingw32-gcc');
  const top = prepare(t, distSrc, pfxSrc, monoSrc);
  const dist = join(top, 'dist');
  const prefix = join(top, 'pfx');
  const work = join(t.workDir, 'wow64-runs', `${process.pid}-${++runs}`);
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
  const w = {
    dist,
    prefix,
    work,
    gexc,
    loader: join(dist, LOADER),
    /** fixtures/wow64/NAME.c (`src`: another source) built with
     *  i686-w64-mingw32-gcc (-O1, `flags`, then `libs`; `x64`:
     *  x86_64-w64-mingw32-gcc instead). */
    async mingw(name, { flags = [], libs = [], out = `${name}.exe`, x64 = false, src = join(FIXTURES, `${name}.c`) } = {}) {
      const exe = join(work, out);
      const r = await run([x64 ? g.tool('x86_64-w64-mingw32-gcc') : mingw, '-O1', ...flags, '-o', exe, src, ...libs], { file: `${exe}.log` });
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
     * WINE_ENV (each a --env), `flags` gexc run's own (`wayland`: a
     * compositor of its own, no output), `env` gexc's, `loader` another
     * loader. A skip when gexc is not built. Resolves run's result plus
     * `pfx`, the prefix it used.
     */
    async native(args, { files = {}, wineEnv = {}, flags = [], env: more = {}, prefix: from = prefix, userReg, setup, wayland = false, loader = w.loader, cwd, timeout = 300000, name = 'native' } = {}) {
      if (!existsSync(gexc)) t.skip(`${gexc} is not built`);
      const p = pfx(`${name}-pfx`, files, { userReg, from });
      await setup?.(p);
      if (wayland) {
        const wl = join(work, `${name}-wl`);
        rmSync(wl, { recursive: true, force: true });
        mkdirSync(wl);
        // (--wayland names the socket, in a directory of the run's own)
        flags = [...flags, '--wayland', join(wl, 'wayland-0'), '--wayland-size', '640x480', '--wayland-out', 'null'];
      }
      const we = { WINEPREFIX: p, ...WINE_ENV, ...wineEnv };
      const argv = [gexc, 'run', ...flags, ...Object.entries(we).flatMap(([k, v]) => ['--env', `${k}=${v}`]), loader, ...args.map(String)];
      // Not the host's display: gexc --wayland sets WAYLAND_DISPLAY and
      // XDG_RUNTIME_DIR to its own compositor only when they are not given.
      const { WAYLAND_DISPLAY, XDG_RUNTIME_DIR, DISPLAY, ...host } = base;
      const r = await run(argv, { cwd: cwd ?? join(p, 'drive_c'), env: { ...host, ...more }, timeout, file: join(work, `${name}.log`) });
      return { ...r, pfx: p };
    },
    /**
     * Wine on the page's x86 shared shim with `args`: the install at
     * /wine32 (`dist`: another; `loader`: the host's loader, by default
     * the install's), a copy of the prefix at /pfx with
     * `pfxFiles` in drive_c and `userReg` added to its user.reg (`setup`:
     * called with its path, then), an empty /t, `files` (`{ guest path: host path
     * }`), `dirs` more (`{ guest: host }`), `flags` run-node's own (before
     * the seed), `wineEnv` on top of WINE_ENV, `env` node's, `cwd` the
     * guest's (default /t). Resolves run's result plus `pfx`.
     */
    async page(args, { files = {}, pfxFiles = {}, userReg, dirs = {}, flags = [], wineEnv = {}, env: more = {}, dist: d = dist, loader: ld, cwd = '/t', setup, timeout = 300000, name = 'page' } = {}) {
      const seed = g.pub('seed.x86.shared.wasm');
      g.pub('gex_browser.x86.shared.wasm');
      const p = pfx(`${name}-pfx`, pfxFiles, { userReg });
      await setup?.(p);
      const tdir = join(work, `${name}-t`);
      rmSync(tdir, { recursive: true, force: true });
      mkdirSync(tdir);
      const we = { WINEPREFIX: '/pfx', ...WINE_ENV, ...wineEnv };
      const argv = [process.execPath, '--max-old-space-size=8192', join(ROOT, 'hosts/browser/js/run-node.mjs'),
        '--dir', `/wine32=${d}`, '--dir', `/pfx=${p}`, '--dir', `/t=${tdir}`,
        ...Object.entries(dirs).flatMap(([to, from]) => ['--dir', `${to}=${from}`]),
        ...Object.entries(files).flatMap(([to, from]) => ['--file', `${to}=${from}`]),
        ...Object.entries(we).flatMap(([k, v]) => ['--env', `${k}=${v}`]),
        '--cwd', cwd, '--argv0', `/wine32/${LOADER}`, ...flags, seed, ld ?? join(d, LOADER), ...args.map(String)];
      const r = await run(argv, { cwd: ROOT, env: { ...base, GEX_ARCH: 'x86', GEX_SHARED: '1', ...more }, timeout, file: join(work, `${name}.log`) });
      return { ...r, pfx: p };
    },
    /** A run's last lines for a FAIL, without the host's. */
    detail: (r, n = 8) => {
      const lines = r.out.split('\n');
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
