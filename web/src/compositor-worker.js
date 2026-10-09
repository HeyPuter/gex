import { requestGexDevice, textureCanvas } from './gpu-device.js';
// gex-wayland's output: a real compositor, in a worker, on WebGPU (the device Mesa uses).
//
// Every guest process publishes descriptors for its surfaces into a shared
// arena (see `wl-bridge.mjs`); this reads them once a frame, uploads the rows
// that changed straight out of the guest's own memory, and draws the scene --
// windows with server-side decorations, their popups and subsurfaces above
// them, and the client's cursor on top. Input goes the other way: the hit test
// happens here, because this is the only place that knows where the windows
// are.
//
// What does NOT happen here is a copy of a frame. A `wl_shm` buffer is guest
// memory, the guest's memory is a SharedArrayBuffer, and `texSubImage2D` reads
// it in place. An OpenGL client's frame comes from Mesa in this worker (see
// startGl).

import {
  readScene,
  ackBlock,
  sendEvent,
  setOutputSize,
  ROLE_TOPLEVEL,
  ROLE_POPUP,
  ROLE_SUBSURFACE,
  ROLE_CURSOR,
  F_MOVE_REQUESTED,
  EV_MOTION,
  EV_BUTTON,
  EV_AXIS,
  EV_KEY,
  EV_MODS,
  EV_POINTER_CLEAR,
  EV_KEYBOARD_FOCUS,
  EV_CONFIGURE,
  EV_CLOSE,
  EV_DISMISS_POPUP,
  EV_MOVED,
} from '@gex/wl-bridge.mjs';

// -- decoration metrics -----------------------------------------------------
//
// A frame the compositor draws, not the client: this is what "server-side
// decorations" means, and a Qt or KDE app that sees the xdg-decoration
// protocol stops drawing its own.
const TITLE_H = 26;
const BORDER = 1;
const BUTTON = 18;
const BUTTON_GAP = 6;

const COL = {
  desktop: [0.07, 0.09, 0.12, 1],
  titleActive: [0.16, 0.42, 0.72, 1],
  titleIdle: [0.18, 0.20, 0.24, 1],
  border: [0.05, 0.06, 0.08, 1],
  close: [0.78, 0.28, 0.28, 1],
  closeIdle: [0.35, 0.30, 0.32, 1],
  shadow: [0, 0, 0, 0.35],
};

// -- WebGPU -------------------------------------------------------------------
//
// The scene is drawn with WebGPU on the device Mesa uses (gpu-device.js), so a
// GL window's frame is a texture here like any other window's.

const WGSL = /* wgsl */ `
struct U {
  rect: vec4f,      // x, y, w, h in output pixels
  texRect: vec4f,   // sub-rectangle of the texture to sample (0..1)
  color: vec4f,
  output: vec2f,    // output size in pixels
  mode: u32,        // 0 = texture, 1 = solid colour
  swizzle: u32,     // 1 = the texture holds BGRA bytes (wl_shm)
  opaque: u32,      // 1 = force alpha to 1 (XRGB has no alpha)
};
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var tex: texture_2d<f32>;
struct V { @builtin(position) pos: vec4f, @location(0) uv: vec2f };
// One unit quad, placed by a pixel rectangle: the corner comes from the
// vertex index, so a draw is a uniform slice and a call.
@vertex fn vs(@builtin(vertex_index) i: u32) -> V {
  let corner = vec2f(f32(i & 1u), f32((i >> 1u) & 1u));
  let px = u.rect.xy + corner * u.rect.zw;
  var o: V;
  o.pos = vec4f(px.x / u.output.x * 2.0 - 1.0, 1.0 - px.y / u.output.y * 2.0, 0.0, 1.0);
  o.uv = u.texRect.xy + corner * u.texRect.zw;
  return o;
}
@fragment fn fs(v: V) -> @location(0) vec4f {
  var c = textureSample(tex, samp, v.uv);
  if (u.mode == 1u) { return u.color; }
  if (u.swizzle == 1u) { c = c.bgra; }
  if (u.opaque == 1u) { c.a = 1.0; }
  return c;
}`;

/** Bytes between two draws' uniforms: the minimum dynamic-offset alignment. */
const SLOT = 256;
const MAX_DRAWS = 1024;

/** A texture this compositor samples and writes: a window's pixels, a title. */
function gpuTexture(device, w, h) {
  return device.createTexture({
    size: [Math.max(1, w), Math.max(1, h)],
    format: 'rgba8unorm',
    usage: GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST | GPUTextureUsage.RENDER_ATTACHMENT,
  });
}

/**
 * How the scene reaches the output canvas: 'canvas' (a WebGPU canvas
 * context, the scene copied into it) or 'readback' (read back into a 2D
 * canvas).
 *
 * Readback is for software WebGPU (SwiftShader, the fallback adapter), which is
 * what headless test runs get: there Chromium has no shared-image backing for
 * a WebGPU canvas, and the first `getCurrentTexture` of *any* WebGPU canvas
 * loses every device in the renderer -- Mesa's included -- with "destroyed".
 * It cannot be probed for without that loss, so it is decided by the adapter.
 * `?present=canvas|readback` (the page's query, forwarded) overrides.
 */
let presentOverride = null;
function presentMode(adapter) {
  if (presentOverride === 'canvas' || presentOverride === 'readback') return presentOverride;
  const info = adapter?.info ?? {};
  return info.isFallbackAdapter || info.architecture === 'swiftshader' ? 'readback' : 'canvas';
}

