// Windows 7 DWM's window frame, as uDWM.dll lays it out (from its public
// symbols: CTopLevelWindow's UpdatePinnedParts, UpdateHighlights,
// UpdateEnvironmentMapViewPort, UpdateNCAreaPositionsAndSizes, CText,
// CButton), drawn with a 2D context out of DWM's atlas from the user's own
// aero.msstyles (tools/aero/extract-dwm-frame.py's dwm-atlas.png and
// dwm-parts.json).
//
// Back to front, all under the window's client area (which covers them):
// the environment map (REFLECTIONMAP, the glass's streaks, stretched over the
// virtual screen and shifted by an eighth of the window's offset from its
// centre), the two highlight corners, the side highlights, the eight frame
// pieces, the four shadows (outside the window), the caption text's glow,
// the caption text, the caption buttons. The env map and the corners are
// clipped to the highlight region: the window's round rect less the
// glass's inner round rect.
//
// Every atlas image is its ATLASRECT slice deflated by a pixel on each side
// (CreateBitmapsFromAtlasImageStrip); nine-grids follow DWM's layout
// (CDrawNineGridInstruction::GetNineGridLayout). Opacities are quantized to
// 255ths, as CMeshImage::SetOpacity does.

/** GDI's CreateRoundRectRgn (as Wine's, which follows Windows): the region's
 *  rows, { top, bottom, rows: [[y, left, right], ...] } for the rows the
 *  corners narrow; every other row in [top, bottom) is [left, right). Note
 *  right and bottom are one less than asked, as Windows has it. */
export function roundRectRegion(left, top, right, bottom, ew, eh) {
  if (left > right) [left, right] = [right, left];
  if (top > bottom) [top, bottom] = [bottom, top];
  right--;
  bottom--;
  ew = Math.min(right - left, Math.abs(ew));
  eh = Math.min(bottom - top, Math.abs(eh));
  const region = { left, top, right, bottom, rows: [] };
  if (ew < 2 || eh < 2) return region;
  const rects = new Array(eh).fill(null).map(() => ({ left, right }));
  const a = ew - 1, b = eh - 1;
  const asq = 8 * a * a, bsq = 8 * b * b;
  let dx = 4 * b * b * (1 - a), dy = 4 * a * a * (1 + (b % 2));
  let err = dx + dy + a * a * (b % 2);
  let x = 0, y = Math.floor(eh / 2);
  rects[y] = { left, right };
  while (x <= Math.floor(ew / 2)) {
    const e2 = 2 * err;
    if (e2 >= dx) { x++; err += dx += bsq; }
    if (e2 <= dy) { y++; err += dy += asq; if (y < eh) rects[y] = { left: left + x, right: right - x }; }
  }
  const half = Math.floor(eh / 2);
  for (let i = 0; i < half; i++) {
    region.rows.push([top + i, rects[b - i].left, rects[b - i].right]);
  }
  for (let i = half + 1; i < eh; i++) {
    region.rows.push([bottom - eh + i, rects[i].left, rects[i].right]);
  }
  return region;
}

/** Clip `ctx` to a region (roundRectRegion's) less another (or null). */
function clipRegion(ctx, outer, inner = null) {
  ctx.beginPath();
  const narrowed = new Map(outer.rows.map(([y, l, r]) => [y, [l, r]]));
  // the outer region, row runs merged where they are the same
  let y = outer.top;
  while (y < outer.bottom) {
    const [l, r] = narrowed.get(y) ?? [outer.left, outer.right];
    let y1 = y + 1;
    while (y1 < outer.bottom && !narrowed.has(y1) && !narrowed.has(y)) y1++;
    if (r > l) ctx.rect(l, y, r - l, y1 - y);
    y = y1;
  }
  if (inner) {
    // even-odd: the inner region's rows cut out
    const inn = new Map(inner.rows.map(([yy, l, r]) => [yy, [l, r]]));
    for (let yy = inner.top; yy < inner.bottom; yy++) {
      const [l, r] = inn.get(yy) ?? [inner.left, inner.right];
      if (r > l) ctx.rect(l, yy, r - l, 1);
    }
    ctx.clip('evenodd');
  } else {
    ctx.clip();
  }
}

