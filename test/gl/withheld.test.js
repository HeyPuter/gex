// Extensions the guest GL withholds although it exports their commands
// (WITHHELD in scripts/gl/gen_gl_extensions.py): the host may list them,
// the guest must not. GL_AMD_pinned_memory makes a buffer's storage the
// application's memory, which the host side of the wire never sees.
// fixtures/programs/gl-extensions-probe.c asks by glGetStringi and
// glGetString(GL_EXTENSIONS), on the host's Mesa and under gexc --gl.
// GL_ARB_buffer_storage is the control: listed by the host, it must still
// be listed by the guest. A host that does not list the withheld one makes
// the check weaker, and says so.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { ROOT, tail } from '../lib/native.mjs';
import { hostGl } from './hostgl.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

const WITHHELD = ['GL_AMD_pinned_memory'];
const CONTROL = 'GL_ARB_buffer_storage';

export default async function (t) {
  const table = readFileSync(join(ROOT, 'guest/gl/gl-extensions-generated.inc'), 'utf8');
  for (const e of WITHHELD) t.check(`${e} is not in guest/gl/gl-extensions-generated.inc`, !table.includes(`"${e}"`));
  const h = await hostGl(t);
  const probe = await h.probe('gl-extensions-probe', ['-lEGL', '-lGL']);
  const n = await h.native(probe, [...WITHHELD, CONTROL]);
  if (!t.check('the probe found a native GL context', n.code === 0, tail(n.stdout + n.stderr))) return;
  const g = await h.gex(probe, { args: [...WITHHELD, CONTROL] });
  if (!t.check('the probe runs under gexc', g.code === 0, tail(g.stderr.split('\n').filter((l) => !l.startsWith('[gex]')).join('\n')))) return;
  const line = (out, e) => out.split('\n').find((l) => l.startsWith(`EXT ${e} `)) ?? '';
  for (const e of WITHHELD) {
    const host = /stringi=1/.test(line(n.stdout, e));
    t.check(`${e} not listed by the guest`, /stringi=0 string=[0n]/.test(line(g.stdout, e)), line(g.stdout, e));
    if (!host) t.say(`${e}: the host does not list it either (a weaker check)`);
  }
  if (/stringi=1/.test(line(n.stdout, CONTROL))) t.check(`${CONTROL} still listed by the guest`, /stringi=1 string=[1n]/.test(line(g.stdout, CONTROL)), line(g.stdout, CONTROL));
  else t.say(`${CONTROL}: the host does not list it (no control)`);
  h.n.clean();
}
