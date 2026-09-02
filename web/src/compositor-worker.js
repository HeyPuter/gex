// gex-wayland's output: a real compositor, in a worker, on one WebGL2 context.
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
// it in place. An OpenGL client's pixels take one more step than a software
// one's -- Mesa renders through virgl in the renderer worker and reads the
// result back into its shm buffer, because vtest has no way to hand a texture
// over -- but nothing is copied on this side of the guest either way.

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

// -- GL ---------------------------------------------------------------------

const VERT = `#version 300 es
// One unit quad, placed by a pixel rectangle. No vertex buffers: the corner
// comes from gl_VertexID, so a draw is four bytes of uniform and a call.
uniform vec4 uRect;      // x, y, w, h in output pixels
uniform vec2 uOutput;    // output size in pixels
uniform vec4 uTexRect;   // sub-rectangle of the texture to sample (0..1)
out vec2 vUV;
void main() {
  vec2 corner = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
  vec2 px = uRect.xy + corner * uRect.zw;
  vec2 clip = vec2(px.x / uOutput.x * 2.0 - 1.0, 1.0 - px.y / uOutput.y * 2.0);
  vUV = uTexRect.xy + corner * uTexRect.zw;
  gl_Position = vec4(clip, 0.0, 1.0);
}`;

const FRAG = `#version 300 es
precision mediump float;
uniform sampler2D uTex;
uniform vec4 uColor;
uniform int uMode;       // 0 = texture, 1 = solid colour
uniform int uSwizzle;    // 1 = the texture is BGRA (wl_shm), 0 = RGBA
uniform int uOpaque;     // 1 = force alpha to 1 (XRGB has no alpha)
in vec2 vUV;
out vec4 fragColor;
void main() {
  if (uMode == 1) {
    fragColor = uColor;
    return;
  }
  vec4 c = texture(uTex, vUV);
  if (uSwizzle == 1) c = c.bgra;
  if (uOpaque == 1) c.a = 1.0;
  fragColor = c;
}`;

function compile(gl, type, src) {
  const sh = gl.createShader(type);
  gl.shaderSource(sh, src);
  gl.compileShader(sh);
  if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) {
    throw new Error(`shader: ${gl.getShaderInfoLog(sh)}`);
  }
  return sh;
}