/** DWM's nine-grid layout of `size` with fixed ends `a` and `b`. */
function layout(size, a, b) {
  if (a + b > size) {
    const l = Math.floor((size * a) / (a + b));
    return [l, 0, size - l];
  }
  return [a, size - a - b, b];
}

/** A nine-grid: `src` [x, y, w, h] in the atlas, `m` [left, right, top,
 *  bottom], into (x, y, w, h); stretched cells sample half a texel in. */
export function nineGrid(ctx, img, src, m, x, y, w, h) {
  if (w <= 0 || h <= 0) return;
  const [ml, mr, mt, mb] = m ?? [0, 0, 0, 0];
  const sw = layout(src[2], ml, mr), sh = layout(src[3], mt, mb);
  const dw = layout(w, ml, mr), dh = layout(h, mt, mb);
  let sy = src[1], dy = y;
  for (let j = 0; j < 3; j++) {
    let sx = src[0], dx = x;
    for (let i = 0; i < 3; i++) {
      if (dw[i] > 0 && dh[j] > 0 && sw[i] > 0 && sh[j] > 0) {
        let u = sx, v = sy, uw = sw[i], vh = sh[j];
        if (i === 1 && dw[i] !== sw[i]) { u += 0.5; uw -= 1; }
        if (j === 1 && dh[j] !== sh[j]) { v += 0.5; vh -= 1; }
        if (uw > 0 && vh > 0) ctx.drawImage(img, u, v, uw, vh, dx, dy, dw[i], dh[j]);
      }
      sx += sw[i];
      dx += dw[i];
    }
    sy += sh[j];
    dy += dh[j];
  }
}

const op255 = (v) => Math.min(255, Math.max(0, Math.round(v * 255))) / 255;

/** The atlas (`{ img, parts }`, parts keyed "NAME:state") slice of a part,
 *  deflated a pixel each side, with its margins. */
function slice(atlas, name, state) {
  const get = (s) => atlas.parts.get(`${name}:${s}`);
  // a state with no image of its own (the maximized frames' 3 and 4, an
  // inactive state with the active's image) has the part's image
  const p = get(state) ?? (state > 2 ? get(state - 2) : null);
  const rect = p?.rect ?? (state > 2 ? get(state - 2)?.rect : null) ?? get(1)?.rect ?? get(0)?.rect;
  if (!rect) return null;
  const [x, y, w, h] = rect;
  return {
    src: [x + 1, y + 1, Math.max(0, w - 2), Math.max(0, h - 2)],
    sizing: p?.sizingMargins ?? get(0)?.sizingMargins ?? [0, 0, 0, 0],
    content: p?.contentMargins ?? get(0)?.contentMargins ?? [0, 0, 0, 0],
    props: p?.properties ?? {},
  };
}

/** The frame's parts: normal, tool window (SMALL*). */
const PARTS = {
  normal: { top: 'TOPFRAME', left: 'LEFTFRAME', right: 'RIGHTFRAME', bottom: 'BOTTOMFRAME' },
  tool: { top: 'SMALLTOPFRAME', left: 'SMALLLEFTFRAME', right: 'SMALLRIGHTFRAME', bottom: 'SMALLBOTTOMFRAME' },
};

/** The environment map (REFLECTIONMAP) at `opacity` (the frame's, before
 *  ColorizationGlassReflectionIntensity), stretched over the virtual screen
 *  and shifted by an eighth of the window's offset from its centre
 *  (CTopLevelWindow::UpdateEnvironmentMapViewPort), into the current clip. */
