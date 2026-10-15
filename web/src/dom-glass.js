// Windows 7's glass on a page with no compositor (bare-desktop.js).
//
// The scene compositor (`compositor-worker.js`, `Painter.glass`) blurs what
// it has drawn so far under a window's glass. Here the browser composites, so
// the browser blurs: each glass rectangle (`gex_glass_v1`, on the record as
// `DomWindow.glass`) is an element with a `backdrop-filter`, stacked directly
// under the window's canvas, and the canvas lets it show through wherever the
// window's alpha says so inside its glass (`DomWindow.paint`, DWM's rule).
// What lies behind -- the desktop, the windows beneath -- is blurred and
// coloured as DWM does it (dwm-glass.js) where the page has WebGPU: a glass
// scene (`domGlassScene`) puts the windows' canvases together bottom to top
// in a screen-sized texture, runs DWM's passes at each window with glass,
// and fills that window's pane (a WebGPU canvas) from the result. Without
// WebGPU the browser's compositor approximates it: the pane's
// `backdrop-filter` is an SVG filter (a Gaussian blur, then the
// colorization as a colour matrix, glass-colorization.js).
//
// A frame the compositor draws (`gex_glass_v1.set_frame`: the client's
// window system left its frame to DWM) is a second canvas above the window's:
// the frame pieces, the caption text on its glow and the caption buttons, out
// of DWM's atlas from the user's own aero.msstyles (tools/aero/
// extract-dwm-frame.py, served at /gex/aero/). Without the atlas, the glass
// has no frame artwork and the caption text is drawn alone.

import { glassFeColorMatrix } from './glass-colorization.js';
import { DwmGlass, dwmGlassConstants } from './dwm-glass.js';
import { drawDwmBlurBehind, drawDwmFrame, dwmFrameOf, dwmGlassClips, loadDwmFont, DWM_SHADOW } from './dwm-frame.js';

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
      loadDwmFont(document.fonts),
    ]);
    if (!json || !png) return false;
    const img = await createImageBitmap(png, { premultiplyAlpha: 'premultiply', colorSpaceConversion: 'none' });
    const parts = new Map();
    for (const p of json.parts) parts.set(`${p.partName}:${p.state}`, p);
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

/**
 * Glass for `win` (a `DomWindow` whose canvas the page has placed in
 * `mount`): call the returned `sync()` whenever the canvas moves, is resized
 * or restacked, and `remove()` when it goes. `scene` (domGlassScene's) draws
 * DWM's own glass into the window's pane; without one, or until its WebGPU
 * device is up, the pane is a CSS backdrop filter.
 */