class Painter {
  constructor(gl) {
    this.gl = gl;
    const prog = gl.createProgram();
    gl.attachShader(prog, compile(gl, gl.VERTEX_SHADER, VERT));
    gl.attachShader(prog, compile(gl, gl.FRAGMENT_SHADER, FRAG));
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) {
      throw new Error(`link: ${gl.getProgramInfoLog(prog)}`);
    }
    this.prog = prog;
    this.u = {
      rect: gl.getUniformLocation(prog, 'uRect'),
      output: gl.getUniformLocation(prog, 'uOutput'),
      texRect: gl.getUniformLocation(prog, 'uTexRect'),
      color: gl.getUniformLocation(prog, 'uColor'),
      mode: gl.getUniformLocation(prog, 'uMode'),
      swizzle: gl.getUniformLocation(prog, 'uSwizzle'),
      opaque: gl.getUniformLocation(prog, 'uOpaque'),
      tex: gl.getUniformLocation(prog, 'uTex'),
    };
    this.vao = gl.createVertexArray();
    // A sampler, so what this draws does not depend on parameters set by
    // whoever made the texture. The guest's own frames are textures Mesa
    // created and left with the GL default filter -- mipmapped, with no
    // mipmaps -- which samples as black and nothing says why. A sampler
    // object overrides that for this unit without touching the texture, which
    // matters because the texture is not ours.
    this.sampler = gl.createSampler();
    gl.samplerParameteri(this.sampler, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.samplerParameteri(this.sampler, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.samplerParameteri(this.sampler, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.samplerParameteri(this.sampler, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.useProgram(prog);
    gl.uniform1i(this.u.tex, 0);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
  }

  begin(w, h) {
    const gl = this.gl;
    // This context is shared: the guest's OpenGL runs in it too (see
    // `startGl`). Two things follow, and both were learned the hard way.
    //
    // What it leaves behind -- a framebuffer, a scissor, a depth test -- is
    // enough to make this draw nothing, or draw into something that is not
    // the canvas. So the state this needs is set, not assumed.
    //
    // And what *this* leaves behind is just as bad the other way: the driver
    // on the other side caches what it believes is bound and does not re-bind
    // what it thinks is already right. Leaving framebuffer 0 bound sent the
    // guest's next frame to the canvas instead of its own texture -- one
    // frame drawn and then nothing, for ever. So `end` puts it all back.
    this.saved = {
      fb: gl.getParameter(gl.FRAMEBUFFER_BINDING),
      program: gl.getParameter(gl.CURRENT_PROGRAM),
      vao: gl.getParameter(gl.VERTEX_ARRAY_BINDING),
      unit: gl.getParameter(gl.ACTIVE_TEXTURE),
      viewport: gl.getParameter(gl.VIEWPORT),
      blend: gl.isEnabled(gl.BLEND),
      scissor: gl.isEnabled(gl.SCISSOR_TEST),
      depth: gl.isEnabled(gl.DEPTH_TEST),
      stencil: gl.isEnabled(gl.STENCIL_TEST),
      cull: gl.isEnabled(gl.CULL_FACE),
      depthMask: gl.getParameter(gl.DEPTH_WRITEMASK),
      colorMask: gl.getParameter(gl.COLOR_WRITEMASK),
      blendSrc: gl.getParameter(gl.BLEND_SRC_ALPHA),
      blendDst: gl.getParameter(gl.BLEND_DST_ALPHA),
    };
    gl.activeTexture(gl.TEXTURE0);
    this.saved.tex0 = gl.getParameter(gl.TEXTURE_BINDING_2D);
    this.saved.sampler = gl.getParameter(gl.SAMPLER_BINDING);
    gl.bindSampler(0, this.sampler);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.disable(gl.SCISSOR_TEST);
    gl.disable(gl.DEPTH_TEST);
    gl.disable(gl.STENCIL_TEST);
    gl.disable(gl.CULL_FACE);
    gl.colorMask(true, true, true, true);
    gl.depthMask(false);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindVertexArray(this.vao);
    gl.useProgram(this.prog);
    gl.uniform1i(this.u.tex, 0);
    gl.viewport(0, 0, w, h);
    gl.uniform2f(this.u.output, w, h);
  }

  /** Put the context back the way the guest's driver believes it is. */
  end() {
    const gl = this.gl;
    const s = this.saved;
    if (!s) return;
    gl.bindFramebuffer(gl.FRAMEBUFFER, s.fb);
    gl.useProgram(s.program);
    gl.bindVertexArray(s.vao);
    gl.bindTexture(gl.TEXTURE_2D, s.tex0);
    gl.bindSampler(0, s.sampler);
    gl.activeTexture(s.unit);
    gl.viewport(s.viewport[0], s.viewport[1], s.viewport[2], s.viewport[3]);
    const set = (cap, on) => (on ? gl.enable(cap) : gl.disable(cap));
    set(gl.BLEND, s.blend);
    set(gl.SCISSOR_TEST, s.scissor);
    set(gl.DEPTH_TEST, s.depth);
    set(gl.STENCIL_TEST, s.stencil);
    set(gl.CULL_FACE, s.cull);
    gl.depthMask(s.depthMask);
    gl.colorMask(s.colorMask[0], s.colorMask[1], s.colorMask[2], s.colorMask[3]);
    gl.blendFunc(s.blendSrc, s.blendDst);
    this.saved = null;
  }

  solid(x, y, w, h, color) {
    const gl = this.gl;
    gl.uniform1i(this.u.mode, 1);
    gl.uniform4f(this.u.color, color[0], color[1], color[2], color[3]);
    gl.uniform4f(this.u.rect, x, y, w, h);
    gl.uniform4f(this.u.texRect, 0, 0, 1, 1);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }

  textured(x, y, w, h, tex, { swizzle = 0, opaque = 0, uv = [0, 0, 1, 1] } = {}) {
    const gl = this.gl;
    gl.uniform1i(this.u.mode, 0);
    gl.uniform1i(this.u.swizzle, swizzle);
    gl.uniform1i(this.u.opaque, opaque);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, tex);
    gl.uniform4f(this.u.rect, x, y, w, h);
    gl.uniform4f(this.u.texRect, uv[0], uv[1], uv[2], uv[3]);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }
}

// -- text, for titlebars ----------------------------------------------------
//
// A titlebar needs a string drawn, and WebGL has no text. A 2D canvas does, so
// each window's title becomes a small texture, remade only when the title
// changes -- which for a running app is almost never.
class TextCache {
  #gl;
  #entries = new Map();

  constructor(gl) {
    this.#gl = gl;
  }

  get(text) {
    let e = this.#entries.get(text);
    if (e) return e;
    const gl = this.#gl;
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
    const tex = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, tex);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, cv);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    e = { tex, w: cv.width / scale, h: cv.height / scale };
    // A cache with no bound is a leak; titles change rarely and a session has
    // a handful of windows, so a small cap is plenty.
    if (this.#entries.size > 64) {
      for (const [k, v] of this.#entries) {
        gl.deleteTexture(v.tex);
        this.#entries.delete(k);
        if (this.#entries.size <= 32) break;
      }
    }
    this.#entries.set(text, e);
    return e;
  }
}

/** The pointer image, drawn when no client has set one of its own. */
function defaultCursor(gl) {
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
  const tex = gl.createTexture();
  gl.bindTexture(gl.TEXTURE_2D, tex);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, cv);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  return { tex, w: 12, h: 19 };
}