class Painter {
  constructor(device, context, format) {
    this.device = device;
    this.context = context;
    this.format = format;
    const module = device.createShaderModule({ code: WGSL });
    this.layout = device.createBindGroupLayout({
      entries: [
        { binding: 0, visibility: GPUShaderStage.VERTEX | GPUShaderStage.FRAGMENT, buffer: { type: 'uniform', hasDynamicOffset: true } },
        { binding: 1, visibility: GPUShaderStage.FRAGMENT, sampler: {} },
        { binding: 2, visibility: GPUShaderStage.FRAGMENT, texture: {} },
      ],
    });
    const blend = { srcFactor: 'src-alpha', dstFactor: 'one-minus-src-alpha', operation: 'add' };
    this.pipeline = device.createRenderPipeline({
      layout: device.createPipelineLayout({ bindGroupLayouts: [this.layout] }),
      vertex: { module, entryPoint: 'vs' },
      fragment: { module, entryPoint: 'fs', targets: [{ format, blend: { color: blend, alpha: blend } }] },
      primitive: { topology: 'triangle-strip' },
    });
    // Linear and clamped, whatever the texture: the guest's GL frames are
    // textures Mesa made, and sampling must not depend on their state.
    this.sampler = device.createSampler({ minFilter: 'linear', magFilter: 'linear' });
    this.uniforms = device.createBuffer({ size: SLOT * MAX_DRAWS, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
    this.data = new ArrayBuffer(SLOT * MAX_DRAWS);
    this.f32 = new Float32Array(this.data);
    this.u32 = new Uint32Array(this.data);
    this.white = gpuTexture(device, 1, 1);
    device.queue.writeTexture({ texture: this.white }, new Uint8Array([255, 255, 255, 255]), { bytesPerRow: 4 }, [1, 1]);
    this.groups = new WeakMap();
    this.scene = null;
  }

  #group(tex) {
    let g = this.groups.get(tex);
    if (!g) {
      g = this.device.createBindGroup({
        layout: this.layout,
        entries: [
          { binding: 0, resource: { buffer: this.uniforms, size: 80 } },
          { binding: 1, resource: this.sampler },
          { binding: 2, resource: tex.createView() },
        ],
      });
      this.groups.set(tex, g);
    }
    return g;
  }

  begin(w, h) {
    // The scene is drawn into a texture of its own and copied to the canvas:
    // `describe` can read it back after the canvas texture has been presented.
    if (!this.scene || this.scene.width !== w || this.scene.height !== h) {
      this.scene?.destroy();
      this.scene = this.device.createTexture({
        size: [w, h],
        format: this.format,
        usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC,
      });
    }
    this.w = w;
    this.h = h;
    this.n = 0;
    this.encoder = this.device.createCommandEncoder();
    this.pass = this.encoder.beginRenderPass({
      colorAttachments: [{ view: this.scene.createView(), loadOp: 'clear', storeOp: 'store',
        clearValue: { r: COL.desktop[0], g: COL.desktop[1], b: COL.desktop[2], a: 1 } }],
    });
    this.pass.setPipeline(this.pipeline);
    return true;
  }

  #draw(x, y, w, h, tex, mode, color, swizzle, opaque, uv) {
    if (this.n >= MAX_DRAWS) return;
    const o = (this.n * SLOT) / 4;
    const f = this.f32;
    f[o] = x; f[o + 1] = y; f[o + 2] = w; f[o + 3] = h;
    f[o + 4] = uv[0]; f[o + 5] = uv[1]; f[o + 6] = uv[2]; f[o + 7] = uv[3];
    f[o + 8] = color[0]; f[o + 9] = color[1]; f[o + 10] = color[2]; f[o + 11] = color[3];
    f[o + 12] = this.w; f[o + 13] = this.h;
    this.u32[o + 14] = mode; this.u32[o + 15] = swizzle; this.u32[o + 16] = opaque;
    this.pass.setBindGroup(0, this.#group(tex), [this.n * SLOT]);
    this.pass.draw(4);
    this.n++;
  }

  solid(x, y, w, h, color) {
    this.#draw(x, y, w, h, this.white, 1, color, 0, 0, [0, 0, 1, 1]);
  }

  textured(x, y, w, h, tex, { swizzle = 0, opaque = 0, uv = [0, 0, 1, 1] } = {}) {
    if (!tex) return;
    this.#draw(x, y, w, h, tex, 0, [0, 0, 0, 0], swizzle, opaque, uv);
  }

  end() {
    this.pass.end();
    if (this.n) this.device.queue.writeBuffer(this.uniforms, 0, this.data, 0, this.n * SLOT);
    if (this.context) {
      const out = this.context.getCurrentTexture();
      this.encoder.copyTextureToTexture({ texture: this.scene }, { texture: out }, [this.w, this.h]);
    }
    this.device.queue.submit([this.encoder.finish()]);
    this.pass = null;
    this.encoder = null;
    if (this.ctx2d) this.#show();
  }

  /** Direct present: `tex` (a GL frame) is the whole output, `w` x `h`, drawn
   *  straight into the canvas's texture (or, reading back, the scene). One
   *  draw, no scene, no decorations, no cursor. */
  directFrame(tex, w, h) {
    this.w = w;
    this.h = h;
    this.n = 0;
    this.encoder = this.device.createCommandEncoder();
    let target;
    if (this.context) {
      target = this.context.getCurrentTexture();
    } else {
      if (!this.scene || this.scene.width !== w || this.scene.height !== h) {
        this.scene?.destroy();
        this.scene = this.device.createTexture({ size: [w, h], format: this.format,
          usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC });
      }
      target = this.scene;
    }
    this.pass = this.encoder.beginRenderPass({
      colorAttachments: [{ view: target.createView(), loadOp: 'clear', storeOp: 'store', clearValue: { r: 0, g: 0, b: 0, a: 1 } }],
    });
    this.pass.setPipeline(this.pipeline);
    this.textured(0, 0, w, h, tex, { opaque: 1 });
    this.pass.end();
    this.device.queue.writeBuffer(this.uniforms, 0, this.data, 0, this.n * SLOT);
    this.device.queue.submit([this.encoder.finish()]);
    this.pass = null;
    this.encoder = null;
    if (this.ctx2d) this.#show();
  }

  /* The readback output (see `presentMode`): the scene read back and put
   * into a 2D canvas. One read in flight; a frame drawn meanwhile is read
   * when it finishes, so the last frame drawn is always the one shown. */
  #reading = false;
  #again = false;

  #show() {
    if (this.#reading) {
      this.#again = true;
      return;
    }
    this.#reading = true;
    this.#again = false;
    this.read().then((r) => {
      if (r && r.w === this.ctx2d.canvas.width && r.h === this.ctx2d.canvas.height) {
        this.ctx2d.putImageData(new ImageData(new Uint8ClampedArray(r.px.buffer, 0, r.w * r.h * 4), r.w, r.h), 0, 0);
      }
    }).catch(() => {}).finally(() => {
      this.#reading = false;
      if (this.#again) this.#show();
    });
  }

  /** The last scene's pixels (`format` order), for tests. */
  async read() {
    if (!this.scene) return null;
    const w = this.scene.width, h = this.scene.height;
    const row = Math.ceil((w * 4) / 256) * 256;
    const buf = this.device.createBuffer({ size: row * h, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ });
    const enc = this.device.createCommandEncoder();
    enc.copyTextureToBuffer({ texture: this.scene }, { buffer: buf, bytesPerRow: row }, [w, h]);
    this.device.queue.submit([enc.finish()]);
    await buf.mapAsync(GPUMapMode.READ);
    const src = new Uint8Array(buf.getMappedRange());
    const px = new Uint8Array(w * h * 4);
    for (let y = 0; y < h; y++) px.set(src.subarray(y * row, y * row + w * 4), y * w * 4);
    buf.unmap();
    buf.destroy();
    return { w, h, px };
  }
}

/** A 2D canvas into a texture: titles, the fallback cursor. */
function canvasTexture(device, cv) {
  const tex = gpuTexture(device, cv.width, cv.height);
  device.queue.copyExternalImageToTexture({ source: cv }, { texture: tex, premultipliedAlpha: false }, [cv.width, cv.height]);
  return tex;
}

// -- text, for titlebars ----------------------------------------------------
//
// A titlebar needs a string drawn. A 2D canvas draws it, and each window's
// title becomes a small texture, remade only when the title changes -- which
// for a running app is almost never.
class TextCache {
  #device;
  #entries = new Map();

  constructor(device) {
    this.#device = device;
  }

