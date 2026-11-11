// The images tests name in `meta.images`, made into the runner's work
// directory before any test runs. Each `prepare(work)` resolves null when
// ready, or the reason it cannot be (the tests that need it are skipped).
//
//   busybox  bb.tar.zst: BUSYBOX (a wasm32 busybox) and its applets, the probe
//            programs (probe/*.c, built with GEXCC or gexcc; rt-truncate,
//            rt-repaint, rt-deadclient and rt-sockpass include programs/ and
//            native/wasm32/ sources), /etc/xattr with
//            an extended attribute; and lazy.bin and lazy-program.bin, a lazy
//            file's bytes (one test's each), and rt-side.so, a side module
//            rt-dlstatic loads (a lazy file's). Kept
//            while its inputs are unchanged, so a run without gexcc can reuse it.
//   slim     slim/ (debian-slim.tar.zst, repo/) from GEX_TEST_SLIM, and
//            slim-repo.json listing the repository.
//   package  pkg/: the runtime package, page/runtime/build.sh (--single:
//            @gexemu/runtime-single).
//   win-progs  win-progs.tar: the Windows test programs (fixtures/wine/src)
//            built with MinGW for x64/ and x86/ (a source's `gex-nt-cc:`,
//            `gex-nt-arch:`, `gex-nt-libs:` and `gex-nt-shared` lines: its
//            compiler, its one arch, its libraries, a DLL); win-progs.json says which
//            did not build, and why.
//   win-media  win-media.tar: media files for wine/media (MEDIA), made with the
//            host's ffmpeg.
//   wine64   wine64.tar and wine64-lazy.json: 64-bit Wine's install
//            (GEX_TEST_WINE64_DIST) at /wine64 and a page prefix made from
//            GEX_TEST_WINE64_PREFIX (wine/page-prefix.sh) at
//            /pfx; files over 64 KiB are lazy, served from wine64/ in the
//            work directory (fixtures/wine/pack.py).
//   wine64-units  wine64-units/: function-tier units (a unit cache, `gexc
//            cgen-image`'s) for the programs in wine/units.mjs and Wine's core DLLs (when translated, not a native PE core's) at the bases a
//            run of them names (fixtures/wine/unit-bases.mjs), and the NT
//            trampolines. Built with GEX_TEST_GEXC (default target/x86's
//            gexc); a few minutes.
//   gl-probes  gl-probes-ARCH.tar: the guest GL (guest/gl/build.sh) and each
//            self-checking probe in guest/gl/probes built against it
//            (/probes/NAME), with a loader and libc: x86-64 glibc with gcc and
//            the host's own, aarch64 musl with GEX_TOOLCHAIN (make toolchains)
//            when it is there; and
//            gl-probes.json, each probe's line from a run on the host's own
//            Mesa (software, surfaceless), which the guest's run must print;
//            and gles-gears (/probes/gles-gears) with its coverage there.
//            x86's also has gles-window (/probes/gles-window), an X client
//            with the host's Xlib beside it, and gles-window-xcb
//            (/probes/gles-window-xcb), the same with libxcb alone through
//            EGL_EXT_platform_xcb, and gles-window-fbo-xcb
//            (/probes/gles-window-fbo-xcb), that presenting as wgpu does,
//            and gles-window-xcb-under (/probes/gles-window-xcb-under), one
//            at an offset with a window over part of it, when the host has
//            Xlib.
//            Needs gcc, the host's EGL and GL headers and libraries, and
//            third_party/opengl-registry.