// -- the compositor ---------------------------------------------------------

const key = (s) => `${s.token}:${s.client}:${s.surface}`;

class Compositor {
  #gl;
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
  #probeFbo = null;
  #focus = null;
  #pointer = { x: 0, y: 0, over: null, buttons: 0 };
  #drag = null;
  #outW = 0;
  #outH = 0;
  #frames = 0;
  #placed = 0;
  #stats = null;
  /** A view onto guest memory can be refused by WebGL; then we stage a copy. */
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

  constructor(canvas, channel, stats) {
    this.#channel = channel;
    this.#stats = stats;
    const gl = canvas.getContext('webgl2', {
      alpha: false,
      antialias: false,
      depth: false,
      preserveDrawingBuffer: true,
    });
    if (!gl) throw new Error('no webgl2 for the compositor');
    this.#gl = gl;
    this.#canvas = canvas;
    this.#painter = new Painter(gl);
    this.#text = new TextCache(gl);
    this.#cursorFallback = defaultCursor(gl);
    this.resize(canvas.width, canvas.height);
  }

  #canvas;

  resize(w, h) {
    this.#outW = Math.max(1, w | 0);
    this.#outH = Math.max(1, h | 0);
    if (this.#canvas.width !== this.#outW) this.#canvas.width = this.#outW;
    if (this.#canvas.height !== this.#outH) this.#canvas.height = this.#outH;
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
        this.#gl.deleteTexture(tex.tex);
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
    const gl = this.#gl;
    let t = this.#textures.get(k);
    if (t && (t.w !== w || t.h !== h)) {
      gl.deleteTexture(t.tex);
      t = null;
    }
    if (!t) {
      const tex = gl.createTexture();
      gl.bindTexture(gl.TEXTURE_2D, tex);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
      t = { tex, w, h, serial: 0 };
      this.#textures.set(k, t);
    }
    return t;
  }

  /**
   * Upload what changed of one surface, reading the guest's memory in place.
   *
   * The rows are contiguous in the guest at `stride` bytes each, so a damage
   * rectangle is uploaded as whole rows: `UNPACK_ROW_LENGTH` lets GL stride
   * over the parts that did not change, and the sub-rectangle costs one call.
   */
  #upload(k, s) {
    // Nothing to read: this surface's pixels are the host's, and `addr` names
    // no buffer. See `F_EXTERNAL`.
    if (s.external || !s.addr) return true;
    const gl = this.#gl;
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
    const rowWords = Math.floor(s.stride / 4);
    const start = s.addr + y * s.stride + x * 4;
    const need = (h - 1) * s.stride + w * 4;
    if (start + need > mem.length) {
      // Past the end of what this process's memory holds. Worth saying: it
      // means the descriptor and the memory disagree, which is either a
      // client's bad buffer or a stale view of a grown memory.
      this.#note(`! ${k} buffer at ${start}+${need} past ${mem.length}`);
      return false;
    }
    gl.bindTexture(gl.TEXTURE_2D, t.tex);
    gl.pixelStorei(gl.UNPACK_ROW_LENGTH, rowWords);
    let view;
    if (this.#stagingOk === false) {
      // WebGL refused a SharedArrayBuffer view, so one staging copy it is --
      // still three fewer copies than the path this replaced.
      if (this.#staging.length < need) this.#staging = new Uint8Array(need);
      this.#staging.set(mem.subarray(start, start + need));
      view = this.#staging.subarray(0, need);
    } else {
      view = mem.subarray(start, start + need);
    }
    try {
      gl.texSubImage2D(gl.TEXTURE_2D, 0, x, y, w, h, gl.RGBA, gl.UNSIGNED_BYTE, view);
      if (this.#stagingOk === null) this.#stagingOk = true;
    } catch (e) {
      if (this.#stagingOk === null) {
        // Once, and then never again: fall back and redo this upload.
        this.#stagingOk = false;
        gl.pixelStorei(gl.UNPACK_ROW_LENGTH, 0);
        return this.#upload(k, s);
      }
      throw e;
    }
    gl.pixelStorei(gl.UNPACK_ROW_LENGTH, 0);
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
    // A window whose pixels are the host's GL, not the guest's memory. The
    // texture is already in this context, and upside down relative to
    // everything else here: GL's origin is bottom-left.
    const gl = this.#glSurface(win);
    if (gl) {
      this.#glDraws++;
      // `uv` is an origin and a size, not two corners: flipping means starting
      // at the top and walking down. Read as corners it samples one row of the
      // frame across the whole window, which looks like vertical stripes in
      // the right colours -- the gears, one scanline of them.
      this.#painter.textured(f.x, f.y, win.width, win.height, gl.tex,
                             { opaque: 1, uv: [0, 1, 1, -1] });
      return;
    }
    const t = this.#textures.get(k);
    if (!t) return;
    this.#painter.textured(f.x, f.y, win.width, win.height, t.tex, {
      swizzle: 1,
      opaque: win.info?.format === 1 ? 1 : 0,
    });
  }

