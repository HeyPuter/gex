// Windows 7's glass colour, from DWM's colorization parameters
// (`gex_glass_v1.set_colorization`; the registry's ColorizationColor,
// ColorizationColorBalance, ColorizationBlurBalance, ColorizationAfterglow
// and ColorizationAfterglowBalance):
//
//     glass = blurred * blur + colour * balance + afterglow * luma(blurred) * afterglowBalance
//
// with the balances 0..1 (they add up to 1 as the personalization page sets
// them: 49, 8 and 43 for the default "sky"). It is linear in the blurred
// backdrop, so it is one 3x3 matrix and an offset: a colour matrix for the
// page without a compositor (an SVG feColorMatrix, dom-glass.js), a few
// multiply-adds for the WebGPU compositor (compositor-worker.js).

/** Rec. 601 luma: the brightness the afterglow follows. */
const LUMA = [0.299, 0.587, 0.114];

const rgb = (argb) => [((argb >>> 16) & 0xff) / 255, ((argb >>> 8) & 0xff) / 255, (argb & 0xff) / 255];

/** `glass` (`readScene`'s): the matrix `m` (row-major, 3x3) and `offset`,
 *  such that glass = m * blurred + offset, per pixel, in 0..1 sRGB. */
export function glassColorMatrix(glass) {
  const blur = (glass?.blurBalance ?? 100) / 100;
  const balance = (glass?.balance ?? 0) / 100;
  const after = (glass?.afterglowBalance ?? 0) / 100;
  const colour = rgb(glass?.argb ?? 0);
  const afterglow = rgb(glass?.afterglow ?? 0);
  const m = [];
  for (let i = 0; i < 3; i++) {
    for (let j = 0; j < 3; j++) m.push((i === j ? blur : 0) + after * afterglow[i] * LUMA[j]);
  }
  return { m, offset: colour.map((c) => c * balance) };
}

/** The same as an SVG feColorMatrix's `values` (4x5, alpha kept). */
export function glassFeColorMatrix(glass) {
  const { m, offset } = glassColorMatrix(glass);
  const f = (n) => +n.toFixed(5);
  return [
    m[0], m[1], m[2], 0, offset[0],
    m[3], m[4], m[5], 0, offset[1],
    m[6], m[7], m[8], 0, offset[2],
    0, 0, 0, 1, 0,
  ].map(f).join(' ');
}