  get(text) {
    let e = this.#entries.get(text);
    if (e) return e;
    const scale = 2; // drawn at 2x and minified, so the glyphs stay crisp
    const cv = new OffscreenCanvas(1, 1);
    let ctx = cv.getContext('2d');
    ctx.font = `${13 * scale}px system-ui, sans-serif`;
    const width = Math.max(1, Math.ceil(ctx.measureText(text).width));
    cv.width = width;
    cv.height = 18 * scale;
    ctx = cv.getContext('2d');
    ctx.font = `${13 * scale}px system-ui, sans-serif`;
    ctx.textBaseline = 'middle';
    ctx.fillStyle = 'white';
    ctx.fillText(text, 0, cv.height / 2);
    e = { tex: canvasTexture(this.#device, cv), w: cv.width / scale, h: cv.height / scale };
    // A cache with no bound is a leak; titles change rarely and a session has
    // a handful of windows, so a small cap is plenty.
    if (this.#entries.size > 64) {
      for (const [k, v] of this.#entries) {
        v.tex.destroy();
        this.#entries.delete(k);
        if (this.#entries.size <= 32) break;
      }
    }
    this.#entries.set(text, e);
    return e;
  }
}

/** The pointer image, drawn when no client has set one of its own. */
function defaultCursor(device) {
  const s = 2;
  const cv = new OffscreenCanvas(12 * s, 19 * s);
  const ctx = cv.getContext('2d');
  ctx.scale(s, s);
  ctx.beginPath();
  ctx.moveTo(0.5, 0.5);
  ctx.lineTo(0.5, 15.5);
  ctx.lineTo(4.5, 11.5);
  ctx.lineTo(7, 17.5);
  ctx.lineTo(9.5, 16.5);
  ctx.lineTo(7, 10.5);
  ctx.lineTo(11.5, 10.5);
  ctx.closePath();
  ctx.fillStyle = 'white';
  ctx.fill();
  ctx.strokeStyle = 'black';
  ctx.lineWidth = 1;
  ctx.stroke();
  return { tex: canvasTexture(device, cv), w: 12, h: 19 };
}

// -- the compositor ---------------------------------------------------------

const key = (s) => `${s.token}:${s.client}:${s.surface}`;

class Compositor {
  #device;
  #painter;
  #text;
  #cursorFallback;
  #channel;
  /** Guest memories to read pixels from, by the token of the process. */
  #memories = new Map();
  /** Per surface: its GL texture and what we last uploaded. */
  #textures = new Map();
  /** Window state the compositor owns: position, stacking, focus. */
  #windows = new Map();
  #order = [];
  #glSaid = '';
  #glSaidAt = 0;
  #glDraws = 0;
  #focus = null;
  #pointer = { x: 0, y: 0, over: null, buttons: 0 };
  #drag = null;
  #outW = 0;
  #outH = 0;
  #frames = 0;
  #placed = 0;
  #stats = null;
  /** A view onto guest memory can be refused by writeTexture; then we stage a copy. */
  #stagingOk = null;
  #staging = new Uint8Array(0);
  /** A short history of what the scene did, for when a picture is not enough. */
  #log = [];
  /**
   * Where a window was, by key, kept after it goes away.
   *
   * A client that hides and shows a window (an `attach(NULL)`, or an X window
   * remapped) would otherwise come back at the next cascade position, walking
   * across the screen every time it blinks.
   */
  #remembered = new Map();

  /** `gpu`: the page's shared WebGPU device (gpu-device.js), Mesa's too. */
  constructor(canvas, channel, stats, gpu) {
    this.#channel = channel;
    this.#stats = stats;
    const device = gpu.device;
    this.#device = device;
    this.#canvas = canvas;
    if (presentMode(gpu.adapter) === 'canvas') {
      const context = canvas.getContext('webgpu');
      if (!context) throw new Error('no webgpu context for the compositor');
      const format = navigator.gpu.getPreferredCanvasFormat();
      context.configure({ device, format, alphaMode: 'opaque', usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_DST });
      this.#painter = new Painter(device, context, format);
    } else {
      // RGBA, so a read row is ImageData as it stands.
      this.#painter = new Painter(device, null, 'rgba8unorm');
      this.#painter.ctx2d = canvas.getContext('2d', { alpha: false });
    }
    this.#text = new TextCache(device);
    this.#cursorFallback = defaultCursor(device);
    this.resize(canvas.width, canvas.height);
  }

  #canvas;

  resize(w, h) {
    this.#outW = Math.max(1, w | 0);
    this.#outH = Math.max(1, h | 0);
    if (!this.#direct && this.#canvas.width !== this.#outW) this.#canvas.width = this.#outW;
    if (!this.#direct && this.#canvas.height !== this.#outH) this.#canvas.height = this.#outH;
    setOutputSize(this.#channel, this.#outW, this.#outH);
    // Every mapped toplevel is told the new size: a compositor that resizes its
    // output and says nothing leaves every window the wrong shape.
    for (const [k, win] of this.#windows) {
      if (win.role !== ROLE_TOPLEVEL) continue;
      this.#configure(win, this.#focus === k);
    }
  }

  /**
   * A guest process's memory, where its surfaces' pixels are.
   *
   * The *memory*, not a view of it: a wasm memory that grows hands out a new
   * `SharedArrayBuffer` object, and a view taken once keeps the old length
   * forever. Every process grows past its initial 80 MB as it runs, so a
   * cached view stopped covering the pools its client allocated later -- the
   * first window of a session appeared and the second never did, because its
   * buffer was "past the end of memory".
   */
  addMemory(token, memory) {
    this.#memories.set(token, { memory, view: new Uint8Array(memory.buffer) });
  }

  /** The current view of a process's memory, re-derived if it has grown. */
  #memoryOf(token) {
    const held = this.#memories.get(token);
    if (!held) return null;
    if (held.view.buffer !== held.memory.buffer) {
      held.view = new Uint8Array(held.memory.buffer);
    }
    return held.view;
  }

  dropMemory(token) {
    this.#memories.delete(token);
  }

  /** The content size a window should have to fit the output with its frame. */
  #fit(win) {
    const maxW = Math.max(160, this.#outW - 2 * BORDER);
    const maxH = Math.max(120, this.#outH - TITLE_H - 2 * BORDER);
    return { w: Math.min(win.width || maxW, maxW), h: Math.min(win.height || maxH, maxH) };
  }

  // -- the scene ----------------------------------------------------------

  #reconcile() {
    const { surfaces, blocks } = readScene(this.#channel);
    const live = new Set();
    const byKey = new Map();
    for (const s of surfaces) {
      const k = key(s);
      live.add(k);
      byKey.set(k, s);
      let win = this.#windows.get(k);
      if (!win) {
        win = {
          key: k,
          token: s.token,
          block: s.block,
          client: s.client,
          surface: s.surface,
          role: s.role,
          x: 0,
          y: 0,
          width: s.width,
          height: s.height,
          title: s.title,
          serial: 0,
        };
        this.#note(`+ ${k} role=${s.role} ${s.width}x${s.height}`);
        // In the map before it is placed or focused: `#focusWindow` and
        // `#configure` look the window up by key, and called with it still
        // missing they set the focus variable and returned without sending
        // anything. Every window came up without the keyboard -- which for an
        // X client meant every key was dropped for want of a focused surface.
        this.#windows.set(k, win);
        const seen = this.#remembered.get(k);
        if (s.role === ROLE_TOPLEVEL && seen) {
          win.x = seen.x;
          win.y = seen.y;
          this.#order.push(k);
          this.#focusWindow(k);
          this.#reportPosition(win);
        } else if (s.role === ROLE_TOPLEVEL) {
          // Cascade, so two windows that appear together are both reachable.
          const step = 28;
          const n = this.#placed++;
          const roomX = Math.max(1, this.#outW - s.width - BORDER);
          const roomY = Math.max(1, this.#outH - s.height - TITLE_H - BORDER);
          win.x = BORDER + ((n * step) % roomX);
          win.y = TITLE_H + BORDER + ((n * step) % roomY);
          this.#order.push(k);
          this.#focusWindow(k);
          this.#reportPosition(win);
        }
      }
      win.role = s.role;
      win.width = s.width;
      win.height = s.height;
      win.title = s.title;
      win.parent = s.parent;
      win.relX = s.relX;
      win.relY = s.relY;
      win.info = s;
      // A client that asked to be dragged (a GTK titlebar) starts a drag as if
      // the pointer had grabbed our own frame.
      if (s.flags & F_MOVE_REQUESTED && s.role === ROLE_TOPLEVEL) {
        this.#drag = { key: k, dx: this.#pointer.x - win.x, dy: this.#pointer.y - win.y };
      }
    }
    // Gone: drop the window, its texture, and its place in the stack.
    for (const k of [...this.#windows.keys()]) {
      if (live.has(k)) continue;
      const tex = this.#textures.get(k);
      if (tex) {
        tex.tex.destroy();
        this.#textures.delete(k);
      }
      this.#note(`- ${k}`);
      const gone = this.#windows.get(k);
      if (gone?.role === ROLE_TOPLEVEL) {
        this.#remembered.set(k, { x: gone.x, y: gone.y });
        // A session that opens and closes a hundred windows should not
        // remember all of them.
        if (this.#remembered.size > 128) {
          for (const old of this.#remembered.keys()) {
            this.#remembered.delete(old);
            if (this.#remembered.size <= 64) break;
          }
        }
      }
      this.#windows.delete(k);
      this.#order = this.#order.filter((o) => o !== k);
      // The keyboard goes to whatever is now on top, and that window is told
      // so -- a focus that is only a variable here leaves every client
      // believing it is inactive.
      if (this.#focus === k) {
        this.#focus = null;
        const next = this.#order[this.#order.length - 1] ?? null;
        if (next) this.#focusWindow(next);
      }
      if (this.#pointer.over === k) this.#pointer.over = null;
    }
    return { byKey, blocks };
  }

  /**
   * Say where a window now is. Only an X11 window cares -- its client places
   * menus in X coordinates, so the X geometry has to follow the layout -- and
   * the shim ignores it for everything else.
   */
  #reportPosition(win) {
    sendEvent(this.#channel, win.block, EV_MOVED, win.x, win.y, 0, 0, win.client, win.surface);
  }

  #note(line) {
    this.#log.push(`${this.#frames} ${line}`);
    if (this.#log.length > 64) this.#log.splice(0, 32);
  }

  /** Absolute position of a surface: popups and subsurfaces follow a parent. */
  #placeOf(win) {
    if (win.role === ROLE_TOPLEVEL) return { x: win.x, y: win.y };
    const parentKey = `${win.token}:${win.client}:${win.parent}`;
    const parent = win.parent ? this.#windows.get(parentKey) : null;
    // A popup with no parent is an X11 override-redirect window -- a menu the
    // client placed itself, in coordinates that are the output's because the
    // window manager keeps the X geometry and the layout in step.
    if (!parent) return { x: win.relX, y: win.relY };
    const at = this.#placeOf(parent);
    return { x: at.x + win.relX, y: at.y + win.relY };
  }

  /** Popups with no parent: X11 menus, drawn and hit-tested above everything. */
  #floating() {
    const out = [];
    for (const [k, win] of this.#windows) {
      if (win.role !== ROLE_POPUP || win.parent) continue;
      out.push(k);
    }
    return out;
  }

  // -- uploads ------------------------------------------------------------

  #texture(k, w, h) {
    let t = this.#textures.get(k);
    if (t && (t.w !== w || t.h !== h)) {
      t.tex.destroy();
      t = null;
    }
    if (!t) {
      t = { tex: gpuTexture(this.#device, w, h), w, h, serial: 0 };
      this.#textures.set(k, t);
    }
    return t;
  }

  /**
   * Upload what changed of one surface, reading the guest's memory in place.
   *
   * The rows are contiguous in the guest at `stride` bytes each, so a damage
   * rectangle is one writeTexture with the guest's stride as `bytesPerRow`,
   * straight out of the guest's (shared) memory.
   */
  #upload(k, s) {
    // Nothing to read: `addr` names no buffer, which is the case for a window
    // whose pixels are the host's GL and for one the window manager published
    // before the X server had anything to commit. `external` on its own is
    // not a reason to skip a buffer that *is* there -- it says the pixels may
    // be somewhere else, not that this surface never has any -- and an X
    // window can have both: a background the server painted, with the GL
    // frame drawn over it.
    if (!s.addr) return true;
    const mem = this.#memoryOf(s.token);
    if (!mem) return false;
    const t = this.#texture(k, s.width, s.height);
    const full = s.damage.w <= 0 || s.damage.h <= 0 || t.serial === 0 || s.serial !== t.serial + 1;
    let x = 0;
    let y = 0;
    let w = s.width;
    let h = s.height;
    if (!full) {
      x = Math.max(0, Math.min(s.damage.x, s.width));
      y = Math.max(0, Math.min(s.damage.y, s.height));
      w = Math.max(0, Math.min(s.damage.w, s.width - x));
      h = Math.max(0, Math.min(s.damage.h, s.height - y));
    }
    if (w <= 0 || h <= 0) {
      t.serial = s.serial;
      return true;
    }
    const start = s.addr + y * s.stride + x * 4;
    const need = (h - 1) * s.stride + w * 4;
    if (start + need > mem.length) {
      // Past the end of what this process's memory holds. Worth saying: it
      // means the descriptor and the memory disagree, which is either a
      // client's bad buffer or a stale view of a grown memory.
      this.#note(`! ${k} buffer at ${start}+${need} past ${mem.length}`);
      return false;
    }
    let view;
    if (this.#stagingOk === false) {
      // A SharedArrayBuffer view was refused, so one staging copy it is.
      if (this.#staging.length < need) this.#staging = new Uint8Array(need);
      this.#staging.set(mem.subarray(start, start + need));
      view = this.#staging.subarray(0, need);
    } else {
      view = mem.subarray(start, start + need);
    }
    try {
      this.#device.queue.writeTexture({ texture: t.tex, origin: { x, y } }, view,
        { offset: 0, bytesPerRow: s.stride, rowsPerImage: h }, { width: w, height: h });
      if (this.#stagingOk === null) this.#stagingOk = true;
    } catch (e) {
      if (this.#stagingOk === null) {
        // Once, and then never again: fall back and redo this upload.
        this.#stagingOk = false;
        return this.#upload(k, s);
      }
      throw e;
    }
    t.serial = s.serial;
    return true;
  }

  // -- drawing ------------------------------------------------------------

  /** The frame around a window's content, and the content's place inside it. */
  #frameOf(win) {
    const at = this.#placeOf(win);
    return {
      x: at.x,
      y: at.y,
      w: win.width,
      h: win.height,
      frameX: at.x - BORDER,
      frameY: at.y - TITLE_H,
      frameW: win.width + 2 * BORDER,
      frameH: win.height + TITLE_H + BORDER,
    };
  }

  #closeButton(win) {
    const f = this.#frameOf(win);
    return {
      x: f.frameX + f.frameW - BUTTON - BUTTON_GAP,
      y: f.frameY + (TITLE_H - BUTTON) / 2,
      w: BUTTON,
      h: BUTTON,
    };
  }

  #drawDecoration(win, active) {
    const p = this.#painter;
    const f = this.#frameOf(win);
    p.solid(f.frameX + 3, f.frameY + 4, f.frameW, f.frameH, COL.shadow);
    p.solid(f.frameX, f.frameY, f.frameW, f.frameH, COL.border);
    p.solid(f.frameX, f.frameY, f.frameW, TITLE_H, active ? COL.titleActive : COL.titleIdle);
    const label = win.title || 'window';
    const t = this.#text.get(label);
    const room = f.frameW - BUTTON - 2 * BUTTON_GAP - 10;
    const w = Math.min(t.w, Math.max(0, room));
    if (w > 4) {
      p.textured(f.frameX + 8, f.frameY + (TITLE_H - t.h) / 2, w, t.h, t.tex, {
        uv: [0, 0, w / t.w, 1],
      });
    }
    const b = this.#closeButton(win);
    p.solid(b.x, b.y, b.w, b.h, active ? COL.close : COL.closeIdle);
    // The cross, from the same text cache as the title: one texture and one
    // draw, rather than a quad per pixel of two diagonals.
    const x = this.#text.get('\u2715');
    const size = Math.min(b.w, b.h) - 6;
    p.textured(b.x + (b.w - size) / 2, b.y + (b.h - size) / 2, size, size, x.tex);
  }

  #drawSurface(k, win) {
    const f = this.#frameOf(win);
    // A window whose pixels are the host's GL, not the guest's memory: Mesa's
    // frame, a texture on this device (gpu-device.js textureCanvas).
    const gl = this.#glSurface(win);
    const t = this.#textures.get(k);
    // A frame for part of the window only (a Windows program's client area,
    // under a title bar and borders it drew itself): the window's own pixels
    // first, then the frame over its rectangle.
    const r = gl && win.info?.glRect;
    if (gl) {
      this.#glDraws++;
      if (r) {
        if (t) this.#painter.textured(f.x, f.y, win.width, win.height, t.tex, { swizzle: 1, opaque: win.info?.format === 1 ? 1 : 0 });
        this.#painter.textured(f.x + r.x, f.y + r.y, r.w, r.h, gl.tex, { opaque: 1 });
      } else {
        this.#painter.textured(f.x, f.y, win.width, win.height, gl.tex, { opaque: 1 });
      }
      return;
    }
    if (!t) return;
    this.#painter.textured(f.x, f.y, win.width, win.height, t.tex, {
      swizzle: 1,
      opaque: win.info?.format === 1 ? 1 : 0,
    });
  }

  /** The GL frame that belongs in this window, if one does.
   *
   * A Wayland client names its own surface and the process that published it.
   * An X client names a drawable instead, and the window manager is what knows
   * which surface that window became -- see `xid` on the record. */
  #glSurface(win) {
    const a = glAttachment();
    // Said once per change, because a GL window that stays black has three
    // possible reasons -- no attachment, no match, no frame -- and they are
    // indistinguishable from the outside.
    const f0 = glFrame();
    // Every two seconds rather than on change: what matters is whether frames
    // keep arriving, and a state that never changes says nothing about that.
    const now = performance.now();
    const list = glSurfaceList();
    const stamp = now - this.#glSaidAt > 2000
      ? `${list.length} surface(s) `
        + list.map((s) => `${s.kind}:${s.id}@${s.token}/${s.frame}`).join(' ')
        + ` f${f0?.serial ?? 0}`
      : this.#glSaid;
    if (stamp !== this.#glSaid) this.#glSaidAt = now;
    if (stamp !== this.#glSaid) {
      this.#glSaid = stamp;
      const f = glFrame();
      this.#note(`gl attachment ${stamp}, frame ${f ? `${f.w}x${f.h} #${f.serial}` : 'none'}`
        + `, window ${win.token}:${win.surface} xid ${win.info?.xid ?? 0}`
        + `, drawn ${this.#glDraws}`
        + `, device ${glDevice?.glState?.() ?? '?'}`);
    }
    // Per window, out of the device's list of them: see `glFrameFor`.
    const frame = glFrameFor(win);
    return frame?.tex ? frame : null;
  }

  /** Children of a surface, in the order they should be drawn (above it). */
  #childrenOf(k) {
    const win = this.#windows.get(k);
    if (!win) return [];
    const out = [];
    for (const [ck, cw] of this.#windows) {
      if (ck === k) continue;
      if (cw.role !== ROLE_POPUP && cw.role !== ROLE_SUBSURFACE) continue;
      if (`${cw.token}:${cw.client}:${cw.parent}` !== k) continue;
      out.push(ck);
    }
    return out;
  }

  #lastSig = null;
  #skipped = 0;

  /** What the output shows, as a string: equal strings, equal pixels (a GL
   *  window's frame counted by its presented serial). */
  #signature() {
    const parts = [this.#outW, this.#outH, this.#focus, this.#pointer.x, this.#pointer.y, this.#order.join(',')];
    for (const [k, win] of this.#windows) {
      const f = win.role === ROLE_TOPLEVEL ? this.#frameOf(win) : null;
      parts.push(k, win.role, win.width, win.height, win.relX, win.relY, win.title, win.parent,
        f ? `${f.x},${f.y},${f.frameW}` : '', this.#textures.get(k)?.serial ?? '-', JSON.stringify(win.info?.glRect ?? null),
        glFrameFor(win)?.serial ?? '');
    }
    return parts.join('|');
  }

  // -- direct present -------------------------------------------------------
  //
  // An ejected single-window GL program (`?direct=1`): while its one toplevel
  // is a GL window and nothing else is on screen, the GL frame is the output
  // canvas, at the frame's own size (the page fits it to the viewport), and
  // the page's pointer coordinates are the frame's pixels. No scene, no
  // decorations, no wl_shm uploads, no hit testing beyond mapping into that
  // window. A second toplevel, a popup or a window without GL (a message box)
  // brings the compositor back, at once, for as long as it is there.
  #directWanted = false;
  #direct = null; // { k, w, h, ox, oy } while presenting directly
  #directSerial = -1;
  #cursorSerial = -1;

  setDirect(on) {
    this.#directWanted = !!on;
  }

  #directTarget() {
    if (!this.#directWanted) return null;
    let top = null;
    for (const [k, win] of this.#windows) {
      if (win.role === ROLE_CURSOR) continue;
      if (win.role !== ROLE_TOPLEVEL) return null; // a popup or subsurface: composite
      if (top) return null;                         // a second toplevel: composite
      top = [k, win];
    }
    if (!top) return null;
    const [k, win] = top;
    const gl = this.#glSurface(win);
    if (!gl) return null;
    const f = this.#frameOf(win);
    const r = win.info?.glRect;
    return { k, win, gl, w: gl.w, h: gl.h, ox: f.x + (r ? r.x : 0), oy: f.y + (r ? r.y : 0) };
  }

  #frameDirect(d, byKey, blocks) {
    if (!this.#direct || this.#direct.w !== d.w || this.#direct.h !== d.h) {
      this.#canvas.width = d.w;
      this.#canvas.height = d.h;
      this.#directSerial = -1;
      this.#cursorSerial = -1;
      postMessage({ kind: 'direct', w: d.w, h: d.h });
      this.#note(`direct present ${d.w}x${d.h} (${d.k})`);
    }
    this.#direct = { k: d.k, w: d.w, h: d.h, ox: d.ox, oy: d.oy };
    this.#directCursor(byKey);
    for (const b of blocks) ackBlock(this.#channel, b.block, b.gen);
    if (d.gl.serial === this.#directSerial) {
      this.#skipped++;
      return;
    }
    this.#directSerial = d.gl.serial;
    this.#painter.directFrame(d.gl.tex, d.w, d.h);
    this.#frames++;
    if (this.#stats) {
      Atomics.store(this.#stats, 0, this.#frames);
      Atomics.store(this.#stats, 1, this.#windows.size);
    }
  }

  #leaveDirect() {
    this.#direct = null;
    this.#canvas.width = this.#outW;
    this.#canvas.height = this.#outH;
    this.#lastSig = null;
    postMessage({ kind: 'direct', w: 0, h: 0 });
    this.#note('direct present off: compositing');
  }

  /* The client's cursor as the page's CSS cursor: none when it hid it (a
   * game drawing its own), its image when it set one. Read from the guest's
   * buffer when its serial moves. */
  #directCursor(byKey) {
    let found = null;
    for (const [k, win] of this.#windows) if (win.role === ROLE_CURSOR) { found = [k, win]; break; }
    if (!found) {
      if (this.#cursorSerial !== 0) postMessage({ kind: 'cursor', css: 'none' });
      this.#cursorSerial = 0;
      return;
    }
    const [k, win] = found;
    const s = byKey.get(k);
    if (!s || !s.addr || s.serial === this.#cursorSerial) return;
    this.#cursorSerial = s.serial;
    const mem = this.#memoryOf(s.token);
    if (!mem || s.width <= 0 || s.height <= 0 || s.width > 256 || s.height > 256) return;
    const px = new Uint8ClampedArray(s.width * s.height * 4);
    for (let y = 0; y < s.height; y++) {
      for (let x = 0; x < s.width; x++) {
        const i = s.addr + y * s.stride + x * 4;
        const o = (y * s.width + x) * 4;
        px[o] = mem[i + 2]; px[o + 1] = mem[i + 1]; px[o + 2] = mem[i]; px[o + 3] = win.info?.format === 1 ? 255 : mem[i + 3];
      }
    }
    const cv = new OffscreenCanvas(s.width, s.height);
    cv.getContext('2d').putImageData(new ImageData(px, s.width, s.height), 0, 0);
    const hx = Math.max(0, win.relX | 0), hy = Math.max(0, win.relY | 0);
    cv.convertToBlob().then((blob) => {
      postMessage({ kind: 'cursor', css: `url(${URL.createObjectURL(blob)}) ${hx} ${hy}, default` });
    }).catch(() => {});
  }

  frame() {
    const { byKey, blocks } = this.#reconcile();
    const d = this.#directTarget();
    if (d) {
      this.#frameDirect(d, byKey, blocks);
      return;
    }
    if (this.#direct) this.#leaveDirect();
    // Upload every surface whose content moved on.
    for (const [k, s] of byKey) {
      const t = this.#textures.get(k);
      if (t && t.serial === s.serial) continue;
      this.#upload(k, s);
    }
    // Nothing on the output changed (no new GL frame either): draw nothing,
    // so the canvas presents no new frame. A presented frame is a full
    // readback where the browser composites in software (headless), and
    // wasted work everywhere else.
    const sig = this.#signature();
    if (sig !== null && sig === this.#lastSig) {
      for (const b of blocks) ackBlock(this.#channel, b.block, b.gen);
      this.#skipped++;
      return;
    }
    this.#lastSig = sig;
    // Nothing to draw into: see `Painter.begin`. The arena is still acked
    // below on the next frame that works, so a restored context catches up.
    if (this.#painter.begin(this.#outW, this.#outH) === false) {
      this.#lastSig = null;
      return;
    }
    // Windows bottom to top, each with its popups and subsurfaces above it.
    for (const k of this.#order) {
      const win = this.#windows.get(k);
      if (!win || win.role !== ROLE_TOPLEVEL) continue;
      this.#drawDecoration(win, this.#focus === k);
      this.#drawSurface(k, win);
      for (const ck of this.#childrenOf(k)) {
        const child = this.#windows.get(ck);
        if (child) this.#drawSurface(ck, child);
      }
    }
    // X11 menus and tooltips: no parent to follow, no frame, above everything.
    for (const k of this.#floating()) {
      const win = this.#windows.get(k);
      if (win) this.#drawSurface(k, win);
    }
    // The cursor the client asked for, at the pointer, offset by its hotspot.
    let drew = false;
    for (const [k, win] of this.#windows) {
      if (win.role !== ROLE_CURSOR) continue;
      const t = this.#textures.get(k);
      if (!t) continue;
      this.#painter.textured(
        this.#pointer.x - win.relX,
        this.#pointer.y - win.relY,
        win.width,
        win.height,
        t.tex,
        { swizzle: 1 },
      );
      drew = true;
      break;
    }
    if (!drew) {
      const c = this.#cursorFallback;
      this.#painter.textured(this.#pointer.x, this.#pointer.y, c.w, c.h, c.tex);
    }
    this.#painter.end();
    for (const b of blocks) ackBlock(this.#channel, b.block, b.gen);
    this.#frames++;
    if (this.#stats) {
      Atomics.store(this.#stats, 0, this.#frames);
      Atomics.store(this.#stats, 1, this.#windows.size);
    }
  }

  // -- input --------------------------------------------------------------

  /** The topmost surface at a point, and where in it. */
  #hit(x, y) {
    for (const k of this.#floating()) {
      const win = this.#windows.get(k);
      if (!win) continue;
      const at = this.#placeOf(win);
      if (x >= at.x && x < at.x + win.width && y >= at.y && y < at.y + win.height) {
        return { key: k, win, local: { x: x - at.x, y: y - at.y }, part: 'content' };
      }
    }
    for (let i = this.#order.length - 1; i >= 0; i--) {
      const k = this.#order[i];
      const win = this.#windows.get(k);
      if (!win) continue;
      // Popups first: a menu is above its window and takes the click.
      for (const ck of this.#childrenOf(k)) {
        const child = this.#windows.get(ck);
        if (!child) continue;
        const at = this.#placeOf(child);
        if (
          x >= at.x && x < at.x + child.width &&
          y >= at.y && y < at.y + child.height
        ) {
          return { key: ck, win: child, local: { x: x - at.x, y: y - at.y }, part: 'content' };
        }
      }
      const f = this.#frameOf(win);
      if (x >= f.x && x < f.x + win.width && y >= f.y && y < f.y + win.height) {
        return { key: k, win, local: { x: x - f.x, y: y - f.y }, part: 'content' };
      }
      if (
        x >= f.frameX && x < f.frameX + f.frameW &&
        y >= f.frameY && y < f.frameY + f.frameH
      ) {
        const b = this.#closeButton(win);
        const onClose = x >= b.x && x < b.x + b.w && y >= b.y && y < b.y + b.h;
        return { key: k, win, local: { x: 0, y: 0 }, part: onClose ? 'close' : 'title' };
      }
    }
    return null;
  }

  /**
   * Tell a window its size and whether it is active -- but only when one of
   * those actually changed.
   *
   * A configure is not free: a toolkit redraws for it, and Qt goes as far as
   * recreating its surface. Sending one per click left a session accumulating
   * stacked empty frames, one per redundant configure.
   */
  #configure(win, activated) {
    const fit = this.#fit(win);
    const want = `${fit.w}x${fit.h}:${activated ? 1 : 0}`;
    if (win.configuredAs === want) return;
    win.configuredAs = want;
    this.#note(`configure ${win.key} ${want}`);
    sendEvent(this.#channel, win.block, EV_CONFIGURE, fit.w, fit.h, 0, activated ? 1 : 0,
      win.client, win.surface);
  }

  #focusWindow(k) {
    if (this.#focus === k) return;
    this.#note(`focus ${k}`);
    const old = this.#focus ? this.#windows.get(this.#focus) : null;
    this.#focus = k;
    if (old) {
      sendEvent(this.#channel, old.block, EV_KEYBOARD_FOCUS, 0, 0, 0, 0, old.client, 0);
      this.#configure(old, false);
    }
    const win = k ? this.#windows.get(k) : null;
    if (!win) return;
    sendEvent(this.#channel, win.block, EV_KEYBOARD_FOCUS, 0, 0, 0, 0, win.client, win.surface);
    this.#configure(win, true);
  }

  #raise(k) {
    this.#order = this.#order.filter((o) => o !== k);
    this.#order.push(k);
  }

  /** Tell whoever owns the pointer that it left, when it moves off them. */
  #leave(k) {
    const win = k ? this.#windows.get(k) : null;
    if (!win) return;
    sendEvent(this.#channel, win.block, EV_POINTER_CLEAR, 0, 0, 0, 0, win.client, 0);
  }

  pointerMove(x, y, time) {
    // Direct present: the page's coordinates are the GL frame's pixels.
    if (this.#direct) {
      x = this.#direct.ox + Math.max(0, Math.min(this.#direct.w - 1, x));
      y = this.#direct.oy + Math.max(0, Math.min(this.#direct.h - 1, y));
    }
    this.#pointer.x = x;
    this.#pointer.y = y;
    if (this.#drag) {
      const win = this.#windows.get(this.#drag.key);
      if (win) {
        win.x = Math.round(x - this.#drag.dx);
        win.y = Math.round(y - this.#drag.dy);
        this.#reportPosition(win);
      }
      return;
    }
    const hit = this.#hit(x, y);
    const over = hit && hit.part === 'content' ? hit.key : null;
    if (this.#pointer.over && this.#pointer.over !== over) {
      const prev = this.#windows.get(this.#pointer.over);
      const next = over ? this.#windows.get(over) : null;
      // Only tell the client the pointer left when it left *that client*: a
      // move between two of its own surfaces is an enter, which the state
      // machine does itself.
      if (prev && (!next || next.token !== prev.token || next.client !== prev.client)) {
        this.#leave(this.#pointer.over);
      }
    }
    this.#pointer.over = over;
    if (!hit || hit.part !== 'content') return;
    sendEvent(this.#channel, hit.win.block, EV_MOTION, hit.local.x, hit.local.y, time, 0,
      hit.win.client, hit.win.surface);
  }

  pointerButton(button, pressed, time) {
    this.#pointer.buttons = pressed
      ? this.#pointer.buttons | 1
      : this.#pointer.buttons & ~1;
    if (!pressed && this.#drag) {
      this.#drag = null;
      return;
    }
    const hit = this.#hit(this.#pointer.x, this.#pointer.y);
    if (pressed) {
      // A click outside an open menu dismisses it, which is what the client's
      // implicit grab asked for.
      for (const [k, win] of this.#windows) {
        if (win.role !== ROLE_POPUP) continue;
        if (hit && hit.key === k) continue;
        sendEvent(this.#channel, win.block, EV_DISMISS_POPUP, 0, 0, 0, 0, win.client, win.surface);
      }
    }
    if (!hit) return;
    if (hit.part === 'close') {
      if (pressed) {
        sendEvent(this.#channel, hit.win.block, EV_CLOSE, 0, 0, 0, 0, hit.win.client,
          hit.win.surface);
      }
      return;
    }
    if (hit.part === 'title') {
      if (pressed) {
        this.#raise(hit.key);
        this.#focusWindow(hit.key);
        this.#drag = {
          key: hit.key,
          dx: this.#pointer.x - hit.win.x,
          dy: this.#pointer.y - hit.win.y,
        };
      }
      return;
    }
    if (pressed) {
      const top = hit.win.role === ROLE_TOPLEVEL
        ? hit.key
        : `${hit.win.token}:${hit.win.client}:${hit.win.parent}`;
      if (this.#windows.has(top)) {
        this.#raise(top);
        this.#focusWindow(top);
      }
      // The press lands where the pointer is, so the client's last motion has
      // to be this position -- a click without a motion in front of it would
      // otherwise arrive wherever the pointer was last seen by that client.
      sendEvent(this.#channel, hit.win.block, EV_MOTION, hit.local.x, hit.local.y, time, 0,
        hit.win.client, hit.win.surface);
    }
    sendEvent(this.#channel, hit.win.block, EV_BUTTON, button, pressed ? 1 : 0, time, 0,
      hit.win.client, hit.win.surface);
  }

  pointerAxis(axis, value, time) {
    const hit = this.#hit(this.#pointer.x, this.#pointer.y);
    if (!hit || hit.part !== 'content') return;
    sendEvent(this.#channel, hit.win.block, EV_AXIS, axis, value, time, 0, hit.win.client,
      hit.win.surface);
  }

  key(code, pressed, mods, time) {
    const win = this.#focus ? this.#windows.get(this.#focus) : null;
    if (!win) return;
    sendEvent(this.#channel, win.block, EV_MODS, mods, 0, 0, 0, win.client, 0);
    sendEvent(this.#channel, win.block, EV_KEY, code, pressed ? 1 : 0, time, 0, win.client,
      win.surface);
  }

  /** The output as it was last composited: `{ w, h, px }` (RGBA, top row first). */
  async readout() {
    try {
      return await this.#painter.read();
    } catch {
      return null;
    }
  }

  /**
   * What the harness and the demo UI read.
   *
   * `lit` is a real readback of the output: a compositor that maps windows,
   * draws frames and shows black is indistinguishable from one that works by
   * every other counter, and that failure has happened at every layer of this
   * pipeline. Sampled every fourth pixel, because this is asked for once a
   * second at most and the answer only has to be nonzero.
   */
  async describe() {
    const wins = [...this.#windows.values()].filter((w) => w.role === ROLE_TOPLEVEL);
    let lit = 0;
    let brightest = 0;
    // A content hash of the output, so a test can tell "the click did
    // something" from "the click did nothing" when what it did was three
    // pixels of a button going darker.
    let hash = 2166136261;
    try {
      const r = await this.#painter.read();
      const px = r?.px ?? new Uint8Array(0);
      // Above the desktop's own colour, so the background does not count as
      // something drawn.
      const floor = Math.round(COL.desktop[2] * 255) + 12;
      for (let i = 0; i < px.length; i += 16) {
        const v = Math.max(px[i], px[i + 1], px[i + 2]);
        if (v > floor) lit++;
        if (v > brightest) brightest = v;
        hash = ((hash ^ v) * 16777619) >>> 0;
      }
    } catch {
      /* nothing drawn yet */
    }
    return {
      frames: this.#frames,
      windows: wins.length,
      surfaces: this.#windows.size,
      // Every surface the compositor is holding, so a test can see a window
      // appear twice rather than infer it from a screenshot.
      list: [...this.#windows.values()].map((w) =>
        `${w.key} role=${w.role} ${w.width}x${w.height}@${w.x},${w.y}`
        + ` serial=${w.info?.serial ?? 0} "${w.title}"`),
      log: this.#log.slice(-24),
      staging: this.#stagingOk === false,
      titles: wins.map((w) => w.title || '(untitled)'),
      focus: this.#focus,
      lit,
      brightest,
      hash,
      output: `${this.#outW}x${this.#outH}`,
      // Direct present (`?direct=1`): the GL frame's size while it is the
      // output (then `lit`/`hash` describe the last composited scene).
      direct: this.#direct ? `${this.#direct.w}x${this.#direct.h}` : null,
      // The GL device's counters (submits, their time, readbacks): the
      // compositor side of a frame's cost.
      gl: glDevice?.glState?.() ?? null,
    };
  }
}

// -- the worker's protocol --------------------------------------------------

let comp = null;
let canvas = null;
let running = false;

function loop() {
  if (!comp || !running) return;
  try {
    // A guest paces itself against frames *displayed*, not produced: its
    // `eglSwapBuffers` waits for this count to move. On the page that was the
    // demo's own animation loop; here there is no other loop to do it, and a
    // client that swapped once would wait for ever.
    glTick++;
    glDevice?.display?.();
    comp.frame();
  } catch (e) {
    postMessage({ kind: 'error', error: e?.stack || String(e) });
    running = false;
    return;
  }
  requestAnimationFrame(loop);
}

// -- the guest's OpenGL, in this worker -------------------------------------
//
// Mesa runs here rather than on the page: a GL window's frame is a canvas in
// this worker, uploaded into the scene GPU-side, so nothing comes back
// through the guest.
let glDevice = null;
let glChannel = null;
let glServing = false;
// Where Mesa presents: a canvas whose WebGPU context is a texture on the
// shared device (gpu-device.js textureCanvas), which the scene samples.
let glTarget = null;
let glFrames = null;

/** What the guest last said its frames belong to, or null. */
function glAttachment() {
  return glDevice?.ok ? glDevice.attachment() : null;
}

/* Every window the host's GL is drawing, and what it last drew for each.
 *
 * Re-read when the device says the list has changed, which is what makes more
 * than one GL window work: each window is matched against its own entry and
 * drawn from its own frame, rather than every GL window being handed whichever
 * frame presented last. */
let glSurfaces = [];
let glSurfaceFrame = -1;
// Bumped once per composited frame, so the list above is read once per frame
// however many windows ask for it.
let glTick = 0;

function glSurfaceList() {
  if (!glDevice?.ok) return [];
  // Once per frame, and read fresh rather than when a counter says it
  // changed: a list captured while only one client had attached and never
  // refreshed is a second GL window that is never drawn, which is a much
  // worse trade than the handful of wasm calls this costs.
  if (glSurfaceFrame === glTick) return glSurfaces;
  glSurfaceFrame = glTick;
  glSurfaces = glDevice.surfaces();
  return glSurfaces;
}

/* The GL frame for one window, or null.
 *
 * A Wayland client names its own surface and the process that published it;
 * an X client names a drawable, and the window manager is what knows which
 * surface that window became (`xid` on the record). */
function glFrameFor(win) {
  for (const s of glSurfaceList()) {
    if (!s.id) continue;
    const hit = s.kind === 0
      ? win.token === s.token && win.surface === s.id
      : win.info?.xid === s.id;
    if (!hit) continue;
    // The WebGPU backend has one canvas and one frame; it is this window's if
    // the match above says so.
    return glFrame();
  }
  return null;
}

/** The frame itself, or null if there is none yet: `tex` is the texture
 * Mesa's last displayed frame was copied into (gex_gl_display). */
function glFrame() {
  const tex = glFrames?.texture();
  if (!tex || tex.width < 2 || tex.height < 2) return null;   // still at its placeholder size
  return { tex, w: tex.width, h: tex.height, serial: glDevice?.presentedCount?.() ?? 0 };
}

async function startGl(m, canvas) {
  // Mesa presents into a canvas of its own whose 'webgpu' context is a
  // texture on the compositor's device: its frames are drawn into the scene
  // like any window's, with no copy between APIs.
  glFrames = textureCanvas();
  glTarget = glFrames.canvas;
  // Emscripten looks a canvas up by selector, and a worker has no document.
  if (!globalThis.document) globalThis.document = { querySelector: () => glTarget };
  const { MesaGlDevice } = await import('./mesa-gl.js');
  glDevice = await MesaGlDevice.create(glTarget, {
    url: m.url,
    api: m.api || 'core',
    env: m.mesaEnv,
    capture: m.glCapture,
    wgslCache: m.wgslCache,
    wgpuStats: m.wgpuStats,
    gpu: await requestGexDevice(),
  });
  if (!glDevice.ok) throw new Error(glDevice.error || 'the GL device would not start');
  glChannel = m.channel;
  postMessage({ kind: 'gl-ready', describe: glDevice.describe() });
  serveGlRing();
}

/** Run the guest's submissions as they arrive.
 *
 * `serveGl` drives itself where `Atomics.waitAsync` exists, which is a worker
 * -- a second loop over the same channel here is not more service, it is a
 * spin that starves the frame loop. Ask for it and leave it alone. */
async function serveGlRing() {
  if (glServing) return;
  glServing = true;
  const { serveGl } = await import('@gex/gl-bridge.mjs');
  serveGl(glDevice, glChannel);
}

self.onmessage = ({ data: m }) => {
  try {
    if (m.kind === 'init') {
      canvas = m.canvas;
      presentOverride = m.present ?? null;
      // The compositor draws with WebGPU on the device Mesa will use.
      if (m.glLimits) globalThis.GEX_GL_LIMITS = m.glLimits;
      requestGexDevice().then((gpu) => {
        comp = new Compositor(m.canvas, m.channel, m.stats ? new Int32Array(m.stats) : null, gpu);
        comp.setDirect(m.direct);
        running = true;
        requestAnimationFrame(loop);
        postMessage({ kind: 'ready' });
      }).catch((e) => postMessage({ kind: 'error', error: e?.stack || String(e) }));
    } else if (m.kind === 'gl') {
      // The guest's GL, hosted here for the reason above. Reported rather
      // than thrown: a session whose GL will not start should still show its
      // other windows.
      startGl(m, canvas).catch((e) =>
        postMessage({ kind: 'gl-failed', error: e?.message || String(e) }));
    } else if (m.kind === 'memory') {
      comp?.addMemory(m.token, m.memory);
    } else if (m.kind === 'drop-memory') {
      comp?.dropMemory(m.token);
      // That process is gone: its GL context, its windows and their frames go
      // with it. Nothing else will ever say so -- a client that exits is not
      // there to send a detach.
      glDevice?.forgetClient?.(m.token);
      glSurfaceFrame = -1;
    } else if (m.kind === 'resize') {
      comp?.resize(m.width, m.height);
    } else if (m.kind === 'pointer-move') {
      comp?.pointerMove(m.x, m.y, m.time);
    } else if (m.kind === 'pointer-button') {
      comp?.pointerButton(m.button, m.pressed, m.time);
    } else if (m.kind === 'pointer-axis') {
      comp?.pointerAxis(m.axis, m.value, m.time);
    } else if (m.kind === 'key') {
      comp?.key(m.code, m.pressed, m.mods, m.time);
    } else if (m.kind === 'wgsl-cache') {
      // The shader translation cache, for a training run to record.
      const blob = glDevice?.wgslCache?.() ?? null;
      postMessage({ kind: 'wgsl-cache', id: m.id, blob, stats: glDevice?.wgslStats?.() ?? null }, blob ? [blob] : []);
    } else if (m.kind === 'readout') {
      // The composited output, read back (RGBA rows, top first): what a test saves as a picture.
      Promise.resolve(comp?.readout?.() ?? null).then((r) => postMessage({ kind: 'readout', id: m.id, state: r }, r ? [r.px.buffer] : []));
    } else if (m.kind === 'describe') {
      Promise.resolve(comp?.describe() ?? null).then((state) => postMessage({ kind: 'describe', id: m.id, state }));
    } else if (m.kind === 'stop') {
      running = false;
    }
  } catch (e) {
    postMessage({ kind: 'error', error: e?.stack || String(e) });
  }
};
