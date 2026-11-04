// The door's views of a client's memory (gex.mjs `memoryViews`) cover all of
// it, in a 64-bit memory past 4 GiB too: the word view's length was
// `byteLength >>> 2`, which wraps there (4 GiB + 64 KiB gave 16384 words).
import { memoryViews } from '../../hosts/browser/js/gex.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 60 };

export default async function (t) {
  const check = (what, buf) => {
    const v = memoryViews(buf);
    t.eq(`${what}: the byte view`, v.u8.length, buf.byteLength);
    t.eq(`${what}: the word view`, v.i32.length, Math.floor(buf.byteLength / 4));
    // The last word reads what the byte view wrote there.
    const at = buf.byteLength - 4;
    v.dv.setUint32(at, 0x5a5aa5a5, true);
    t.eq(`${what}: the last word`, v.i32[at / 4] >>> 0, 0x5a5aa5a5);
  };
  check('32-bit memory, 64 KiB', new WebAssembly.Memory({ initial: 1, maximum: 1, shared: true }).buffer);
  let big;
  try {
    big = new WebAssembly.Memory({ address: 'i64', initial: 65537n, maximum: 65537n, shared: true }).buffer;
  } catch (e) {
    return t.skip(`no 64-bit memory here: ${e.message}`);
  }
  check('64-bit memory, 4 GiB + 64 KiB', big);
}