function envMap(ctx, atlas, f, opacity) {
  const env = slice(atlas, 'REFLECTIONMAP', 1) ?? slice(atlas, 'REFLECTIONMAP', 0);
  if (!env) return;
  const scr = f.screen ?? { x: 0, y: 0, vx: 0, vy: 0, vcx: 1024, vcy: 768 };
  const rl = scr.x + 1, rt = scr.y + 1;
  const w = Math.max(f.W - 2, 0);
  const t = Math.trunc;
  const vpx = scr.vx - rl + t((rl + t(w / 2) - t(scr.vcx / 2) - scr.vx) * 0.125);
  const vpy = scr.vy - rt;
  // the viewport is relative to the deflated window rect's corner, (1, 1)
  ctx.globalAlpha = op255(opacity * ((f.reflection ?? 50) / 50));
  ctx.drawImage(atlas.img, ...env.src, 1 + vpx, 1 + vpy, scr.vcx, scr.vcy);
}

/**
 * The environment map over a window's blur-behind glass (no DWM frame: the
 * taskbar, the Start menu), clipped to the glass (CTopLevelWindow's blur
 * canvas and its CImage). `f`: W, H, rects (window pixels), active, screen,
 * reflection, as drawDwmFrame's.
 */
export function drawDwmBlurBehind(ctx, atlas, f) {
  if (!f.rects?.length) return;
  const p = atlas.parts.get(`REFLECTIONMAP:${f.active ? 1 : 2}`);
  const v = p?.properties?.OPACITY;
  ctx.save();
  ctx.beginPath();
  for (const r of f.rects) ctx.rect(r.x, r.y, r.w, r.h);
  ctx.clip();
  envMap(ctx, atlas, f, typeof v === 'number' ? v / 100 : f.active ? 0.9 : 0.75);
  ctx.restore();
}

/** Shadow sizes, deflated: left, top, right, bottom. */
export const DWM_SHADOW = { l: 14, t: 14, r: 20, b: 20 };

/**
 * Draw a window's DWM frame into `ctx`, whose origin is the window's
 * top-left (the shadows go outside it: give the canvas DWM_SHADOW's room).
 * `f`:
 *   W, H          the window's size
 *   N, G          non-client and glass margins { l, r, t, b }
 *   active, maximized, tool
 *   title, font   the caption and its CSS font (lfCaptionFont)
 *   buttons       [{ kind: 'close'|'max'|'min'|'help', x, y, w, h, state }]
 *                 (state 0 none, 1 normal, 2 hot, 3 pushed, 4 disabled)
 *   screen        { x, y } the window's place, { vx, vy, vcx, vcy } the
 *                 virtual screen
 *   reflection    ColorizationGlassReflectionIntensity (default 50)
 *   shadows       drop shadows on (SPI_GETDROPSHADOW; default true)
 *   sheet         a sheet of glass (DwmExtendFrameIntoClientArea -1)
 */
