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

  async package(work) {
    execFileSync(join(ROOT, 'web/runtime/build.sh'), [join(work, 'pkg')], { stdio: ['ignore', 'ignore', 'inherit'] });
    return null;
  },
};
