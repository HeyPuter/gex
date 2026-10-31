// What src/bigdll.c's `work` returns after `calls` calls from x = 1, built
// with `funcs` functions (its comment says how).
export function expect(funcs, calls = 1) {
  let x = 1;
  for (let c = 0; c < calls; c++) {
    for (let i = 0; i < funcs; i++) {
      x = (Math.imul(x, 2 * i + 3) + i * 7 + 1) >>> 0;
      if (x & (1 << (i % 16))) x = (x ^ (x >>> (i % 13 + 1))) >>> 0;
      else x = (x + i * 31 + 5) >>> 0;
      for (let k = 0; k < (x & 3); k++) x = ((x << 3) ^ (x >>> (i % 7 + 2)) ^ i) >>> 0;
    }
  }
  return x;
}
