// gexc's own Wayland compositor end to end (fixtures/integration/wlclient.c):
// the client gets its frame callback and the compositor's frame
// (--wayland-out) has its green border, blue field and red diagonal. Then the
// GPU compositor (--wayland-gpu), which with the screen the window's size has
// nowhere to put the surface but the origin, so must give the CPU one's frame
// byte for byte; it must say which adapter it got (with none it falls back to
// the CPU and the comparison is vacuous: a skip).
import { existsSync, readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { qemu } from '../lib/qemu.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 300 };

export default async function (t) {
  const q = qemu(t);
  const client = await q.build('wlclient', 'wlclient.c', ['-static', '-O2']);
  const run = async (label, more) => {
    const out = join(q.dir, label);
    rmSync(`${out}-000.ppm`, { force: true });
    const r = await q.n.gexc(['run', ...more, '--wayland', '/run/user/0/wayland-0', '--wayland-size', '64x48', '--wayland-out', out, client], { cwd: q.dir });
    return { ...r, frame: `${out}-000.ppm` };
  };
  const cpu = await run('wayland', []);
  t.eq('wayland: exit status', cpu.code, 0, cpu.stderr.trim().split('\n').slice(0, 3).join(' / '));
  t.check('wayland: frame callback', /^frame callback yes$/m.test(cpu.stdout), cpu.stdout.split('\n').slice(0, 10).join(' / '));
  if (!t.check('wayland: a frame', existsSync(cpu.frame))) return;
  const ppm = readFileSync(cpu.frame);
  const px = (at) => [...ppm.subarray(at, at + 3)].join(' ');
  t.eq('wayland: border, field, diagonal', [103, 3943, 4717].map(px).join('|'), '0 200 0|0 0 200|200 0 0');

  const gpu = await run('wayland-gpu', ['--verbose', '--wayland-gpu']);
  t.eq('wayland-gpu: exit status', gpu.code, 0, gpu.stderr.trim().split('\n').slice(0, 3).join(' / '));
  const adapter = /compositing on (.*)/.exec(gpu.stderr)?.[1];
  if (!adapter) t.skip(`wayland-gpu: no graphics adapter (${/.*no GPU compositor.*/.exec(gpu.stderr)?.[0] ?? 'no note'})`);
  t.check(`wayland-gpu: the GPU's frame is the CPU's (${adapter})`, existsSync(gpu.frame) && readFileSync(gpu.frame).equals(ppm));
}