import { execFile, execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, linkSync, mkdirSync, readdirSync, readFileSync, realpathSync, rmSync, statSync, symlinkSync, writeFileSync } from 'node:fs';
import { availableParallelism, homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { UNIT_PROGS } from './wine/units.mjs';

const HERE = import.meta.dirname;
const ROOT = join(HERE, '..', '..');

/** The probe programs and the libraries each links. */
const PROBES = {
  'rt-probe': ['-lm'],
  'rt-wl': ['-lwayland-client', '-lffi'],
  'rt-joy': [],
  'rt-truncate': [],
  'rt-sockpass': [],
  'rt-repaint': [],
  'rt-deadclient': [],
  'rt-unix': [],
  'rt-x11': ['-lX11', '-lX11-xcb', '-lxcb-xfixes', '-lxcb', '-lXau', '-lXdmcp'],
  // static, serving what rt-side.so imports (SIDES)
  'rt-dlstatic': ['-pthread', ...['malloc', 'snprintf', 'strlen', 'fprintf', 'stderr', 'pthread_mutex_lock', 'pthread_mutex_unlock', 'sin', 'cos'].map((s) => `-Wl,--export=${s}`)],
};

/** Side modules (GEXCC_SHARED=1 -shared), made beside the image rather than
 *  in it: a test gives each as a lazy file. */
const SIDES = ['rt-side'];

/** The `win-media` image's files (wine/media): 3 s of a test picture and a
 *  440 Hz tone, H.264 and AAC in MP4, WMV2 and WMA2 in ASF. */
const SRC = ['-f', 'lavfi', '-i', 'testsrc2=size=320x240:rate=25:duration=3', '-f', 'lavfi', '-i', 'sine=frequency=440:sample_rate=44100:duration=3'];
const MEDIA = {
  'h264-aac.mp4': [...SRC, '-c:v', 'libx264', '-preset', 'veryfast', '-pix_fmt', 'yuv420p', '-profile:v', 'main', '-c:a', 'aac', '-b:a', '96k', '-shortest'],
  'wmv2-wma2.wmv': [...SRC, '-c:v', 'wmv2', '-b:v', '600k', '-c:a', 'wmav2', '-b:a', '96k', '-shortest'],
};

/** What each image leaves in the work directory: with `--prebuilt`, an
 *  image whose files are not there is missing. */
// The `wine64-units` image: Wine's DLLs that get a unit (where every
// program spends its calls), beside the test programs (wine/units.mjs).
const UNIT_DLLS = ['ntdll.dll', 'kernelbase.dll', 'kernel32.dll', 'msvcrt.dll', 'ucrtbase.dll'];

export const outputs = { busybox: ['bb.tar.zst', 'rt-side.so'], slim: ['slim'], package: ['pkg', 'pkg-no-m64'], 'gl-probes': ['gl-probes.json'], wine64: ['wine64.tar', 'wine64-lazy.json', 'wine64'], 'win-progs': ['win-progs.tar'], 'win-media': ['win-media.tar'], 'wine64-units': ['wine64-units'], kde: ['kde'], 'wine-site': ['wine-site'], mem64: ['mem64-pick.x86', 'mem64-miss.x86', 'mem64-retry.x86', 'no-m64'], 'mesa-cold': ['mesa-cold'], 'native-progs': ['mount-tmpfs.x86', 'mount-tmpfs.a64', 'truncate-path.x86', 'truncate-path.a64'] };

export const images = {
  // Mesa's cold part (page/mesa-cold): for each of the mt and st builds'
  // gex-wasm targets (GEX_TEST_MESA_BUILD, default target/web-build: its
  // mesa-mt and mesa-st, after ports/mesa/build.sh), mesa-cold/BUILD/whole
  // (the module unsplit), split (the build's own split) and miss (every
  // function kept but glReadPixels', ports/mesa/split.py --cold, left for a
  // miss), each beside its glue marked split or not; and
  // mesa-cold/gears.gexgltr, fixtures/gl's gles-gears trace.
  async 'mesa-cold'(work) {
    const top = process.env.GEX_TEST_MESA_BUILD || join(ROOT, 'target/web-build');
    const out = join(work, 'mesa-cold');
    rmSync(out, { recursive: true, force: true });
    for (const v of ['mt', 'st']) {
      const tgt = join(top, `mesa-${v}`, 'build/src/gallium/targets/gex-wasm');
      for (const f of ['gex-wgpu-replay.wasm.orig', 'gex-wgpu-replay.js', 'split/gex-wgpu-replay.wasm']) {
        if (!existsSync(join(tgt, f))) return `no ${f} in ${tgt} (ports/mesa/build.sh; GEX_TEST_MESA_BUILD)`;
      }
      const glue = readFileSync(join(tgt, 'gex-wgpu-replay.js'), 'utf8');
      const marked = (split) => glue.replace(/(gexMesaColdSplit *= *["'])GEX_MESA_COLD_[A-Z]*/, `$1GEX_MESA_COLD_${split ? 'YES' : 'NO'}`);
      const variant = (name, files, split) => {
        const d = join(out, v, name);
        mkdirSync(d, { recursive: true });
        for (const [to, from] of Object.entries(files)) copyFileSync(from, join(d, to));
        writeFileSync(join(d, 'gex-wgpu-replay.js'), marked(split));
      };
      const cold = join(tgt, 'split/gex-wgpu-replay.deferred.wasm');
      variant('whole', { 'gex-wgpu-replay.wasm': join(tgt, 'gex-wgpu-replay.wasm.orig') }, false);
      variant('split', { 'gex-wgpu-replay.wasm': join(tgt, 'split/gex-wgpu-replay.wasm'), ...(existsSync(cold) ? { 'gex-wgpu-replay.deferred.wasm': cold } : {}) }, existsSync(cold));
      const r = spawnSync('python3', ['-I', join(ROOT, 'ports/mesa/split.py'), join(tgt, 'gex-wgpu-replay.wasm.orig'), '-', join(out, v, 'miss'), '--cold', '[Rr]ead[Pp]ixels'], { encoding: 'utf8' });
      if (r.status !== 0) return `ports/mesa/split.py --cold: ${(r.stderr || r.stdout).trim().split('\n').slice(-3).join(' / ')}`;
      writeFileSync(join(out, v, 'miss', 'gex-wgpu-replay.js'), marked(true));
      writeFileSync(join(out, v, 'split.json'), JSON.stringify({ cold: existsSync(cold) }));
    }
    copyFileSync(join(HERE, 'gl/gles-gears-frames.gexgltr'), join(out, 'gears.gexgltr'));
    return null;
  },
  // A KDE site as debdemo's mksite makes it (GEX_TEST_KDE_SITE: kde.tar.zst,
  // lazy.json and its rest bundle), linked into the work directory as `kde`.
  async kde(work) {
    const site = process.env.GEX_TEST_KDE_SITE;
    if (!site) return 'set GEX_TEST_KDE_SITE to a KDE site (debdemo mksite)';
    if (!existsSync(join(site, 'kde.tar.zst')) || !existsSync(join(site, 'lazy.json'))) return `no kde.tar.zst and lazy.json in ${site}`;
    const to = join(work, 'kde');
    rmSync(to, { recursive: true, force: true });
    symlinkSync(realpathSync(site), to);
    return null;
  },
  // A Wine site as gex-eject makes it (GEX_TEST_WINE_SITE: its index.html,
  // project/build/web/nt.json and the files it names), linked into the work
  // directory as `wine-site`.
  async 'wine-site'(work) {
    const site = process.env.GEX_TEST_WINE_SITE;
    if (!site) return 'set GEX_TEST_WINE_SITE to an ejected Wine site (gex-eject)';
    if (!existsSync(join(site, 'project/build/web/nt.json'))) return `no project/build/web/nt.json in ${site}`;
    const to = join(work, 'wine-site');
    rmSync(to, { recursive: true, force: true });
    symlinkSync(realpathSync(site), to);
    return null;
  },
  // test/fixtures/programs/mem64-pick.c as an x86-64 program (static musl,
  // zig), for core/mem64: a system's memory64 process through the API.
  // And no-m64/: the runtime's files as gex's site lays them out (gex/...,
  // links to page/public/gex), less the memory64 shim, for core/mem64-missing
  // (and core/mem64-retry, which links it in later).
  async mem64(work) {
    const zig = process.env.ZIG || 'zig';
    for (const name of ['mem64-pick', 'mem64-miss', 'mem64-retry']) {
      const src = join(HERE, `programs/${name}.c`), out = join(work, `${name}.x86`);
      const key = createHash('sha256').update(readFileSync(src)).digest('hex'), keyFile = `${out}.key`;
      if (existsSync(out) && existsSync(keyFile) && readFileSync(keyFile, 'utf8') === key) continue;
      const r = spawnSync(zig, ['cc', '-target', 'x86_64-linux-musl', '-O2', '-static', '-o', out, src], { stdio: ['ignore', 'ignore', 'pipe'] });
      if (r.error) return `no ${zig} (ZIG)`;
      if (r.status !== 0) throw new Error(`${name}.c: ${r.stderr}`);
      writeFileSync(keyFile, key);
    }
    const pub = join(ROOT, 'page/public/gex'), site = join(work, 'no-m64');
    rmSync(site, { recursive: true, force: true });
    mkdirSync(join(site, 'gex'), { recursive: true });
    for (const f of readdirSync(pub)) {
      if (f !== 'gex_browser.x86.m64.shared.wasm') symlinkSync(join(pub, f), join(site, 'gex', f));
    }
    return null;
  },

  // test/fixtures/programs/mount-tmpfs.c and truncate-path.c as static musl
  // programs for each translated architecture (zig), for core/mount-tmpfs
  // and core/truncate-path: the native ABIs' own syscall numbers.
  async 'native-progs'(work) {
    const zig = process.env.ZIG || 'zig';
    for (const name of ['mount-tmpfs', 'truncate-path']) {
      const src = join(HERE, `programs/${name}.c`);
      const key = createHash('sha256').update(readFileSync(src)).digest('hex');
      for (const [arch, target] of [['x86', 'x86_64-linux-musl'], ['a64', 'aarch64-linux-musl']]) {
        const out = join(work, `${name}.${arch}`), keyFile = `${out}.key`;
        if (existsSync(out) && existsSync(keyFile) && readFileSync(keyFile, 'utf8') === key) continue;
        const r = spawnSync(zig, ['cc', '-target', target, '-O2', '-static', '-o', out, src], { stdio: ['ignore', 'ignore', 'pipe'] });
        if (r.error) return `no ${zig} (ZIG)`;
        if (r.status !== 0) throw new Error(`${name}.c: ${r.stderr}`);
        writeFileSync(keyFile, key);
      }
    }
    return null;
  },

  async busybox(work) {
    const bb = process.env.BUSYBOX;
    const applets = readFileSync(join(ROOT, 'toolchain/busybox/busybox.applets'), 'utf8').split('\n').filter(Boolean);
    const h = createHash('sha256');
    if (bb && existsSync(bb)) h.update(readFileSync(bb));
    h.update(applets.join('\n'));
    for (const name of [...Object.keys(PROBES), ...SIDES]) h.update(readFileSync(join(HERE, 'probe', `${name}.c`)));
    // rt-truncate.c is programs/truncate-path.c.
    h.update(readFileSync(join(HERE, 'programs/truncate-path.c')));
    // rt-sockpass.c is native/wasm32/sockpass.c.
    h.update(readFileSync(join(HERE, 'native/wasm32/sockpass.c')));
    for (const n of ['repaint', 'deadclient']) h.update(readFileSync(join(HERE, `programs/${n}.c`)));
    h.update(JSON.stringify(PROBES));
    const key = h.digest('hex');
    const keyFile = join(work, 'bb.key');
    writeFileSync(join(work, 'lazy.bin'), Buffer.alloc(3 << 20, 7));
    // The same for core/runprogram, which reads it, apart from core/dir-times's
    // lazy.bin while the two may run at once.
    writeFileSync(join(work, 'lazy-program.bin'), Buffer.alloc(3 << 20, 7));
    const have = existsSync(join(work, 'bb.tar.zst')) && existsSync(keyFile) && readFileSync(keyFile, 'utf8');
    // Without busybox here, what a run before made (a copy where there is no
    // gexcc); else only if made from the same inputs.
    if (have && (!bb || have === key) && SIDES.every((n) => existsSync(join(work, `${n}.so`)))) return null;
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
    for (const name of SIDES) {
      execFileSync(cc, ['-O2', '-Wall', '-fPIC', '-shared', '-o', join(work, `${name}.so`), join(HERE, 'probe', `${name}.c`)],
        { stdio: ['ignore', 'ignore', 'inherit'], env: { ...process.env, GEXCC_SHARED: '1' } });
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
    const src = Object.fromEntries(readdirSync(probes).filter((f) => /^gl4\d-.*-probe\.c$/.test(f)).map((f) => [f.slice(0, -2), join(probes, f)]));
    const names = Object.keys(src).sort();
    const version = join(ROOT, 'test/fixtures/programs/gl-version.c');
    const musl = process.env.GEX_TOOLCHAIN || join(homedir(), '.cache/gex/toolchains/aarch64-linux-musl-cross');
    const h = createHash('sha256');
    for (const f of readdirSync(join(ROOT, 'guest/gl')).sort()) {
      const p = join(ROOT, 'guest/gl', f);
      if (statSync(p).isFile()) h.update(f).update(readFileSync(p));
    }
    h.update(readFileSync(join(ROOT, 'crates/gex-gl/src/proto.rs')));
    for (const n of names) h.update(n).update(readFileSync(src[n]));
    h.update(readFileSync(version));
    const gears = join(probes, 'gles-gears.c');
    h.update(readFileSync(gears));
    const window = join(probes, 'gles-window.c');
    h.update(readFileSync(window));
    const windowXcb = join(probes, 'gles-window-xcb.c');
    h.update(readFileSync(windowXcb));
    const windowFboXcb = join(probes, 'gles-window-fbo-xcb.c');
    h.update(readFileSync(windowFboXcb));
    const windowXcbUnder = join(probes, 'gles-window-xcb-under.c');
    h.update(readFileSync(windowXcbUnder));
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
      const ref = join(dir, 'native', n);
      let line = null;
      try {
        run('cc', ['-O2', `-I${reg}`, '-o', ref, src[n], '-lEGL', '-lGL', '-lm']);
        const out = execFileSync(ref, { encoding: 'utf8', timeout: 60000, env: { ...process.env, EGL_PLATFORM: 'surfaceless', LIBGL_ALWAYS_SOFTWARE: '1', GEX_NATIVE_REFERENCE: '1' }, stdio: ['ignore', 'pipe', 'pipe'] });
        line = out.split('\n').filter((l) => l.trim()).pop() ?? null;
      } catch {
        line = null;
      }
      // The GL the probe needs: its highest `#version` or the context it asks for.
      const text = readFileSync(src[n], 'utf8');
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
      // x86's has the host's Xlib too, for gles-window (an X client).
      x86: { cc: 'gcc', lib: 'usr/lib/x86_64-linux-gnu', x11: ['libX11.so.6', 'libxcb.so.1', 'libXau.so.6', 'libXdmcp.so.6', 'libbsd.so.0', 'libmd.so.0'].map((f) => `/lib/x86_64-linux-gnu/${f}`), runtime: [['/lib64/ld-linux-x86-64.so.2', 'lib64/ld-linux-x86-64.so.2'], ['/lib/x86_64-linux-gnu/libc.so.6', 'usr/lib/x86_64-linux-gnu/libc.so.6'], ['/lib/x86_64-linux-gnu/libm.so.6', 'usr/lib/x86_64-linux-gnu/libm.so.6']] },
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
      for (const n of names) run(x.cc, ['-O2', `-I${reg}`, `-I${inc}`, '-o', join(tree, 'probes', n), src[n], ...link]);
      run(x.cc, ['-O2', `-I${inc}`, '-o', join(tree, 'probes/gl-version'), version, ...link]);
      run(x.cc, ['-O2', `-I${reg}`, `-I${inc}`, '-o', join(tree, 'probes/gles-gears'), gears, `-L${gl}`, '-l:libEGL.so.1', '-l:libGLESv2.so.2', '-lm']);
      if (x.x11?.every((f) => existsSync(f))) {
        for (const f of x.x11) copyFileSync(realpathSync(f), join(tree, x.lib, f.replace(/^.*\//, '')));
        mkdirSync(join(tree, 'tmp'), { recursive: true });
        run(x.cc, ['-O2', `-I${reg}`, `-I${inc}`, '-o', join(tree, 'probes/gles-window'), window, `-L${gl}`, '-l:libEGL.so.1', '-l:libGLESv2.so.2', '-ldl']);
        run(x.cc, ['-O2', `-I${reg}`, `-I${inc}`, '-o', join(tree, 'probes/gles-window-xcb'), windowXcb, `-L${gl}`, '-l:libEGL.so.1', '-l:libGLESv2.so.2', '-ldl']);
        run(x.cc, ['-O2', `-I${reg}`, `-I${inc}`, '-o', join(tree, 'probes/gles-window-fbo-xcb'), windowFboXcb, `-L${gl}`, '-l:libEGL.so.1', '-l:libGLESv2.so.2', '-ldl']);
        run(x.cc, ['-O2', `-I${reg}`, `-I${inc}`, '-o', join(tree, 'probes/gles-window-xcb-under'), windowXcbUnder, `-L${gl}`, '-l:libEGL.so.1', '-l:libGLESv2.so.2', '-ldl']);
      }
      execFileSync('tar', ['-cf', join(work, `gl-probes-${a}.tar`), '-C', tree, '.']);
    }
    writeFileSync(join(work, 'gl-probes.json'), JSON.stringify(expect, null, 1));
    writeFileSync(keyFile, key);
    return null;
  },

  async wine64(work) {
    const dist = process.env.GEX_TEST_WINE64_DIST;
    const pfx = process.env.GEX_TEST_WINE64_PREFIX;
    if (!dist || !pfx) return 'set GEX_TEST_WINE64_DIST and GEX_TEST_WINE64_PREFIX (wine/dist.sh)';
    if (!existsSync(join(dist, 'lib/wine/x86_64-unix/wine'))) return `no Wine loader in ${dist}`;
    const h = createHash('sha256').update(realpathSync(dist)).update(realpathSync(pfx));
    for (const f of [join(dist, 'lib/wine/x86_64-unix/wine'), join(dist, 'lib/wine/x86_64-unix/ntdll.so'), join(pfx, 'system.reg'), join(ROOT, 'wine/page-prefix.sh')]) {
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
    // links resolved (wine/page-prefix.sh).
    execFileSync('bash', [join(ROOT, 'wine/page-prefix.sh'), pfx, dist, join(dir, 'pfx')], { stdio: ['ignore', 'ignore', 'pipe'] });
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
    const builds = [];
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
        // `gex-nt-shared`: a DLL. `gex-nt-variant: NAME FLAGS...` (one a
        // line): built once per variant, as NAME with those flags.
        const dll = /gex-nt-shared/.test(text);
        const variants = [...text.matchAll(/gex-nt-variant: *(\S+)(.*)/g)].map((m) => [m[1], m[2].trim().split(/\s+/).filter(Boolean)]);
        // `gex-nt-msvc: DEF...`: the MSVC ABI, built with clang-cl and linked
        // with lld-link against import libraries made from those .def files
        // (x64), no MSVC headers or libraries.
        const msvc = /gex-nt-msvc: *(.*)/.exec(text)?.[1].trim().split(/\s+/);
        if (msvc) {
          try {
            const tmp = join(work, 'win-progs-msvc');
            mkdirSync(tmp, { recursive: true });
            const llvm = (t) => (existsSync(`/usr/lib/llvm-22/bin/${t}`) ? `/usr/lib/llvm-22/bin/${t}` : t);
            const libs = msvc.map((d) => {
              const lib = join(tmp, d.replace(/(-x64)?\.def$/, '.lib'));
              execFileSync(llvm('llvm-dlltool'), ['-m', 'i386:x86-64', '-d', join(src, d), '-l', lib], { stdio: ['ignore', 'ignore', 'pipe'] });
              return lib;
            });
            const obj = join(tmp, 'main.obj');
            execFileSync(llvm('clang-cl'), ['--target=x86_64-pc-windows-msvc', '/O1', '/GS-', '/EHa', '/c', join(src, f), `/Fo${obj}`], { stdio: ['ignore', 'ignore', 'pipe'] });
            execFileSync(llvm('lld-link'), ['/nodefaultlib', '/entry:entry', '/subsystem:console', '/dynamicbase', '/Brepro', obj, ...libs, `/out:${join(dir, arch, f.replace(/\.[a-z]+$/, '.exe'))}`], { stdio: ['ignore', 'ignore', 'pipe'] });
          } catch (e) {
            failed[`${arch}/${f}`] = String(e.stderr ?? e.message).split('\n').find((l) => /error/.test(l)) ?? String(e.message).split('\n')[0];
          }
          rmSync(join(work, 'win-progs-msvc'), { recursive: true, force: true });
          continue;
        }
        for (const [name, flags] of variants.length ? variants : [[f.replace(/\.[a-z]+$/, ''), []]])
          builds.push([cc[0], [...cc.slice(1), '-O2', '-s', '-Wl,--no-insert-timestamp', ...(dll ? ['-shared'] : []), ...flags, '-o', join(dir, arch, `${name}.${dll ? 'dll' : 'exe'}`), join(src, f), ...libs],
            `${arch}/${variants.length ? `${name}.c` : f}`]);
      }
    }
    // The compiles side by side, half the host's CPUs at a time.
    const run1 = ([cmd, args, key]) => new Promise((resolve) => {
      execFile(cmd, args, { maxBuffer: 1 << 26 }, (e, _out, err) => {
        if (e) failed[key] = String(err || e.message).split('\n').find((l) => /error/.test(l)) ?? String(e.message).split('\n')[0];
        resolve();
      });
    });
    const lanes = Math.max(1, availableParallelism() >> 1);
    await Promise.all(Array.from({ length: lanes }, async () => { while (builds.length) await run1(builds.shift()); }));
    writeFileSync(join(work, 'win-progs.json'), JSON.stringify({ failed }, null, 1));
    execFileSync('tar', ['-cf', join(work, 'win-progs.tar'), '-C', dir, '.']);
    writeFileSync(keyFile, key);
    return null;
  },

  async 'win-media'(work) {
    // Made with the host's FFmpeg (libx264), each the same bytes for the same
    // FFmpeg: no timestamps, one thread.
    const h = createHash('sha256').update(JSON.stringify(MEDIA));
    const v = spawnSync('ffmpeg', ['-hide_banner', '-version'], { encoding: 'utf8' });
    if (v.error || v.status !== 0) return 'no ffmpeg (the media for wine/media)';
    h.update(v.stdout.split('\n')[0]);
    const key = h.digest('hex');
    const keyFile = join(work, 'win-media.key');
    if (existsSync(join(work, 'win-media.tar')) && existsSync(keyFile) && readFileSync(keyFile, 'utf8') === key) return null;
    const dir = join(work, 'win-media');
    rmSync(dir, { recursive: true, force: true });
    mkdirSync(dir, { recursive: true });
    for (const [name, args] of Object.entries(MEDIA)) {
      const r = spawnSync('ffmpeg', ['-hide_banner', '-loglevel', 'error', '-y', '-threads', '1', ...args, '-fflags', '+bitexact', '-flags', '+bitexact', join(dir, name)], { encoding: 'utf8' });
      if (r.status !== 0) return `ffmpeg could not make ${name}: ${(r.stderr ?? '').trim().split('\n').pop()}`;
    }
    execFileSync('tar', ['-cf', join(work, 'win-media.tar'), '-C', dir, '.']);
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
    // Each with the switches its line names (the process's flavour: a
    // compact-space process's units are --wine64-compact ones).
    for (const m of said.matchAll(/gex-pe: image (\S+) at base (0x[0-9a-f]+): no function-tier unit.*--pe-base 0x[0-9a-f]+((?: --[a-z0-9-]+(?: \d+)?)*)$/gm)) {
      const [, path, base, flags] = m;
      const name = path.split('/').pop().toLowerCase();
      const host = path.startsWith('/wine64/') && UNIT_DLLS.includes(name) ? join(realpathSync(process.env.GEX_TEST_WINE64_DIST), path.slice(8))
        : UNIT_PROGS.includes(name) ? join(progs, 'x64', name) : null;
      if (host) builds.add(JSON.stringify([host, '--pe-base', base, ...flags.split(' ').filter(Boolean), '--jobs', '8']));
    }
    const tramp = /gex-pe: missing unit: \[the trampolines\].* gexc cgen-image (--nt-trampolines64(?: --[a-z0-9-]+(?: \d+)?)*)$/m.exec(said);
    if (tramp) builds.add(JSON.stringify(tramp[1].split(' ')));
    // Wine's core DLLs only where the dist translates them: a native PE
    // core (wine/dist.sh's default) runs them as wasm and names none.
    if (!UNIT_PROGS.every((e) => [...builds].some((b) => b.includes(`/${e}"`)))) return `the run named no base for each of ${UNIT_PROGS.join(', ')}`;
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

  // `single`: the runner's --single, whose pages have no SharedArrayBuffer:
  // @gexemu/runtime-single.
  async package(work, { single = false } = {}) {
    execFileSync(join(ROOT, 'page/runtime/build.sh'), [...(single ? ['--single'] : []), join(work, 'pkg')], { stdio: ['ignore', 'ignore', 'inherit'] });
    // A copy without the memory64 shim, for core/mem64-missing.
    const less = join(work, 'pkg-no-m64');
    rmSync(less, { recursive: true, force: true });
    execFileSync('cp', ['-al', join(work, 'pkg'), less]);
    const m64 = readdirSync(less).filter((f) => f.startsWith('gex_browser.x86.m64.shared'));
    if (m64.length !== 1) throw new Error(`pkg: ${m64.length} memory64 shims`);
    rmSync(join(less, m64[0]));
    return null;
  },
};