export function domGlass(win, mount, scene = null) {
  const c = win.canvas;
  let panes = [];
  let pane = null; // the scene's: one WebGPU canvas over the glass's bounds
  let overlay = null;
  let hovered = null;
  let sig = '';

  const place = () => {
    const x = parseFloat(c.style.left) || 0;
    const y = parseFloat(c.style.top) || 0;
    const z = c.style.zIndex;
    const rects = glassRects(win);
    const g = win.glass;
    if (scene?.ready && rects.length) {
      // DWM's glass, drawn by the scene into one canvas under the window.
      for (const p of panes) p.remove();
      panes = [];
      let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
      for (const r of rects) {
        x0 = Math.min(x0, r.x); y0 = Math.min(y0, r.y);
        x1 = Math.max(x1, r.x + r.w); y1 = Math.max(y1, r.y + r.h);
      }
      if (!pane) pane = scene.pane();
      const cv = pane.canvas;
      const above = overlay ?? c;
      if (cv.nextSibling !== above) above.before(cv);
      cv.style.left = `${x + x0}px`;
      cv.style.top = `${y + y0}px`;
      cv.style.zIndex = z;
      pane.bounds = { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
      pane.rects = rects;
      pane.win = win;
      pane.overlay = () => overlay;
      scene.add(pane);
    } else {
      if (pane) { scene.drop(pane); pane = null; }
      // The panes: one element per rectangle, before the canvas in the
      // document so that at the canvas's z-index they are under it and over
      // everything stacked lower.
      while (panes.length > rects.length) panes.pop().remove();
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
        const above = i === rects.length - 1 ? (overlay ?? c) : null;
        if (above && p.nextSibling !== above) above.before(p);
        else if (!above && !p.isConnected) (overlay ?? c).before(p);
        p.style.left = `${x + r.x}px`;
        p.style.top = `${y + r.y}px`;
        p.style.width = `${r.w}px`;
        p.style.height = `${r.h}px`;
        p.style.zIndex = z;
        p.style.backdropFilter = filter;
      });
    }
    scene?.dirty();
    const framed = g && (g.frame & 1) !== 0;
    if (!framed && !rects.length) {
      overlay?.remove();
      overlay = null;
      return;
    }
    if (!overlay) {
      overlay = document.createElement('canvas');
      overlay.className = 'gex-glass-frame';
      overlay.style.position = 'absolute';
      overlay.style.pointerEvents = 'none';
      overlay.version = 0;
    }
    // DWM's frame is under the client area (the window's canvas, see-through
    // outside its client rectangle) and over the glass, with room for the
    // shadows outside the window.
    if (overlay.nextSibling !== c) c.before(overlay);
    const S = DWM_SHADOW;
    overlay.style.left = `${x - S.l}px`;
    overlay.style.top = `${y - S.t}px`;
    overlay.style.zIndex = z;
    if (overlay.width !== win.width + S.l + S.r || overlay.height !== win.height + S.t + S.b) {
      overlay.width = Math.max(1, win.width + S.l + S.r);
      overlay.height = Math.max(1, win.height + S.t + S.b);
      sig = '';
    }
    const W = mount.clientWidth, H = mount.clientHeight;
    const now = JSON.stringify([win.glass, win.title, hovered && [hovered.x, hovered.y], !!aeroAtlas(), x, y, W, H,
      !framed && win.focused]);
    if (now === sig) return;
    sig = now;
    const ctx = overlay.getContext('2d');
    ctx.clearRect(0, 0, overlay.width, overlay.height);
    const a = aeroAtlas();
    if (a) {
      ctx.save();
      ctx.translate(S.l, S.t);
      if (framed) drawDwmFrame(ctx, a, dwmFrameOf(win.glass, win.width, win.height, win.title, x, y, W, H, hovered));
      else {
        drawDwmBlurBehind(ctx, a, {
          W: win.width, H: win.height, rects, active: win.focused === true,
          screen: { x, y, vx: 0, vy: 0, vcx: W, vcy: H },
        });
      }
      ctx.restore();
    }
    overlay.version++;
    if (aero === null || aero instanceof Promise) waiting.add(place);
  };
  scene?.whenReady(place);

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
      if (pane) { scene.drop(pane); pane.canvas.remove(); pane = null; }
      overlay?.remove();
      overlay = null;
      waiting.delete(place);
      c.removeEventListener('pointermove', onMove);
      c.removeEventListener('pointerleave', onLeave);
      scene?.dirty();
    },
  };
}

// -- DWM's glass on WebGPU ---------------------------------------------------

const SCENE_WGSL = /* wgsl */ `
struct U { rect: vec4f, viewport: vec2f, src: vec2f, mode: u32 };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var tex: texture_2d<f32>;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {
  let corner = vec2f(f32(i & 1u), f32((i >> 1u) & 1u));
  let px = u.rect.xy + corner * u.rect.zw;
  return vec4f(px.x / u.viewport.x * 2.0 - 1.0, 1.0 - px.y / u.viewport.y * 2.0, 0.0, 1.0);
}
// The texel under this pixel, 1:1: a window's canvas at its place (mode 0),
// or the scene's glass into a pane (mode 1).
@fragment fn fs(@builtin(position) at: vec4f) -> @location(0) vec4f {
  let p = vec2i(floor(at.xy - u.rect.xy + u.src));
  return textureLoad(tex, p, 0);
}`;

const SLOT = 256;
const MAX_DRAWS = 512;

/**
 * The page's glass scene: DWM's glass for every window with glass, over
 * what is beneath it on the page. `desktop` is the `DomDesktop` (its
 * `windows()`), `mount` the element their canvases are in, its top-left the
 * screen's (0, 0). Redrawn on the next animation frame after anything
 * changed: a canvas painted, a window moved, restacked or resized.
 */
