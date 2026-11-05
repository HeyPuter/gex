// The guest GL library's names are one library (guest/gl/build.sh), so a
// program that loads more than one of them has one GL state
// (fixtures/programs/gl-one-library-probe.c), under gexc with no GL behind it.
import { tail } from '../lib/native.mjs';
import { hostGl } from './hostgl.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 300 };

export default async function (t) {
  const h = await hostGl(t);
  const probe = await h.probe('gl-one-library-probe', ['-ldl']);
  const g = await h.gex(probe, { gl: false });
  t.check('SUMMARY failed=0', /SUMMARY failed=0/.test(g.stdout), `exit ${g.code}: ${tail(g.stdout)} ${tail(g.stderr)}`);
  h.n.clean();
}