export function drawDwmFrame(ctx, atlas, f) {
  const { W, H, N, G } = f;
  const img = atlas.img;
  const st = f.active ? 1 : 2;
  const pst = f.maximized ? (f.active ? 3 : 4) : st; // property states
  const parts = f.tool ? PARTS.tool : PARTS.normal;
  const r = f.tool ? 0 : 6;
  const top = slice(atlas, parts.top, st);
  const prop = (name, key, fallback) => {
    const p = atlas.parts.get(`${name}:${pst}`) ?? atlas.parts.get(`${name}:${st}`);
    const v = p?.properties?.[key];
    return typeof v === 'number' ? v / 100 : fallback;
  };
  // The highlight region: the window's round rect less the glass's inner
  // one (none for a sheet of glass).
  const outer = roundRectRegion(1, 1, Math.max(1, W - 1), Math.max(1, H - 1), 2 * r, 2 * r);
  const inner = f.sheet ? null
    : roundRectRegion(G.l - 1, G.t - 1, Math.max(G.l - 1, W - G.r + 1), Math.max(G.t - 1, H - G.b + 1), 2, 2);

  // 1. The environment map, over the virtual screen.
  ctx.save();
  clipRegion(ctx, outer, inner);
  envMap(ctx, atlas, f, prop('REFLECTIONMAP', 'OPACITY', f.active ? 0.9 : 0.75));
  ctx.restore();

  // 2. The highlight corners: the left one, and its mirror on the right.
  const corner = slice(atlas, 'LEFTHIGHLIGHTCORNER', st);
  if (corner && !f.maximized) {
    const [, , cw, ch] = corner.src;
    const px = 1, py = 1, qx = W - 1, qy = H - 1;
    ctx.save();
    clipRegion(ctx, outer, inner);
    ctx.beginPath();
    const half = Math.trunc(W / 2);
    ctx.rect(px, py, Math.min(half, px + cw) - px, Math.min(qy, py + ch) - py);
    ctx.clip();
    ctx.globalAlpha = op255(prop('LEFTHIGHLIGHTCORNER', 'OPACITY', 0.75));
    ctx.drawImage(img, ...corner.src, px, py, cw, ch);
    ctx.restore();
    const x0 = qx - cw;
    ctx.save();
    clipRegion(ctx, outer, inner);
    ctx.beginPath();
    const l = Math.max(half, x0);
    ctx.rect(l, py, qx - l, Math.min(qy, py + ch) - py);
    ctx.clip();
    ctx.globalAlpha = op255(prop('LEFTHIGHLIGHTCORNER', 'OPACITY', 0.75));
    ctx.translate(x0 + cw, py);
    ctx.scale(-1, 1);
    ctx.drawImage(img, ...corner.src, 0, 0, cw, ch);
    ctx.restore();
  }

  // 3. The side highlights: the top quarter of the side bands below the
  //    caption.
  const side = slice(atlas, 'SIDEHIGHLIGHT', st);
  if (side && !f.sheet) {
    const opacity = op255(prop('SIDEHIGHLIGHT', 'OPACITY', f.active ? 1 : 0.5));
    const band = Math.max(H - G.t - G.b, 0);
    const h = band - Math.floor(0.75 * band);
    ctx.save();
    ctx.globalAlpha = opacity;
    const lw = Math.max(G.l - 2, 0), rw = Math.max(G.r - 2, 0);
    nineGrid(ctx, img, side.src, side.sizing, 1, G.t, lw, h);
    nineGrid(ctx, img, side.src, side.sizing, W - 1 - rw, G.t, rw, h);
    ctx.restore();
  }

  // 4. The frame: the top and bottom strips cut in three columns at the top
  //    part's left content margin, each a nine-grid with its slot's margins.
  const left = slice(atlas, parts.left, st), right = slice(atlas, parts.right, st);
  const bottom = slice(atlas, parts.bottom, st);
  if (top && left && right && bottom) {
    const c = top.content[0] || 9;
    const cut = (s) => {
      const [x, y, w, h] = s.src;
      return [[x, y, c, h], [x + c, y, Math.max(0, w - 2 * c), h], [x + w - c, y, c, h]];
    };
    const [tl, tc, tr] = cut(top), [bl, bc, br] = cut(bottom);
    const L = left.sizing, R = right.sizing, T = top.sizing, B = bottom.sizing;
    const cw = Math.max(0, W - G.l - G.r);
    nineGrid(ctx, img, tl, [L[0], L[1], T[2], T[3]], 0, 0, G.l, G.t);
    nineGrid(ctx, img, tc, T, G.l, 0, cw, G.t);
    nineGrid(ctx, img, tr, [R[0], R[1], T[2], T[3]], W - G.r, 0, G.r, G.t);
    const mh = Math.max(0, H - G.t - G.b);
    nineGrid(ctx, img, left.src, L, 0, G.t, G.l, mh);
    nineGrid(ctx, img, right.src, R, W - G.r, G.t, G.r, mh);
    nineGrid(ctx, img, bl, [L[0], L[1], B[2], B[3]], 0, H - G.b, G.l, G.b);
    nineGrid(ctx, img, bc, B, G.l, H - G.b, cw, G.b);
    nineGrid(ctx, img, br, [R[0], R[1], B[2], B[3]], W - G.r, H - G.b, G.r, G.b);
  }

  // 5. The shadows, outside the window.
  if (f.shadows !== false && !f.maximized) {
    const s = DWM_SHADOW;
    const sl = slice(atlas, 'LEFTSHADOW', st), sr = slice(atlas, 'RIGHTSHADOW', st);
    const stp = slice(atlas, 'TOPSHADOW', st), sb = slice(atlas, 'BOTTOMSHADOW', st);
    if (sl) nineGrid(ctx, img, sl.src, sl.sizing, -s.l, 0, s.l, H);
    if (sr) nineGrid(ctx, img, sr.src, sr.sizing, W, 0, s.r, H);
    if (stp) nineGrid(ctx, img, stp.src, stp.sizing, -s.l, -s.t, W + s.l + s.r, s.t);
    if (sb) nineGrid(ctx, img, sb.src, sb.sizing, -s.l, H, W + s.l + s.r, s.b);
  }

  // 6. The caption: its glow (TEXTGLOW stretched around the text), then the
  //    text, in the band [0, N.t) right of the icon and left of the buttons.
  const iconW = f.icon === false ? 0 : 16;
  const tx = N.l + (iconW ? iconW + 5 : 0);
  const shown = (f.buttons ?? []).filter((b) => b.state);
  const tr = shown.length ? Math.min(...shown.map((b) => b.x)) : W - N.r;
  const tvw = Math.max(0, tr - tx), tvh = N.t;
  if (f.title && tvw > 0) {
    ctx.save();
    ctx.font = f.font ?? '12px "Segoe UI", system-ui, sans-serif';
    let text = f.title;
    const measure = (s) => Math.ceil(ctx.measureText(s).width);
    if (measure(text) > tvw) {
      while (text.length && measure(text + '...') > tvw) text = text.slice(0, -1);
      text += '...';
    }
    const m = ctx.measureText(text);
    const textW = Math.min(tvw, measure(text));
    const textH = Math.ceil((m.fontBoundingBoxAscent ?? 12) + (m.fontBoundingBoxDescent ?? 4));
    const glow = slice(atlas, 'TEXTGLOW', 0) ?? slice(atlas, 'TEXTGLOW', 1);
    const gw = Math.min(tvw, textW), gh = Math.min(tvh, textH);
    const gy = Math.trunc((tvh - gh) / 2);
    if (glow) {
      ctx.save();
      ctx.beginPath();
      ctx.rect(1, 0, tx + tvw + 17 - 1, N.t - 1);
      ctx.clip();
      ctx.globalAlpha = op255(prop(parts.top, 'OPACITY', f.active ? 0.8 : 0.6));
      nineGrid(ctx, img, glow.src, glow.sizing, tx - 17, gy - 8, gw + 34, gh + 18);
      ctx.restore();
    }
    ctx.beginPath();
    ctx.rect(tx, 0, tvw, tvh);
    ctx.clip();
    ctx.fillStyle = f.textColor ?? '#000';
    ctx.textBaseline = 'alphabetic';
    const ty = ((tvh - textH) >> 1) + (m.fontBoundingBoxAscent ?? 12);
    ctx.fillText(text, tx, ty);
    ctx.restore();
  }

  // 7. The buttons: background, glow when hot, glyph centred in the
  //    background's content margins.
  const hasHelp = shown.some((b) => b.kind === 'help');
  const any = shown.some((b) => b.kind !== 'close');
  const A = f.active ? 'ACTIVE' : 'INACTIVE';
  for (const b of shown) {
    const state = b.state === 2 ? 2 : b.state === 3 ? 3 : b.state === 4 ? 4 : 1;
    let bg;
    if (f.tool) bg = `SMALLBUTTON${A}CLOSE`;
    else if (b.kind === 'close') bg = any ? `BUTTON${A}CLOSE` : `BUTTON${A}CLOSEALONE`;
    else if (b.kind === 'max') bg = `BUTTON${A}CAPTION`;
    else if (b.kind === 'min') bg = hasHelp ? `BUTTON${A}CAPTION` : `BUTTON${A}CAPTIONEND`;
    else bg = `BUTTON${A}CAPTIONEND`;
    const back = slice(atlas, bg, state) ?? slice(atlas, bg, 1);
    if (back) nineGrid(ctx, img, back.src, back.sizing, b.x, b.y, b.w, b.h);
    if (state === 2) {
      const gname = b.kind === 'close' ? (f.tool ? 'SMALLBUTTONCLOSEGLOW' : 'BUTTONCLOSEGLOW') : 'BUTTONGLOW';
      const g = slice(atlas, gname, 1) ?? slice(atlas, gname, 0);
      if (g) {
        const [ml, mr, mt, mb] = g.sizing;
        nineGrid(ctx, img, g.src, g.sizing, b.x - ml, b.y - mt, b.w + ml + mr, b.h + mt + mb);
      }
    }
    const glyphName = {
      close: f.tool ? 'SMALLBUTTONCLOSEGLYPH96' : 'BUTTONCLOSEGLYPH96',
      max: f.maximized ? 'BUTTONRESTOREGLYPH96' : 'BUTTONMAXGLYPH96',
      min: 'BUTTONMINGLYPH96',
      help: 'BUTTONHELPGLYPH96',
    }[b.kind];
    const gl = slice(atlas, glyphName, state) ?? slice(atlas, glyphName, 1);
    if (gl && back) {
      const [cl, cr, ct, cb] = back.content;
      const [, , gw, gh] = gl.src;
      const x = b.x + cl + Math.round((b.w - cl - cr - gw) / 2);
      const y = b.y + ct + Math.round((b.h - ct - cb - gh) / 2);
      ctx.drawImage(img, ...gl.src, x, y, gw, gh);
    }
  }
}

