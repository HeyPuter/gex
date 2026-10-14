// Windows 7's glass on a page with no compositor (bare-desktop.js).
//
// The scene compositor (`compositor-worker.js`, `Painter.glass`) blurs what
// it has drawn so far under a window's glass. Here the browser composites, so
// the browser blurs: each glass rectangle (`gex_glass_v1`, on the record as
// `DomWindow.glass`) is an element with a `backdrop-filter`, stacked directly
// under the window's canvas, and the canvas lets it show through wherever the
// window's alpha says so inside its glass (`DomWindow.paint`, DWM's rule).
// What lies behind -- the desktop, the windows beneath -- is blurred and
// coloured by the browser's own compositor, as it was by DWM: the filter is
// an SVG one (a Gaussian blur, then Windows 7's colorization as a colour
// matrix, glass-colorization.js), which `backdrop-filter: url()` applies.
//
// A frame the compositor draws (`gex_glass_v1.set_frame`: the client's
// window system left its frame to DWM) is a second canvas above the window's:
// the frame pieces, the caption text on its glow and the caption buttons, out
// of DWM's atlas from the user's own aero.msstyles (tools/aero/
// extract-dwm-frame.py, served at /gex/aero/). Without the atlas, the glass
// has no frame artwork and the caption text is drawn alone.

import { glassFeColorMatrix } from './glass-colorization.js';

const UI = 1;
/** The blur's standard deviation, in pixels: DWM's look at 96 dpi. */
const BLUR = 4;

/** The page's glass filters, one per colorization, in one hidden <svg>. */
const filters = new Map(); // values -> id
let svg = null;

function glassFilter(glass) {
  const values = glassFeColorMatrix(glass);
  let id = filters.get(values);
  if (id) return id;
  const ns = 'http://www.w3.org/2000/svg';
  if (!svg) {
    svg = document.createElementNS(ns, 'svg');
    svg.setAttribute('width', '0');
    svg.setAttribute('height', '0');
    svg.setAttribute('aria-hidden', 'true');
    svg.style.position = 'absolute';
    document.body.append(svg);
  }
  id = `gex-glass-${filters.size + 1}`;
  const f = document.createElementNS(ns, 'filter');
  f.id = id;
  f.setAttribute('color-interpolation-filters', 'sRGB');
  const blur = document.createElementNS(ns, 'feGaussianBlur');
  blur.setAttribute('stdDeviation', String(BLUR));
  blur.setAttribute('edgeMode', 'duplicate');
  const matrix = document.createElementNS(ns, 'feColorMatrix');
  matrix.setAttribute('type', 'matrix');
  matrix.setAttribute('values', values);
  f.append(blur, matrix);
  svg.append(f);
  filters.set(values, id);
  return id;
}

let aero = null; // null: not tried; a promise while loading; false: none; else { img, parts }
const waiting = new Set();

function aeroAtlas() {
  if (aero !== null) return aero instanceof Promise ? null : aero || null;
  aero = (async () => {
    const [json, png] = await Promise.all([
      fetch('/gex/aero/dwm-parts.json').then((r) => (r.ok ? r.json() : null)),
      fetch('/gex/aero/dwm-atlas.png').then((r) => (r.ok ? r.blob() : null)),
    ]);
    if (!json || !png) return false;
    const img = await createImageBitmap(png, { premultiplyAlpha: 'premultiply', colorSpaceConversion: 'none' });
    const parts = new Map();
    for (const p of json.parts) if (p.rect) parts.set(`${p.partName}:${p.state}`, p);
    return { img, parts };
  })().catch((e) => {
    console.warn('[dom-glass] no Aero frame artwork', e);
    return false;
  }).then((a) => {
    aero = a;
    for (const redraw of waiting) redraw();
    waiting.clear();
  });
  return null;
}

/** A part of the atlas drawn into a rectangle as a nine-grid: `src` its
 *  [x, y, w, h] in the atlas, `m` its fixed margins [left, right, top,
 *  bottom], the middle stretched. As `Painter.nineGrid`. */
function nineGrid(ctx, img, src, m, x, y, w, h) {
  if (w <= 0 || h <= 0) return;
  const [l, r, t, b] = m ?? [0, 0, 0, 0];
  const sx = [src[0], src[0] + l, src[0] + src[2] - r, src[0] + src[2]];
  const sy = [src[1], src[1] + t, src[1] + src[3] - b, src[1] + src[3]];
  const dl = Math.min(l, w / 2), dr = Math.min(r, w / 2), dt = Math.min(t, h / 2), db = Math.min(b, h / 2);
  const dx = [x, x + dl, x + w - dr, x + w];
  const dy = [y, y + dt, y + h - db, y + h];
  for (let j = 0; j < 3; j++) {
    for (let i = 0; i < 3; i++) {
      const sw = sx[i + 1] - sx[i], sh = sy[j + 1] - sy[j];
      const ow = dx[i + 1] - dx[i], oh = dy[j + 1] - dy[j];
      if (sw <= 0 || sh <= 0 || ow <= 0 || oh <= 0) continue;
      ctx.drawImage(img, sx[i], sy[j], sw, sh, dx[i], dy[j], ow, oh);
    }
  }
}

