// An i386 program whose file is mostly a payload after its image (an
// installer's shape: 120 MiB here), on the page: it runs, and the shim reads
// none of the payload into its own heap, which never gives a span back (the
// code scans read whole only ELF objects; a PE image is not one).
import { join } from 'node:path';
import { readFileSync, writeFileSync } from 'node:fs';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

const SRC = new URL('../fixtures/wine/src/', import.meta.url).pathname;
const PAYLOAD = 120 << 20;

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('bigimage', { src: join(SRC, 'bigimage.c') });
  const img = readFileSync(exe);
  const big = new Uint8Array(img.length + PAYLOAD);
  big.set(img);
  for (let i = img.length; i < big.length; i += 4096) big[i] = i & 0xff;
  const file = `${exe}.big`;
  writeFileSync(file, big);
  const p = await w.page(['C:\\bigimage.exe'], { pfxFiles: { 'bigimage.exe': file }, prefix: w.pagePrefix(), cwd: '/pfx/drive_c', name: 'p-bigimage', timeout: 600000 });
  t.check('it ran', /^bigimage ok$/m.test(p.out), w.detail(p));
  const took = p.out.match(/a single (\d+) MiB allocation takes a span of its own[^\n]*/g) ?? [];
  t.check('no payload-sized span of the shim heap', !took.some((l) => Number(l.match(/single (\d+)/)[1]) >= 64), took.join(' / '));
  w.clean();
}
