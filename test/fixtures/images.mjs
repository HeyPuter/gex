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
//   gl-probes  gl-probes.tar: the x86-64 guest GL (guest/gl/build.sh with gcc)
//            and each self-checking probe in guest/gl/probes built against it
//            (/probes/NAME), with the host's loader and libc; and
//            gl-probes.json, each probe's line from a run on the host's own
//            Mesa (software, surfaceless), which the guest's run must print.
//            Needs gcc, the host's EGL and GL, and third_party/opengl-registry.

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, linkSync, mkdirSync, readdirSync, readFileSync, realpathSync, rmSync, statSync, symlinkSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const HERE = import.meta.dirname;
const ROOT = join(HERE, '..', '..');

/** The probe programs and the libraries each links. */
const PROBES = {
  'rt-probe': ['-lm'],
  'rt-wl': ['-lwayland-client', '-lffi'],
  'rt-joy': [],
  'rt-x11': ['-lX11', '-lX11-xcb', '-lxcb-xfixes', '-lxcb', '-lXau', '-lXdmcp'],
};

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
    const h = createHash('sha256');
    for (const f of readdirSync(join(ROOT, 'guest/gl')).sort()) {
      const p = join(ROOT, 'guest/gl', f);
      if (statSync(p).isFile()) h.update(f).update(readFileSync(p));
    }
    h.update(readFileSync(join(ROOT, 'crates/gex-gl/src/proto.rs')));
    for (const n of names) h.update(n).update(readFileSync(join(probes, `${n}.c`)));
    const version = join(ROOT, 'test/fixtures/programs/gl-version.c');
    h.update(readFileSync(version));
    const key = h.digest('hex');
    const keyFile = join(work, 'gl-probes.key');
    if (existsSync(join(work, 'gl-probes.tar')) && existsSync(keyFile) && readFileSync(keyFile, 'utf8') === key) return null;
    const dir = join(work, 'gl-probes');
    rmSync(dir, { recursive: true, force: true });
    const lib = join(dir, 'tree/usr/lib/x86_64-linux-gnu');
    mkdirSync(lib, { recursive: true });
    mkdirSync(join(dir, 'tree/probes'), { recursive: true });
    mkdirSync(join(dir, 'tree/lib64'), { recursive: true });
    mkdirSync(join(dir, 'native'), { recursive: true });
    const gl = join(dir, 'gl64');
    execFileSync('sh', [join(ROOT, 'guest/gl/build.sh'), gl], { cwd: ROOT, env: { ...process.env, GEX_CC: 'gcc' }, stdio: ['ignore', 'ignore', 'pipe'] });
    for (const f of ['libGLESv2.so.2', 'libEGL.so.1', 'libGL.so.1']) copyFileSync(join(gl, f), join(lib, f));
    copyFileSync(realpathSync('/lib64/ld-linux-x86-64.so.2'), join(dir, 'tree/lib64/ld-linux-x86-64.so.2'));
    copyFileSync(realpathSync('/lib/x86_64-linux-gnu/libc.so.6'), join(lib, 'libc.so.6'));
    const expect = {};
    for (const n of names) {
      const src = join(probes, `${n}.c`);
      // The reference: the host's Mesa, in software, with no display.
      const ref = join(dir, 'native', n);
      let line = null;
      try {
        execFileSync('cc', ['-O2', `-I${reg}`, '-o', ref, src, '-lEGL', '-lGL', '-lm'], { stdio: ['ignore', 'ignore', 'pipe'] });
        const out = execFileSync(ref, { encoding: 'utf8', timeout: 60000, env: { ...process.env, EGL_PLATFORM: 'surfaceless', LIBGL_ALWAYS_SOFTWARE: '1', GEX_NATIVE_REFERENCE: '1' }, stdio: ['ignore', 'pipe', 'pipe'] });
        line = out.split('\n').filter((l) => l.trim()).pop() ?? null;
      } catch (e) {
        line = null;
      }
      // The GL the probe needs: its highest `#version` or the context it asks for.
      const text = readFileSync(src, 'utf8');
      const glsl = [...text.matchAll(/#version (\d+)(?! es)/g)].map((m) => Number(m[1]));
      const ctx = text.match(/EGL_CONTEXT_MAJOR_VERSION, *(\d+)[^]*?EGL_CONTEXT_MINOR_VERSION, *(\d+)/);
      const needs = Math.max(0, ...glsl, ctx ? Number(ctx[1]) * 100 + Number(ctx[2]) * 10 : 0);
      expect[n] = { want: line, needs };
      execFileSync('cc', ['-O2', `-I${reg}`, '-o', join(dir, 'tree/probes', n), src, `-L${gl}`, '-l:libEGL.so.1', '-l:libGL.so.1', '-lm'], { stdio: ['ignore', 'ignore', 'pipe'] });
    }
    execFileSync('cc', ['-O2', `-I${reg}`, '-o', join(dir, 'tree/probes/gl-version'), version, `-L${gl}`, '-l:libEGL.so.1', '-l:libGL.so.1'], { stdio: ['ignore', 'ignore', 'pipe'] });
    writeFileSync(join(work, 'gl-probes.json'), JSON.stringify(expect, null, 1));
    execFileSync('tar', ['-cf', join(work, 'gl-probes.tar'), '-C', join(dir, 'tree'), '.']);
    writeFileSync(keyFile, key);
    return null;
  },

  async package(work) {
    execFileSync(join(ROOT, 'web/runtime/build.sh'), [join(work, 'pkg')], { stdio: ['ignore', 'ignore', 'inherit'] });
    return null;
  },
};