/** The glass rectangles clipped to the window, in window pixels. */
function glassRects(win) {
  const rects = [];
  for (const r of win.glass?.rects ?? []) {
    const x = Math.max(0, r.x), y = Math.max(0, r.y);
    const w = Math.min(win.width, r.x + r.w) - x, h = Math.min(win.height, r.y + r.h) - y;
    if (w > 0 && h > 0) rects.push({ x, y, w, h });
  }
  return rects;
}

/** DWM's frame, as `#drawAeroFrame` in compositor-worker.js, on a 2D canvas
 *  the window's size. */
function drawFrame(ctx, win, rects, hovered) {
  const g = win.glass;
  const a = aeroAtlas();
  const active = (g.frame & 2) !== 0;
  const part = (name, state) => a?.parts.get(`${name}:${state}`);
  const fs = active ? 1 : 2;
  if (a && rects.length) {
    const [top] = rects;
    const edges = [
      ['TOPFRAME', top],
      ['BOTTOMFRAME', rects.find((r) => r !== top && r.y > top.y + top.h && r.w >= top.w - 1)],
      ['LEFTFRAME', rects.find((r) => r.x === top.x && r.y > top.y && r.w < top.w / 2)],
      ['RIGHTFRAME', rects.find((r) => r.x > top.x && r.y > top.y && r.w < top.w / 2 && r.x + r.w === top.x + top.w)],
    ];
    for (const [name, r] of edges) {
      const q = part(name, fs);
      if (!q || !r) continue;
      const [l, rr, t, b] = q.sizingMargins ?? [0, 0, 0, 0];
      const half = (n) => Math.max(0, Math.floor((n - 1) / 2));
      const m = name === 'TOPFRAME' || name === 'BOTTOMFRAME'
        ? [l || half(q.rect[2]), rr || half(q.rect[2]), t, b]
        : [l, rr, t || half(q.rect[3]), b || half(q.rect[3])];
      nineGrid(ctx, a.img, q.rect, m, r.x, r.y, r.w, r.h);
    }
  }
  // The caption text, black on a white glow, left of the buttons.
  const title = win.title || '';
  if (title) {
    const font = `${Math.round(12 * UI)}px "Segoe UI", system-ui, sans-serif`;
    ctx.font = font;
    const tw = Math.ceil(ctx.measureText(title).width);
    const th = Math.round(16 * UI);
    const left = Math.round(30 * UI);
    const right = Math.min(...[g.close, g.max, g.min].filter((b) => b.state).map((b) => b.x), win.width);
    const w = Math.min(tw, right - left - Math.round(8 * UI));
    const y = Math.round(((rects[0]?.h ?? 30 * UI) - th) / 2);
    if (w > 4) {
      const glow = part('TEXTGLOW', 0);
      if (a && glow) {
        ctx.globalAlpha = active ? 0.9 : 0.6;
        nineGrid(ctx, a.img, glow.rect, glow.sizingMargins, left - 17, y - 8, w + 34, th + 16);
      }
      ctx.globalAlpha = active ? 1 : 0.7;
      ctx.save();
      ctx.beginPath();
      ctx.rect(left, y, w, th);
      ctx.clip();
      ctx.fillStyle = 'black';
      ctx.textBaseline = 'middle';
      ctx.fillText(title, left, y + th / 2);
      ctx.restore();
      ctx.globalAlpha = 1;
    }
  }
  // The buttons: their backgrounds, then their glyphs, centred.
  const maximized = (g.frame & 4) !== 0;
  const buttons = [
    [g.min, active ? 'BUTTONACTIVECAPTIONEND' : 'BUTTONINACTIVECAPTIONEND', 'BUTTONMINGLYPH96'],
    [g.max, active ? 'BUTTONACTIVECAPTION' : 'BUTTONINACTIVECAPTION', maximized ? 'BUTTONRESTOREGLYPH96' : 'BUTTONMAXGLYPH96'],
    [g.close, active ? 'BUTTONACTIVECLOSE' : 'BUTTONINACTIVECLOSE', 'BUTTONCLOSEGLYPH96'],
  ];
  for (const [b, bg, glyph] of buttons) {
    if (!b.state || b.w <= 0 || b.h <= 0) continue;
    const close = glyph === 'BUTTONCLOSEGLYPH96';
    // Hover is the compositor's to see, as it was DWM's.
    const state = b.state === 1 && hovered === b ? 2 : b.state;
    if (state === 2 && a) {
      const glow = part(close ? 'BUTTONCLOSEGLOW' : 'BUTTONGLOW', 0) ?? part(close ? 'BUTTONCLOSEGLOW' : 'BUTTONGLOW', 1);
      if (glow) {
        const [gx, gy, gw, gh] = glow.rect;
        ctx.drawImage(a.img, gx, gy, gw, gh, b.x + (b.w - gw) / 2, b.y + (b.h - gh) / 2, gw, gh);
      }
    }
    const back = part(bg, state);
    if (back) nineGrid(ctx, a.img, back.rect, back.sizingMargins, b.x, b.y, b.w, b.h);
    else {
      ctx.fillStyle = close ? 'rgba(199, 64, 51, 0.9)' : 'rgba(217, 230, 255, 0.6)';
      ctx.fillRect(b.x, b.y, b.w, b.h);
    }
    const gl = part(glyph, state);
    if (gl) {
      const [gx, gy, gw, gh] = gl.rect;
      ctx.drawImage(a.img, gx, gy, gw, gh, b.x + Math.floor((b.w - gw) / 2), b.y + Math.floor((b.h - gh) / 2), gw, gh);
    }
  }
}

