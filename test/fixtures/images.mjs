// The images tests name in `meta.images`, made into the runner's work
// directory before any test runs. Each `prepare(work)` resolves null when
// ready, or the reason it cannot be (the tests that need it are skipped).
//
//   busybox  bb.tar.zst: BUSYBOX (a wasm32 busybox) and its applets, the probe
//            programs (probe/*.c, built with GEXCC or gexcc), /etc/xattr with
//            an extended attribute; and lazy.bin, a lazy file's bytes. Kept
//            while its inputs are unchanged, so a run without gexcc can reuse it.
//   slim     slim/ (debian-slim.tar.zst, repo/) from GEX_TEST_SLIM, and
//            slim-repo.json listing the repository.
//   package  pkg/: the runtime package, web/runtime/build.sh.
//   win-progs  win-progs.tar: the Windows test programs (fixtures/wine/src)
//            built with MinGW for x64/ and x86/ (a source's `gex-nt-cc:`,
//            `gex-nt-arch:`, `gex-nt-libs:` and `gex-nt-shared` lines: its
//            compiler, its one arch, its libraries, a DLL); win-progs.json says which
//            did not build, and why.
//   wine64   wine64.tar and wine64-lazy.json: 64-bit Wine's install
//            (GEX_TEST_WINE64_DIST) at /wine64 and a page prefix made from
//            GEX_TEST_WINE64_PREFIX (tools/wine-wasm/page-prefix.sh) at
//            /pfx; files over 64 KiB are lazy, served from wine64/ in the
//            work directory (fixtures/wine/pack.py).
//   wine64-units  wine64-units/: function-tier units (a unit cache, `gexc
//            cgen-image`'s) for the programs in wine/units.mjs and Wine's core DLLs at the bases a
//            run of them names (fixtures/wine/unit-bases.mjs), and the NT
//            trampolines. Built with GEX_TEST_GEXC (default target/x86's
//            gexc); a few minutes.
//   gl-probes  gl-probes-ARCH.tar: the guest GL (guest/gl/build.sh) and each
//            self-checking probe in guest/gl/probes built against it
//            (/probes/NAME), with a loader and libc: x86-64 glibc with gcc and
//            the host's own, aarch64 musl with ~/toolchains/aarch64-linux-musl-cross
//            when it is there; and
//            gl-probes.json, each probe's line from a run on the host's own
//            Mesa (software, surfaceless), which the guest's run must print;
//            and gles-gears (/probes/gles-gears) with its coverage there.
//            Needs gcc, the host's EGL and GL headers and libraries, and
//            third_party/opengl-registry.

import { execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, linkSync, mkdirSync, readdirSync, readFileSync, realpathSync, rmSync, statSync, symlinkSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { UNIT_PROGS } from './wine/units.mjs';

const HERE = import.meta.dirname;
const ROOT = join(HERE, '..', '..');

/** The probe programs and the libraries each links. */
const PROBES = {
  'rt-probe': ['-lm'],
  'rt-wl': ['-lwayland-client', '-lffi'],
  'rt-joy': [],
  'rt-x11': ['-lX11', '-lX11-xcb', '-lxcb-xfixes', '-lxcb', '-lXau', '-lXdmcp'],
};

/** What each image leaves in the work directory: with `--prebuilt`, an
 *  image whose files are not there is missing. */
// The `wine64-units` image: Wine's DLLs that get a unit (where every
// program spends its calls), beside the test programs (wine/units.mjs).
const UNIT_DLLS = ['ntdll.dll', 'kernelbase.dll', 'kernel32.dll', 'msvcrt.dll', 'ucrtbase.dll'];

export const outputs = { busybox: ['bb.tar.zst'], slim: ['slim'], package: ['pkg'], 'gl-probes': ['gl-probes.json'], wine64: ['wine64.tar', 'wine64-lazy.json', 'wine64'], 'win-progs': ['win-progs.tar'], 'wine64-units': ['wine64-units'] };

export const images = {
  async busybox(work) {
    const bb = process.env.BUSYBOX;
    const applets = readFileSync(join(ROOT, 'tools/wine-wasm/busybox.applets'), 'utf8').split('\n').filter(Boolean);
    const h = createHash('sha256');
    if (bb && existsSync(bb)) h.update(readFileSync(bb));
    h.update(applets.join('\n'));
    for (const name of Object.keys(PROBES)) h.update(readFileSync(join(HERE, 'probe', `${name}.c`)));
    const key = h.digest('hex');
    const keyFile = join(work, 'bb.key');
    writeFileSync(join(work, 'lazy.bin'), Buffer.alloc(3 << 20, 7));
    const have = existsSync(join(work, 'bb.tar.zst')) && existsSync(keyFile) && readFileSync(keyFile, 'utf8');
    // Without busybox here, what a run before made (a copy where there is no
    // gexcc); else only if made from the same inputs.
    if (have && (!bb || have === key)) return null;
    if (!bb || !existsSync(bb)) return 'BUSYBOX: a wasm32 busybox';
    const tree = join(work, 'tree');
    rmSync(tree, { recursive: true, force: true });
    for (const d of ['bin', 'usr/bin', 'etc', 'tmp', 'root', 'home/user', 'dev', 'proc']) mkdirSync(join(tree, d), { recursive: true });
    copyFileSync(bb, join(tree, 'bin/busybox'));
    for (const name of applets) linkSync(join(tree, 'bin/busybox'), join(tree, 'bin', name));
    const cc = process.env.GEXCC || 'gexcc';
    for (const [name, libs] of Object.entries(PROBES)) {
      execFileSync(cc, ['-O2', '-Wall', '-o', join(tree, 'bin', name), join(HERE, 'probe', `${name}.c`), ...libs], { stdio: ['ignore', 'ignore', 'inherit'] });
    }
    execFileSync('sh', ['-c', 'echo xattr > "$1/etc/xattr" && setfattr -n user.rt -v kept "$1/etc/xattr"', 'sh', tree]);
    execFileSync('tar', ['--xattrs', '-cf', join(work, 'bb.tar'), '-C', tree, '.']);
    execFileSync('zstd', ['-q', '-f', '-3', join(work, 'bb.tar'), '-o', join(work, 'bb.tar.zst')]);
    writeFileSync(keyFile, key);
    return null;
  },

  async slim(work) {
    const src = process.env.GEX_TEST_SLIM;
    if (!src || !existsSync(join(src, 'debian-slim.tar.zst'))) return 'GEX_TEST_SLIM: a directory with debian-slim.tar.zst and repo/';
    rmSync(join(work, 'slim'), { recursive: true, force: true });
    symlinkSync(realpathSync(src), join(work, 'slim'));
    const list = [];
    const walk = (d, rel) => {
      for (const e of readdirSync(d, { withFileTypes: true })) {
        const p = `${rel}/${e.name}`;
        if (e.isDirectory()) {
          list.push([p, 0, 'directory']);
          walk(join(d, e.name), p);
        } else if (e.isFile()) list.push([p, statSync(join(d, e.name)).size, 'file']);
      }
    };
    walk(join(src, 'repo'), '');
    writeFileSync(join(work, 'slim-repo.json'), JSON.stringify(list));
    return null;
  },

  async 'gl-probes'(work) {
    const reg = join(ROOT, 'third_party/opengl-registry/api');
    if (!existsSync(reg)) return 'third_party/opengl-registry is not checked out';
    const probes = join(ROOT, 'guest/gl/probes');
    const names = readdirSync(probes).filter((f) => /^gl4\d-.*-probe\.c$/.test(f)).map((f) => f.slice(0, -2));
    const version = join(ROOT, 'test/fixtures/programs/gl-version.c');
    const musl = join(homedir(), 'toolchains/aarch64-linux-musl-cross');
    const h = createHash('sha256');
    for (const f of readdirSync(join(ROOT, 'guest/gl')).sort()) {
      const p = join(ROOT, 'guest/gl', f);
      if (statSync(p).isFile()) h.update(f).update(readFileSync(p));
    }
    h.update(readFileSync(join(ROOT, 'crates/gex-gl/src/proto.rs')));
    for (const n of names) h.update(n).update(readFileSync(join(probes, `${n}.c`)));
    h.update(readFileSync(version));
    const gears = join(probes, 'gles-gears.c');
    h.update(readFileSync(gears));
    h.update(readFileSync(new URL(import.meta.url))); // how it is built
    h.update(String(existsSync(musl)));
    const key = h.digest('hex');
    const keyFile = join(work, 'gl-probes.key');
    if (existsSync(join(work, 'gl-probes.json')) && existsSync(keyFile) && readFileSync(keyFile, 'utf8') === key) return null;
    const dir = join(work, 'gl-probes');
    rmSync(dir, { recursive: true, force: true });
    for (const a of ['x86', 'a64']) rmSync(join(work, `gl-probes-${a}.tar`), { force: true });
    // EGL's and KHR's headers are the host's for both arches: they hold no
    // more than declarations.
    const inc = join(dir, 'include');
    mkdirSync(inc, { recursive: true });
    for (const d of ['EGL', 'KHR']) execFileSync('cp', ['-r', `/usr/include/${d}`, inc]);
    const run = (cmd, args, env = {}) => execFileSync(cmd, args, { cwd: ROOT, env: { ...process.env, ...env }, stdio: ['ignore', 'ignore', 'pipe'] });
    // The reference lines: the host's Mesa, in software, with no display.
    const expect = {};
    mkdirSync(join(dir, 'native'), { recursive: true });
    for (const n of names) {
      const src = join(probes, `${n}.c`);
      const ref = join(dir, 'native', n);
      let line = null;
      try {
        run('cc', ['-O2', `-I${reg}`, '-o', ref, src, '-lEGL', '-lGL', '-lm']);
        const out = execFileSync(ref, { encoding: 'utf8', timeout: 60000, env: { ...process.env, EGL_PLATFORM: 'surfaceless', LIBGL_ALWAYS_SOFTWARE: '1', GEX_NATIVE_REFERENCE: '1' }, stdio: ['ignore', 'pipe', 'pipe'] });
        line = out.split('\n').filter((l) => l.trim()).pop() ?? null;
      } catch {
        line = null;
      }
      // The GL the probe needs: its highest `#version` or the context it asks for.
      const text = readFileSync(src, 'utf8');
      const glsl = [...text.matchAll(/#version (\d+)(?! es)/g)].map((m) => Number(m[1]));
      const ctx = text.match(/EGL_CONTEXT_MAJOR_VERSION, *(\d+)[^]*?EGL_CONTEXT_MINOR_VERSION, *(\d+)/);
      const needs = Math.max(0, ...glsl, ctx ? Number(ctx[1]) * 100 + Number(ctx[2]) * 10 : 0);
      expect[n] = { want: line, needs };
    }
    // gles-gears' coverage on the host's Mesa, which the guest's should match.
    try {
      run('cc', ['-O2', `-I${reg}`, '-o', join(dir, 'native/gles-gears'), gears, '-lEGL', '-lGLESv2', '-lm']);
      const out = execFileSync(join(dir, 'native/gles-gears'), ['30'], { encoding: 'utf8', timeout: 60000, env: { ...process.env, EGL_PLATFORM: 'surfaceless', LIBGL_ALWAYS_SOFTWARE: '1' }, stdio: ['ignore', 'pipe', 'pipe'] });
      expect['gles-gears'] = { covered: Number(/covered (\d+)%/.exec(out)?.[1] ?? NaN) };
    } catch {
      expect['gles-gears'] = { covered: null };
    }
    // Each arch's tree: its guest GL, its loader and libc, the probes.
    const arches = {
      x86: { cc: 'gcc', lib: 'usr/lib/x86_64-linux-gnu', runtime: [['/lib64/ld-linux-x86-64.so.2', 'lib64/ld-linux-x86-64.so.2'], ['/lib/x86_64-linux-gnu/libc.so.6', 'usr/lib/x86_64-linux-gnu/libc.so.6'], ['/lib/x86_64-linux-gnu/libm.so.6', 'usr/lib/x86_64-linux-gnu/libm.so.6']] },
      a64: existsSync(musl) && { cc: join(musl, 'bin/aarch64-linux-musl-gcc'), lib: 'usr/lib', runtime: [[join(musl, 'aarch64-linux-musl/lib/libc.so'), 'lib/ld-musl-aarch64.so.1']] },
    };
    for (const [a, x] of Object.entries(arches)) {
      if (!x) continue;
      const tree = join(dir, `tree-${a}`);
      const gl = join(dir, `gl-${a}`);
      mkdirSync(join(tree, x.lib), { recursive: true });
      mkdirSync(join(tree, 'probes'), { recursive: true });
      run('sh', [join(ROOT, 'guest/gl/build.sh'), gl], { GEX_CC: x.cc });
      for (const f of ['libGLESv2.so.2', 'libEGL.so.1', 'libGL.so.1']) copyFileSync(join(gl, f), join(tree, x.lib, f));
      for (const [from, to] of x.runtime) {
        mkdirSync(dirname(join(tree, to)), { recursive: true });
        copyFileSync(realpathSync(from), join(tree, to));
      }
      const link = [`-L${gl}`, '-l:libEGL.so.1', '-l:libGL.so.1', '-lm'];
      for (const n of names) run(x.cc, ['-O2', `-I${reg}`, `-I${inc}`, '-o', join(tree, 'probes', n), join(probes, `${n}.c`), ...link]);
      run(x.cc, ['-O2', `-I${inc}`, '-o', join(tree, 'probes/gl-version'), version, ...link]);
      run(x.cc, ['-O2', `-I${reg}`, `-I${inc}`, '-o', join(tree, 'probes/gles-gears'), gears, `-L${gl}`, '-l:libEGL.so.1', '-l:libGLESv2.so.2', '-lm']);
      execFileSync('tar', ['-cf', join(work, `gl-probes-${a}.tar`), '-C', tree, '.']);
    }
    writeFileSync(join(work, 'gl-probes.json'), JSON.stringify(expect, null, 1));
    writeFileSync(keyFile, key);
    return null;
  },

  async wine64(work) {
    const dist = process.env.GEX_TEST_WINE64_DIST;
    const pfx = process.env.GEX_TEST_WINE64_PREFIX;
    if (!dist || !pfx) return 'set GEX_TEST_WINE64_DIST and GEX_TEST_WINE64_PREFIX (tools/wine-wasm/wine64-dist.sh)';
    if (!existsSync(join(dist, 'lib/wine/x86_64-unix/wine'))) return `no Wine loader in ${dist}`;
    const h = createHash('sha256').update(realpathSync(dist)).update(realpathSync(pfx));
    for (const f of [join(dist, 'lib/wine/x86_64-unix/wine'), join(dist, 'lib/wine/x86_64-unix/ntdll.so'), join(pfx, 'system.reg'), join(ROOT, 'tools/wine-wasm/page-prefix.sh')]) {
      if (existsSync(f)) h.update(f).update(String(statSync(f).mtimeMs)).update(String(statSync(f).size));
    }
    h.update(readFileSync(new URL(import.meta.url))).update(readFileSync(join(import.meta.dirname, 'wine/pack.py')));
    const key = h.digest('hex');
    const keyFile = join(work, 'wine64.key');
    if (existsSync(join(work, 'wine64.tar')) && existsSync(keyFile) && readFileSync(keyFile, 'utf8') === key) return null;
    const dir = join(work, 'wine64');
    rmSync(dir, { recursive: true, force: true });
    mkdirSync(dir, { recursive: true });
    // The page's prefix: the installed one made current for the dist, its
    // links resolved (tools/wine-wasm/page-prefix.sh).
    execFileSync('bash', [join(ROOT, 'tools/wine-wasm/page-prefix.sh'), pfx, dist, join(dir, 'pfx')], { stdio: ['ignore', 'ignore', 'pipe'] });
    symlinkSync(realpathSync(dist), join(dir, 'dist'));
    // Small files in the tar; the rest fetched as read, from the work
    // directory (the dist through its link).
    execFileSync('python3', ['-I', join(import.meta.dirname, 'wine/pack.py'), join(work, 'wine64.tar'), join(work, 'wine64-lazy.json'), './wine64/',
      `${realpathSync(dist)}=/wine64,dist,follow`, `${join(dir, 'pfx')}=/pfx,pfx`], { stdio: ['ignore', 'ignore', 'pipe'] });
    writeFileSync(keyFile, key);
    return null;
  },

  async 'win-progs'(work) {
    // Each source's compiler from its `gex-nt-cc:` line (ARCH the triple's
    // arch), MinGW gcc otherwise.
    const src = join(import.meta.dirname, 'wine/src');
    const h = createHash('sha256');
    const names = readdirSync(src).filter((f) => /\.(c|cpp)$/.test(f)).sort();
    for (const f of readdirSync(src).sort()) h.update(f).update(readFileSync(join(src, f)));
    h.update(readFileSync(new URL(import.meta.url)));
    const key = h.digest('hex');
    const keyFile = join(work, 'win-progs.key');
    if (existsSync(join(work, 'win-progs.tar')) && existsSync(keyFile) && readFileSync(keyFile, 'utf8') === key) return null;
    const dir = join(work, 'win-progs');
    rmSync(dir, { recursive: true, force: true });
    const failed = {};
    for (const [arch, triple] of [['x64', 'x86_64'], ['x86', 'i686']]) {
      mkdirSync(join(dir, arch), { recursive: true });
      for (const f of names) {
        const text = readFileSync(join(src, f), 'utf8');
        const cc = (/gex-nt-cc: *(.*)/.exec(text)?.[1].replace(/\*\/\s*$/, "").trim().replaceAll('ARCH', triple) ?? `${triple}-w64-mingw32-gcc`).split(/\s+/);
        // `gex-nt-arch: x86|x64`: built for that one only; `gex-nt-libs:`
        // after the source, where the linker wants libraries.
        const only = /gex-nt-arch: *(\w+)/.exec(text)?.[1];
        if (only && only !== arch) continue;
        const libs = (/gex-nt-libs: *(.*)/.exec(text)?.[1].replace(/\*\/\s*$/, '').trim() ?? '').split(/\s+/).filter(Boolean);
        // std::thread needs MinGW's POSIX thread model, where the default is win32.
        if (/-g\+\+$/.test(cc[0]) && existsSync(`/usr/bin/${cc[0]}-posix`)) cc[0] += '-posix';
        // One that does not build is missing, and said so (win-progs.json);
        // the others are still made.
        try {
          // `gex-nt-shared`: a DLL.
          const dll = /gex-nt-shared/.test(text);
          execFileSync(cc[0], [...cc.slice(1), '-O2', '-s', '-Wl,--no-insert-timestamp', ...(dll ? ['-shared'] : []), '-o', join(dir, arch, f.replace(/\.[a-z]+$/, dll ? '.dll' : '.exe')), join(src, f), ...libs], { stdio: ['ignore', 'ignore', 'pipe'] });
        } catch (e) {
          failed[`${arch}/${f}`] = String(e.stderr ?? e.message).split('\n').find((l) => /error/.test(l)) ?? String(e.message).split('\n')[0];
        }
      }
    }
    writeFileSync(join(work, 'win-progs.json'), JSON.stringify({ failed }, null, 1));
    execFileSync('tar', ['-cf', join(work, 'win-progs.tar'), '-C', dir, '.']);
    writeFileSync(keyFile, key);
    return null;
  },

  async 'wine64-units'(work) {
    for (const dep of ['wine64', 'win-progs']) {
      const why = await images[dep](work);
      if (why) return why;
    }
    const gexc = process.env.GEX_TEST_GEXC ?? join(ROOT, 'target/x86/release/gexc');
    if (!existsSync(gexc)) return `no ${gexc} (cargo build --release -p gexc --features x86 --target-dir target/x86, or GEX_TEST_GEXC)`;
    const h = createHash('sha256').update(String(statSync(gexc).mtimeMs)).update(String(statSync(gexc).size));
    for (const f of ['wine64.key', 'win-progs.key']) h.update(readFileSync(join(work, f)));
    h.update(readFileSync(new URL(import.meta.url))).update(readFileSync(join(HERE, 'wine/unit-bases.mjs')));
    const key = h.digest('hex');
    const keyFile = join(work, 'wine64-units.key');
    const dir = join(work, 'wine64-units');
    if (existsSync(dir) && existsSync(keyFile) && readFileSync(keyFile, 'utf8') === key) return null;
    rmSync(dir, { recursive: true, force: true });
    mkdirSync(dir, { recursive: true });
    // The images and bases a run names as having no unit (with the flavour
    // it wants: GEX_CGEN_GUARD=1 when it says so), of Wine's core and the
    // test programs, and the NT trampolines.
    // A system per program: a wineserver that exited leaves its socket's
    // name bound (page/unix-rebind).
    let said = '';
    for (const exe of UNIT_PROGS) {
      const r = spawnSync('node', [join(HERE, 'wine/unit-bases.mjs'), work, exe], { encoding: 'utf8', maxBuffer: 1 << 30 });
      said += `${r.stdout}\n${r.stderr}\n`;
      if (r.status !== 0) return `the run of ${exe} that names the images failed (exit ${r.status}): ${`${r.stdout}\n${r.stderr}`.trim().split('\n').slice(-3).join(' / ')}`;
    }
    const guard = /GEX_CGEN_GUARD=1 gexc cgen-image/.test(said);
    const progs = join(work, 'wine64-units-progs');
    rmSync(progs, { recursive: true, force: true });
    mkdirSync(progs);
    execFileSync('tar', ['-xf', join(work, 'win-progs.tar'), '-C', progs, ...UNIT_PROGS.map((e) => `./x64/${e}`)]);
    const builds = new Set();
    for (const m of said.matchAll(/gex-pe: image (\S+) at base (0x[0-9a-f]+): no function-tier unit/g)) {
      const [, path, base] = m;
      const name = path.split('/').pop().toLowerCase();
      const host = path.startsWith('/wine64/') && UNIT_DLLS.includes(name) ? join(realpathSync(process.env.GEX_TEST_WINE64_DIST), path.slice(8))
        : UNIT_PROGS.includes(name) ? join(progs, 'x64', name) : null;
      if (host) builds.add(JSON.stringify([host, '--pe-base', base, '--jobs', '8']));
    }
    if (/gex-pe: missing unit: \[the trampolines\]/.test(said)) builds.add(JSON.stringify(['--nt-trampolines64']));
    if (![...builds].some((b) => b.includes('ntdll.dll'))) return 'the run named no base for ntdll.dll';
    for (const b of builds) {
      try {
        execFileSync('nice', [gexc, 'cgen-image', ...JSON.parse(b)], { env: { ...process.env, GEX_CGEN_CACHE: dir, ...(guard ? { GEX_CGEN_GUARD: '1' } : {}) }, stdio: ['ignore', 'ignore', 'pipe'] });
      } catch (e) {
        return `gexc cgen-image ${JSON.parse(b).join(' ')}: ${String(e.stderr ?? e.message).trim().split('\n').slice(-2).join(' / ')}`;
      }
    }
    rmSync(progs, { recursive: true, force: true });
    writeFileSync(keyFile, key);
    return null;
  },

  async package(work) {
    execFileSync(join(ROOT, 'web/runtime/build.sh'), [join(work, 'pkg')], { stdio: ['ignore', 'ignore', 'inherit'] });
    return null;
  },
};