/** drawDwmFrame's description of a window with `glass` (gex_glass_v1, as the
 *  compositors keep it: rects, client, frame flags, buttons), `w` x `h`, at
 *  (x, y) on a screen W x H; `hovered` the caption button under the pointer. */
export function dwmFrameOf(glass, w, h, title, x, y, W, H, hovered = null) {
  const g = glass;
  const c = g.client?.w > 0 ? g.client : { x: 0, y: 0, w, h };
  const N = { l: c.x, t: c.y, r: w - c.x - c.w, b: h - c.y - c.h };
  // the glass margins: the bands' widths (a sheet of glass is one rect)
  const rects = g.rects ?? [];
  const sheet = rects.some((r) => r.x <= 0 && r.y <= 0 && r.x + r.w >= w && r.y + r.h >= h);
  const G = { ...N };
  for (const r of rects) {
    if (r.y <= 0 && r.w >= w - 1) G.t = Math.max(G.t, r.y + r.h);
    else if (r.y + r.h >= h && r.w >= w - 1) G.b = Math.max(G.b, h - r.y);
    else if (r.x <= 0) G.l = Math.max(G.l, r.x + r.w);
    else if (r.x + r.w >= w) G.r = Math.max(G.r, w - r.x);
  }
  const button = (kind, b) => ({
    kind, x: b?.x ?? 0, y: b?.y ?? 0, w: b?.w ?? 0, h: b?.h ?? 0,
    state: !b ? 0 : b.state === 1 && hovered === b ? 2 : b.state,
  });
  return {
    W: w, H: h, N, G, sheet,
    active: (g.frame & 2) !== 0,
    maximized: (g.frame & 4) !== 0,
    title: title || '',
    buttons: [button('min', g.min), button('max', g.max), button('close', g.close)],
    screen: { x, y, vx: 0, vy: 0, vcx: W, vcy: H },
  };
}

/** A window's glass rectangles (window pixels) in screen pixels, the top and
 *  bottom bands of a DWM frame cut to the window's round rect, as DWM's
 *  frame glass is (CTopLevelWindow::UpdateNCAreaGeometry). */
export function dwmGlassClips(glass, w, h, x, y, rects) {
  const framed = (glass?.frame & 1) !== 0;
  const maximized = (glass?.frame & 4) !== 0;
  const rr = framed && !maximized ? roundRectRegion(x, y, x + w, y + h, 12, 12) : null;
  return rects.map((r) => {
    const band = rr && r.w >= w - 1 && (r.y <= 0 || r.y + r.h >= h);
    return { x: x + r.x, y: y + r.y, w: r.w, h: r.h, clip: band ? rr : null };
  });
}