/**
 * Glass for `win` (a `DomWindow` whose canvas the page has placed in
 * `mount`): call the returned `sync()` whenever the canvas moves, is resized
 * or restacked, and `remove()` when it goes.
 */
export function domGlass(win, mount) {
  const c = win.canvas;
  let panes = [];
  let overlay = null;
  let hovered = null;
  let sig = '';

  const place = () => {
    const x = parseFloat(c.style.left) || 0;
    const y = parseFloat(c.style.top) || 0;
    const z = c.style.zIndex;
    const rects = glassRects(win);
    // The panes: one element per rectangle, before the canvas in the
    // document so that at the canvas's z-index they are under it and over
    // everything stacked lower.
    while (panes.length > rects.length) panes.pop().remove();
    const g = win.glass;
    const filter = g ? `url(#${glassFilter(g)})` : 'none';
    rects.forEach((r, i) => {
      let p = panes[i];
      if (!p) {
        p = document.createElement('div');
        p.className = 'gex-glass';
        p.style.position = 'absolute';
        p.style.pointerEvents = 'none';
        panes.push(p);
      }
      if (p.nextSibling !== c) c.before(p);
      p.style.left = `${x + r.x}px`;
      p.style.top = `${y + r.y}px`;
      p.style.width = `${r.w}px`;
      p.style.height = `${r.h}px`;
      p.style.zIndex = z;
      p.style.backdropFilter = filter;
    });
    const framed = g && (g.frame & 1) !== 0;
    if (!framed) {
      overlay?.remove();
      overlay = null;
      return;
    }
    if (!overlay) {
      overlay = document.createElement('canvas');
      overlay.className = 'gex-glass-frame';
      overlay.style.position = 'absolute';
      overlay.style.pointerEvents = 'none';
    }
    if (overlay.previousSibling !== c) c.after(overlay);
    overlay.style.left = `${x}px`;
    overlay.style.top = `${y}px`;
    overlay.style.zIndex = z;
    if (overlay.width !== win.width || overlay.height !== win.height) {
      overlay.width = Math.max(1, win.width);
      overlay.height = Math.max(1, win.height);
      sig = '';
    }
    const now = JSON.stringify([win.glass, win.title, hovered && [hovered.x, hovered.y], !!aeroAtlas()]);
    if (now === sig) return;
    sig = now;
    const ctx = overlay.getContext('2d');
    ctx.clearRect(0, 0, overlay.width, overlay.height);
    drawFrame(ctx, win, rects, hovered);
    if (aero === null || aero instanceof Promise) waiting.add(place);
  };

  // Hover: the caption button under the pointer, as DWM tracked it.
  const hover = (e) => {
    const g = win.glass;
    let b = null;
    if (g && (g.frame & 1) && e) {
      const x = e.offsetX, y = e.offsetY;
      b = [g.close, g.max, g.min].find((q) => q.state && x >= q.x && x < q.x + q.w && y >= q.y && y < q.y + q.h) ?? null;
    }
    if (b === hovered) return;
    hovered = b;
    place();
  };
  const onMove = (e) => hover(e);
  const onLeave = () => hover(null);
  c.addEventListener('pointermove', onMove);
  c.addEventListener('pointerleave', onLeave);

  return {
    sync: place,
    remove() {
      for (const p of panes) p.remove();
      panes = [];
      overlay?.remove();
      overlay = null;
      waiting.delete(place);
      c.removeEventListener('pointermove', onMove);
      c.removeEventListener('pointerleave', onLeave);
    },
  };
}
