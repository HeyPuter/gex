// /proc/self/mountinfo and /proc/mounts are the process's mount table in
// Linux's format, with statx's mount ids (what `mountpoint`, findmnt and
// libmount read: openjdk's postinst asks `mountpoint /proc`). Natively.
import { mkdirSync } from 'node:fs';
import { native, trim, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 300 };

export default async function (t) {
  const n = native(t);
  const d = n.dir('mountinfo');
  mkdirSync(`${d}/root/proc`, { recursive: true });
  await n.cc(['-O1', '-static', '-o', `${d}/root/mountinfo`, n.src('mountinfo/mountinfo.c')]);
  const r = await n.gexc(['run', '--sysroot', `${d}/root`, `${d}/root/mountinfo`]);
  t.eq('mountinfo, statx and mounts', trim(r.stdout + r.stderr), [
    'mountinfo: root listed, proc listed, proc under root yes',
    'statx: root matches, proc matches, a file in proc matches',
    'mounts: proc listed'].join('\n'));
  t.check('exits 0', r.code === 0, `exit ${r.code}: ${tail(r.stderr)}`);
  n.clean();
}