  /** What this context reads out of the guest's texture: the compositor's own
   *  view of it, which is the only thing that says whether a black window is
   *  a black frame or a texture this side cannot see. */
  #glSample(f) {
    if (!f?.tex) return 'none';
    const gl = this.#gl;
    try {
      if (!this.#probeFbo) this.#probeFbo = gl.createFramebuffer();
      const was = gl.getParameter(gl.FRAMEBUFFER_BINDING);
      gl.bindFramebuffer(gl.FRAMEBUFFER, this.#probeFbo);
      gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, f.tex, 0);
      const status = gl.checkFramebufferStatus(gl.FRAMEBUFFER);
      let px = new Uint8Array(4);
      if (status === gl.FRAMEBUFFER_COMPLETE) {
        gl.readPixels(Math.floor(f.w / 2), Math.floor(f.h / 2), 1, 1,
                      gl.RGBA, gl.UNSIGNED_BYTE, px);
      }
      gl.bindFramebuffer(gl.FRAMEBUFFER, was);
      return `${status.toString(16)} ${px[0]},${px[1]},${px[2]},${px[3]}`;
    } catch (e) {
      return `failed: ${e?.message || e}`;
    }
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
    const stamp = now - this.#glSaidAt > 2000
      ? `${a ? `${a.kind}:${a.id}@${a.token}#${a.generation}` : 'none'} f${f0?.serial ?? 0}`
      : this.#glSaid;
    if (stamp !== this.#glSaid) this.#glSaidAt = now;
    if (stamp !== this.#glSaid) {
      this.#glSaid = stamp;
      const f = glFrame();
      this.#note(`gl attachment ${stamp}, frame ${f ? `${f.w}x${f.h} #${f.serial}` : 'none'}`
        + `, window ${win.token}:${win.surface} xid ${win.info?.xid ?? 0}`
        + `, drawn ${this.#glDraws}`
        + `, shared ${glRenderer?.sharesContextWith?.(this.#gl) ?? '?'}`
        + `, sample ${this.#glSample(f)}`
        + `, device ${glDevice?.glState?.() ?? '?'}`
        + `, renderer ${glRenderer?.glState?.() ?? '?'}`);
    }
    if (!a || !a.id) return null;
    // A Wayland client names its own surface and the process that published
    // it; an X client names a drawable, and the window manager is what put
    // that window's id on the record.
    const hit = a.kind === 0
      ? win.token === a.token && win.surface === a.id
      : win.info?.xid === a.id;
    if (!hit) return null;
    const frame = glFrame();
    return frame && frame.tex ? frame : null;
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

  frame() {
    const { byKey, blocks } = this.#reconcile();
    const gl = this.#gl;
    // Upload every surface whose content moved on.
    for (const [k, s] of byKey) {
      const t = this.#textures.get(k);
      if (t && t.serial === s.serial) continue;
      this.#upload(k, s);
    }
    this.#painter.begin(this.#outW, this.#outH);
    gl.clearColor(...COL.desktop);
    gl.clear(gl.COLOR_BUFFER_BIT);
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
    // The guest's driver shares this context and caches what it thinks is
    // bound: leave it as it was found.
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

  /**
   * What the harness and the demo UI read.
   *
   * `lit` is a real readback of the output: a compositor that maps windows,
   * draws frames and shows black is indistinguishable from one that works by
   * every other counter, and that failure has happened at every layer of this
   * pipeline. Sampled every fourth pixel, because this is asked for once a
   * second at most and the answer only has to be nonzero.
   */
  describe() {
    const gl = this.#gl;
    const wins = [...this.#windows.values()].filter((w) => w.role === ROLE_TOPLEVEL);
    let lit = 0;
    let brightest = 0;
    // A content hash of the output, so a test can tell "the click did
    // something" from "the click did nothing" when what it did was three
    // pixels of a button going darker.
    let hash = 2166136261;
    try {
      const w = this.#outW;
      const h = this.#outH;
      const px = new Uint8Array(w * h * 4);
      gl.readPixels(0, 0, w, h, gl.RGBA, gl.UNSIGNED_BYTE, px);
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
      /* a context that is not current yet answers nothing */
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
// Mesa and virglrenderer run here rather than on the page, and that is the
// whole point: they render into *this* worker's WebGL context, so a GL
// window's frame is a texture this compositor binds. Nothing is read back and
// nothing is uploaded -- the path that used to end in the guest copying its
// own frame into an shm buffer for us to upload again.
//
// One canvas is what makes it one context: `getContext('webgl2')` on a canvas
// that already has one hands back the same object, whoever asked first.
let glDevice = null;
let glRenderer = null;
let glChannel = null;
let glServing = false;

/** What the guest last said its frames belong to, or null. */
function glAttachment() {
  return glDevice?.ok ? glDevice.attachment() : null;
}

/** The frame itself: a texture in this context, or null if there is none. */
function glFrame() {
  return glRenderer?.ok ? glRenderer.frame() : null;
}

async function startGl(m, canvas) {
  // Emscripten looks a canvas up by selector, and a worker has no document.
  if (!globalThis.document) globalThis.document = { querySelector: () => canvas };
  const { MesaGlDevice } = await import('./mesa-gl.js');
  glDevice = await MesaGlDevice.create(canvas, {
    backend: 'virgl',
    url: m.url || '/gex/gex-virgl-replay.js',
    api: m.api || 'core',
  });
  if (!glDevice.ok) throw new Error(glDevice.error || 'the GL device would not start');
  glRenderer = glDevice.renderer;
  // The compositor draws these frames where their window is; a blit would put
  // the last one over the whole output. `?glcanvas=1` keeps the blit, which
  // is how to tell "the renderer drew nothing" from "the compositor drew it
  // wrong" -- they look the same from outside.
  glRenderer.presentToCanvas(!!m.canvasBlit);
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
      comp = new Compositor(m.canvas, m.channel, m.stats ? new Int32Array(m.stats) : null);
      running = true;
      requestAnimationFrame(loop);
      postMessage({ kind: 'ready' });
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
    } else if (m.kind === 'describe') {
      postMessage({ kind: 'describe', id: m.id, state: comp?.describe() ?? null });
    } else if (m.kind === 'stop') {
      running = false;
    }
  } catch (e) {
    postMessage({ kind: 'error', error: e?.stack || String(e) });
  }
};