export function domGlassScene(desktop, mount) {
  const panes = new Set();
  const waiters = new Set();
  const uploads = new WeakMap(); // canvas -> { tex, version, w, h }
  let device = null, glass = null, pipes = null, layout = null, uniforms = null;
  let scene = null, format = null;
  let queued = false, last = '';
  const self = {
    ready: false,
    whenReady(fn) { if (!self.ready) waiters.add(fn); },
    pane() {
      const canvas = document.createElement('canvas');
      canvas.className = 'gex-glass';
      canvas.style.position = 'absolute';
      canvas.style.pointerEvents = 'none';
      const context = canvas.getContext('webgpu');
      context.configure({ device, format, alphaMode: 'premultiplied' });
      return { canvas, context, bounds: null, rects: [], win: null, overlay: () => null };
    },
    add(p) { panes.add(p); },
    drop(p) { panes.delete(p); },
    dirty() {
      if (queued || !self.ready) return;
      queued = true;
      requestAnimationFrame(() => { queued = false; draw(); });
    },
  };

  (async () => {
    const adapter = await navigator.gpu?.requestAdapter();
    if (!adapter) return;
    device = await adapter.requestDevice();
    format = navigator.gpu.getPreferredCanvasFormat();
    glass = new DwmGlass(device, 'rgba8unorm');
    const module = device.createShaderModule({ code: SCENE_WGSL });
    layout = device.createBindGroupLayout({
      entries: [
        { binding: 0, visibility: GPUShaderStage.VERTEX | GPUShaderStage.FRAGMENT, buffer: { type: 'uniform', hasDynamicOffset: true } },
        { binding: 1, visibility: GPUShaderStage.FRAGMENT, texture: {} },
      ],
    });
    const pl = device.createPipelineLayout({ bindGroupLayouts: [layout] });
    const premul = { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' };
    const pipe = (f, blend) => device.createRenderPipeline({
      layout: pl,
      vertex: { module, entryPoint: 'vs' },
      fragment: { module, entryPoint: 'fs', targets: [{ format: f, blend }] },
      primitive: { topology: 'triangle-strip' },
    });
    pipes = { over: pipe('rgba8unorm', { color: premul, alpha: premul }), copy: pipe(format, undefined) };
    uniforms = device.createBuffer({ size: SLOT * MAX_DRAWS, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
    self.ready = true;
    for (const fn of waiters) fn();
    waiters.clear();
    // Something may change without a sync (a window repainting under glass):
    // the canvases' versions are checked every frame while there is glass.
    const tick = () => { if (panes.size) self.dirty(); requestAnimationFrame(tick); };
    requestAnimationFrame(tick);
  })().catch((e) => console.warn('[dom-glass] no WebGPU for DWM glass; the CSS filter instead', e));

  const visible = (el) => el.isConnected && el.style.display !== 'none' && el.style.visibility !== 'hidden';
  const z = (el) => Number(el.style.zIndex) || 0;

  function draw() {
    if (!panes.size) return;
    const W = Math.max(1, mount.clientWidth), H = Math.max(1, mount.clientHeight);
    // Bottom to top: each window's canvas, its glass (under it) and its frame
    // (over it). Equal z-indices keep the document's order.
    const glassOf = new Map([...panes].map((p) => [p.win, p]));
    const wins = desktop.windows().filter((w) => w.canvas && visible(w.canvas) && w.canvas.width > 0)
      .sort((a, b) => z(a.canvas) - z(b.canvas)
        || (a.canvas.compareDocumentPosition(b.canvas) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1));
    const pos = (el) => [parseFloat(el.style.left) || 0, parseFloat(el.style.top) || 0];
    let sig = `${W}x${H}`;
    for (const w of wins) {
      const p = glassOf.get(w), o = p?.overlay();
      sig += `|${w.key}:${w.paints}:${pos(w.canvas)}:${z(w.canvas)}:${w.canvas.width}x${w.canvas.height}`;
      if (p) sig += `:${w.focused}:${JSON.stringify(w.glass)}:${p.bounds.x},${p.bounds.y}:${o ? o.version : '-'}`;
    }
    if (sig === last) return;
    last = sig;
    if (!scene || scene.width !== W || scene.height !== H) {
      scene?.destroy();
      scene = device.createTexture({
        size: [W, H], format: 'rgba8unorm',
        usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC | GPUTextureUsage.TEXTURE_BINDING,
      });
    }
    const upload = (cv) => {
      let u = uploads.get(cv);
      if (!u || u.w !== cv.width || u.h !== cv.height) {
        u?.tex.destroy();
        u = { tex: device.createTexture({ size: [cv.width, cv.height], format: 'rgba8unorm',
          usage: GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST | GPUTextureUsage.RENDER_ATTACHMENT }),
        version: -1, w: cv.width, h: cv.height, group: null };
        uploads.set(cv, u);
      }
      return u;
    };
    let n = 0;
    const data = new Float32Array(SLOT / 4);
    const u32 = new Uint32Array(data.buffer);
    const slot = (rect, viewport, src, mode) => {
      data.fill(0);
      data.set(rect, 0); data.set(viewport, 4); data.set(src, 6); u32[8] = mode;
      device.queue.writeBuffer(uniforms, n * SLOT, data);
      return n++ * SLOT;
    };
    const group = (tex) => device.createBindGroup({
      layout, entries: [{ binding: 0, resource: { buffer: uniforms, size: SLOT } }, { binding: 1, resource: tex.createView() }],
    });
    glass.frame();
    const enc = device.createCommandEncoder();
    let pass = enc.beginRenderPass({
      colorAttachments: [{ view: scene.createView(), loadOp: 'clear', storeOp: 'store', clearValue: { r: 0, g: 0, b: 0, a: 1 } }],
    });
    pass.setPipeline(pipes.over);
    const blit = (cv, x, y, version) => {
      if (n >= MAX_DRAWS - 1) return;
      const u = upload(cv);
      if (u.version !== version) {
        device.queue.copyExternalImageToTexture({ source: cv }, { texture: u.tex, premultipliedAlpha: true }, [cv.width, cv.height]);
        u.version = version;
      }
      u.group ??= group(u.tex);
      pass.setBindGroup(0, u.group, [slot([x, y, cv.width, cv.height], [W, H], [0, 0], 0)]);
      pass.draw(4);
    };
    for (const w of wins) {
      const [x, y] = pos(w.canvas);
      const p = glassOf.get(w);
      if (p && p.rects.length && n + 2 < MAX_DRAWS) {
        pass.end();
        const rects = dwmGlassClips(w.glass, w.width, w.height, x, y, p.rects).map((r) => {
          const x0 = Math.max(0, r.x), y0 = Math.max(0, r.y);
          return { ...r, x: x0, y: y0, w: Math.min(W, r.x + r.w) - x0, h: Math.min(H, r.y + r.h) - y0 };
        }).filter((r) => r.w > 0 && r.h > 0);
        glass.encode(enc, scene, W, H, rects, dwmGlassConstants(w.glass, colorizationOpacity(w)));
        // The pane: the glass's rectangles out of the scene, transparent
        // between them.
        const b = p.bounds;
        if (p.canvas.width !== b.w || p.canvas.height !== b.h) {
          p.canvas.width = Math.max(1, b.w);
          p.canvas.height = Math.max(1, b.h);
        }
        const out = enc.beginRenderPass({
          colorAttachments: [{ view: p.context.getCurrentTexture().createView(), loadOp: 'clear', storeOp: 'store', clearValue: { r: 0, g: 0, b: 0, a: 0 } }],
        });
        out.setPipeline(pipes.copy);
        const sg = group(scene);
        for (const r of p.rects) {
          const rx = r.x - b.x, ry = r.y - b.y;
          out.setBindGroup(0, sg, [slot([rx, ry, r.w, r.h], [p.canvas.width, p.canvas.height], [x + r.x, y + r.y], 1)]);
          out.draw(4);
        }
        out.end();
        pass = enc.beginRenderPass({ colorAttachments: [{ view: scene.createView(), loadOp: 'load', storeOp: 'store' }] });
        pass.setPipeline(pipes.over);
      }
      // the frame (under the client area), then the window
      const o = p?.overlay();
      if (o && visible(o)) blit(o, ...pos(o), o.version);
      blit(w.canvas, x, y, w.paints);
    }
    pass.end();
    device.queue.submit([enc.finish()]);
  }

  return self;
}

/** DWM's COLORIZATIONOPACITY for `win`'s frame (0..1), as the compositor's
 *  #colorizationOpacity: the theme's TOPFRAME, active or inactive. */
function colorizationOpacity(win) {
  const active = (win.glass?.frame & 2) !== 0 || (!(win.glass?.frame & 1) && win.focused === true);
  const q = aeroAtlas()?.parts.get(`TOPFRAME:${active ? 1 : 2}`);
  const v = q?.properties?.COLORIZATIONOPACITY;
  return (typeof v === 'number' ? v : active ? 100 : 40) / 100;
}
