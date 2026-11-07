// An i386 program whose file is mostly an installer's payload, on the page:
// after its image (120 MiB here), and inside it, as a resource section of
// 120 MiB ahead of its base relocations (Chrome's installer carries its
// payload that way). Each runs, and the shim reads none of the payload into
// its own heap, which never gives a span back.
import { join } from 'node:path';
import { readFileSync, writeFileSync } from 'node:fs';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1200 };

const SRC = new URL('../fixtures/wine/src/', import.meta.url).pathname;
const PAYLOAD = 120 << 20;

/** `img` with a resource section of `size` bytes where its .reloc was, the
 *  sections from there on (its relocations, MinGW's debug sections) moved
 *  after it: an empty resource directory, then bytes. */
function withResources(img, size) {
  const v = new DataView(img.buffer, img.byteOffset, img.byteLength);
  const coff = v.getUint32(0x3c, true) + 4;
  const nsec = v.getUint16(coff + 2, true);
  const opt = coff + 20;
  const table = opt + v.getUint16(coff + 16, true);
  const [sa, fa] = [v.getUint32(opt + 32, true), v.getUint32(opt + 36, true)];
  const up = (x, a) => Math.ceil(x / a) * a;
  const name = (i) => String.fromCharCode(...img.subarray(table + 40 * i, table + 40 * i + 8)).replace(/\0+$/, '');
  const reloc = [...Array(nsec).keys()].find((i) => name(i) === '.reloc');
  if (reloc === undefined) throw new Error('no .reloc section (link with --dynamicbase)');
  if (table + 40 * (nsec + 1) > v.getUint32(opt + 60, true)) throw new Error('no room for a section header');
  const rva = v.getUint32(table + 40 * reloc + 12, true);
  const delta = up(size, sa);
  // every section from .reloc on, and the directories in them, move up;
  // their bytes stay where they are in the file
  for (let i = 0; i < nsec; i++) {
    const at = table + 40 * i + 12;
    if (v.getUint32(at, true) >= rva) v.setUint32(at, v.getUint32(at, true) + delta, true);
  }
  for (let i = 0; i < 16; i++) {
    const at = opt + 96 + 8 * i;
    if (i !== 4 && v.getUint32(at + 4, true) && v.getUint32(at, true) >= rva) v.setUint32(at, v.getUint32(at, true) + delta, true);
  }
  v.setUint32(opt + 56, v.getUint32(opt + 56, true) + delta, true);
  const raw = up(img.length, fa);
  const out = new Uint8Array(raw + up(size, fa));
  out.set(img);
  const o = new DataView(out.buffer);
  const s = table + 40 * nsec;
  out.set(new TextEncoder().encode('.rsrc'), s);
  o.setUint32(s + 8, size, true);
  o.setUint32(s + 12, rva, true);
  o.setUint32(s + 16, up(size, fa), true);
  o.setUint32(s + 20, raw, true);
  o.setUint32(s + 36, 0x40000040, true);
  o.setUint16(coff + 2, nsec + 1, true);
  o.setUint32(opt + 96 + 2 * 8, rva, true);
  o.setUint32(opt + 96 + 2 * 8 + 4, size, true);
  for (let i = raw + 16; i < raw + size; i += 4096) out[i] = ((i >> 12) & 0xff) | 1;
  return out;
}

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('bigimage', { src: join(SRC, 'bigimage.c'), flags: ['-Wl,--dynamicbase'] });
  const img = readFileSync(exe);
  const after = new Uint8Array(img.length + PAYLOAD);
  after.set(img);
  for (let i = img.length; i < after.length; i += 4096) after[i] = i & 0xff;
  const files = { after, inside: withResources(new Uint8Array(img), PAYLOAD) };
  for (const [name, bytes] of Object.entries(files)) {
    const file = `${exe}.${name}`;
    writeFileSync(file, bytes);
    const p = await w.page(['C:\\bigimage.exe'], { pfxFiles: { 'bigimage.exe': file }, prefix: w.pagePrefix(), cwd: '/pfx/drive_c', name: `p-${name}`, timeout: 600000 });
    t.check(`${name}: it ran`, /^bigimage ok$/m.test(p.out), w.detail(p));
    const took = p.out.match(/a single (\d+) MiB allocation takes a span of its own[^\n]*/g) ?? [];
    t.check(`${name}: no payload-sized span of the shim heap`, !took.some((l) => Number(l.match(/single (\d+)/)[1]) >= 64), took.join(' / '));
  }
  w.clean();
}
