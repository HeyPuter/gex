// A probe built against the host's GL (host cc, -lEGL -lGL), run on the
// host's Mesa in software with no display, and under gexc against the guest
// GL library (guest/gl/build.sh, built once per work directory): the native
// run is the reference, the guest's answers are checked against it. x86 only
// (the host's cc makes x86-64 programs).
//
//   const h = await hostGl(t);
//   const probe = await h.probe('gl-map-buffer-probe', ['-lEGL', '-lGL']);
//   const n = await h.native(probe);
//   const g = await h.gex(probe, { env: { GEXGL_PROBE_VMDATA: '1' } });
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { exec, native, ROOT } from '../lib/native.mjs';

const PROGRAMS = join(ROOT, 'test/fixtures/programs');

/** The guest GL library in the work directory, built when its sources changed. */
async function guestGl(t, n) {
  if (!existsSync(join(ROOT, 'third_party/opengl-registry/api/GL/glcorearb.h'))) t.skip('third_party/opengl-registry is not checked out');
  const h = createHash('sha256');
  for (const d of ['guest/gl', 'scripts/gl'])
    for (const f of readdirSync(join(ROOT, d)).sort()) {
      const p = join(ROOT, d, f);
      if (statSync(p).isFile()) h.update(`${d}/${f}`).update(readFileSync(p));
    }
  h.update(readFileSync(join(ROOT, 'crates/gex-gl/src/proto.rs')));
  const out = join(t.workDir, 'host-gl', h.digest('hex').slice(0, 16));
  if (existsSync(join(out, '.done'))) return out;
  const tmp = `${out}.tmp-${process.pid}`;
  rmSync(tmp, { recursive: true, force: true });
  mkdirSync(join(out, '..'), { recursive: true });
  const r = await exec(['sh', join(ROOT, 'guest/gl/build.sh'), tmp], { env: { ...n.env, GEX_CC: n.env.GEX_CC || 'cc' }, timeout: 600000 });
  if (r.code !== 0) throw new Error(`guest/gl/build.sh: ${r.stderr.trim().split('\n').slice(-5).join(' / ')}`);
  writeFileSync(join(tmp, '.done'), '');
  try {
    renameSync(tmp, out);
  } catch {
    // another test made it first
    rmSync(tmp, { recursive: true, force: true });
  }
  return out;
}

export async function hostGl(t) {
  const n = native(t);
  const gl = await guestGl(t, n);
  const dir = n.dir('gl');
  return {
    n,
    dir,
    gl,
    /** fixtures/programs/NAME.c built with the host's cc and `libs`. */
    async probe(name, libs) {
      const exe = join(dir, name);
      await n.hostcc(['-O2', '-Wall', '-o', exe, join(PROGRAMS, `${name}.c`), ...libs]);
      return exe;
    },
    /** The probe on the host's Mesa (software, no display). */
    native: (exe, args = []) => exec([exe, ...args], { env: { ...n.env, EGL_PLATFORM: 'surfaceless', LIBGL_ALWAYS_SOFTWARE: '1' }, timeout: 300000 }),
    /** The probe under gexc against the guest GL (`gl`: with --gl, the host's GL behind it), `env` the guest's. */
    gex: (exe, { args = [], env = {}, gl: withGl = true } = {}) =>
      n.gexc(['run', ...(withGl ? ['--gl'] : []), ...Object.entries({ LD_LIBRARY_PATH: gl, ...env }).flatMap(([k, v]) => ['--env', `${k}=${v}`]), exe, ...args], { timeout: 600000 }),
  };
}
