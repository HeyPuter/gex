// The matrices and enables the guest GL answers itself
// (guest/gl/gexgl-shadow.inc), against the host's Mesa's answers to the same
// commands (fixtures/programs/gl-matrix-shadow-probe.c): under gexc --gl
// with the shadow on, checked against the host (GEXGL_SHADOW_CHECK=1, which
// must compare some answers and find none different), and off.
import { tail } from '../lib/native.mjs';
import { hostGl } from './hostgl.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

const MODES = { on: { GEXGL_SHADOW: 'on' }, check: { GEXGL_SHADOW_CHECK: '1' }, off: { GEXGL_NO_QUERY_CACHE: '1' } };

export default async function (t) {
  const h = await hostGl(t);
  const probe = await h.probe('gl-matrix-shadow-probe', ['-lEGL', '-lGL', '-lm']);
  const n = await h.native(probe);
  if (!t.check('the probe finishes on the host\'s Mesa', /^DONE/m.test(n.stdout), tail(n.stdout + n.stderr))) return;
  for (const [mode, env] of Object.entries(MODES)) {
    const g = await h.gex(probe, { env });
    const a = n.stdout.split('\n'), b = g.stdout.split('\n');
    const at = a.findIndex((l, i) => l !== b[i]);
    t.check(`${mode}: the host's answers`, g.stdout === n.stdout, at < 0 ? `exit ${g.code}` : `line ${at + 1}: host ${a[at]}, guest ${b[at]}; ${tail(g.stderr)}`);
    t.check(`${mode}: no shadow answer differs`, !/shadow .* differs/.test(g.stderr), g.stderr.split('\n').filter((l) => l.includes('shadow')).slice(0, 4).join(' / '));
    if (mode === 'check') t.check('check: answers compared, 0 differed', /shadow: [1-9][0-9]* answers compared, 0 differed/.test(g.stderr), g.stderr.split('\n').filter((l) => l.includes('shadow')).slice(0, 4).join(' / '));
  }
  h.n.clean();
}
