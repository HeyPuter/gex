// glMapBufferRange's contents rules (fixtures/programs/gl-map-buffer-probe.c),
// natively: the probe passes on the host's Mesa first, then under gexc --gl
// against the guest GL; the 64 MiB ASTC upload, tried twice, is reported
// once ("gexgl: dropped opcode").
import { tail } from '../lib/native.mjs';
import { hostGl } from './hostgl.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

const WANT = 'SUMMARY cases=15 passed=15 failed=0';

export default async function (t) {
  const h = await hostGl(t);
  const probe = await h.probe('gl-map-buffer-probe', ['-lEGL', '-lGL']);
  const n = await h.native(probe);
  if (!n.stdout.includes(WANT)) return t.check('the probe passes on the host\'s Mesa', false, tail(n.stdout + n.stderr));
  const g = await h.gex(probe, { env: { GEXGL_PROBE_VMDATA: '1' } });
  t.check(`gexc --gl: ${WANT}`, g.stdout.includes(WANT), `exit ${g.code}: ${tail(g.stdout)} ${tail(g.stderr)}`);
  const dropped = (g.stderr.match(/gexgl: dropped opcode/g) ?? []).length;
  t.eq('one "dropped opcode" line', dropped, 1, g.stderr.split('\n').filter((l) => l.includes('gexgl:')).slice(0, 6).join(' / '));
  h.n.clean();
}
