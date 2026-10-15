// Windows 7 DWM's glass, as dwmcore.dll and uDWM.dll compute it, for the
// compositor to draw pixel for pixel (compositor-worker.js, Painter.glass).
//
// The blur (dwmcore's CBlurEffect, on Direct3D 10 hardware): a Gaussian of
// sigma 3 (CCommonRegistryData::m_flBlurSigma) and radius 7 (6.9 rounded up,
// GetEffectiveBlurRadius), its 15 weights paired into 8 bilinear taps
// (CHw1DConvolutionFilterSampleCalculator: samples -7 and -6 make the first
// tap, ..., 5 and 6 the seventh, 7 alone the eighth), normalized by their sum.
// Two passes over the back buffer, which is gamma-encoded B8G8R8A8:
//   1. horizontally, into an accumulation buffer of (W >> 2) x H texels of the
//      same format: buffer texel bx is centred on the screen's column 4 bx + 2,
//      its taps offset by (position - 0.5) source texels (so the kernel's
//      centre is column 4 bx + 1), the buffer's mesh x scaled by 0.25;
//   2. vertically, from that buffer stretched back to the screen's width
//      (bilinear), the taps offset by their positions, then the colorization
//      (dwmcore's pixel shader 2005):
//        glass = B * blurBalance + luma709(B) * afterglow + colour, alpha 1
//      with afterglow and colour premultiplied by their balances.
// Sampling clamps at the screen's edges.
//
// The colorization (uDWM's CGlassColorizationParameters): the registry's
// ColorizationColor and ColorizationAfterglow (their alpha unused) and the
// balances, adjusted per window by the frame's COLORIZATIONOPACITY f (the
// theme's DWMWindow part and state: TOPFRAME 100% active, 40% inactive) as
// AdjustWindowColorization does, each balance rounded to a whole percent:
//   colourBalance' = colourBalance * f
//   blurBalance'   = 1 - (1 - blurBalance) * f
//   afterglowBalance unchanged.

/** The blur's sigma and radius, in screen pixels. */
export const DWM_BLUR_SIGMA = 3;
export const DWM_BLUR_RADIUS = 7;

const f32 = Math.fround;

/**
 * DWM's 8 hardware taps: `positions` (texels from the centre, before the
 * passes' own offsets) and `weights` (normalized), in float32 as dwmcore
 * computes them.
 */
export function dwmBlurTaps(sigma = DWM_BLUR_SIGMA, radius = DWM_BLUR_RADIUS) {
  const s2 = f32(sigma * sigma);
  const twoS2 = s2 + s2; // a double in dwmcore
  const norm = 1 / f32(Math.sqrt(f32(s2 * f32(6.283185482025146))));
  const input = [];
  for (let i = 0; i <= 2 * radius; i++) {
    const x = f32(i - radius);
    input.push(f32(Math.pow(2.718281828459045, (x * x * -1) / twoS2) * norm));
  }
  const positions = [], weights = [];
  let total = 0;
  for (let i = 0; i <= 2 * radius; i += 2) {
    const a = input[i], b = i + 1 <= 2 * radius ? input[i + 1] : 0;
    const w = f32(a + b);
    positions.push(f32((i - radius) + f32(b / w)));
    weights.push(w);
    total = f32(total + w);
  }
  return { positions, weights: weights.map((w) => f32(w / total)) };
}

const byte = (v, shift) => ((v >>> shift) & 0xff) / 255;
const rgb = (argb) => [byte(argb, 16), byte(argb, 8), byte(argb, 0)];
/** uDWM's FloatPercentageToUInt: floor(x * 100 + 0.5). */
const percent = (x) => Math.floor(f32(f32(x * 100) + 0.5));

/**
 * The pixel shader's constants for a window's glass: `colour` and
 * `afterglow` (rgb, balance applied) and `blur`, from `glass` (`readScene`'s:
 * argb, afterglow, balance, afterglowBalance, blurBalance, percentages) and
 * the frame's COLORIZATIONOPACITY `f` (0..1).
 */
export function dwmGlassConstants(glass, f = 1) {
  const cb = (glass?.balance ?? 0) / 100;
  const ab = (glass?.afterglowBalance ?? 0) / 100;
  const bb = (glass?.blurBalance ?? 100) / 100;
  // AdjustWindowColorization with no tint of the frame's own (its
  // COLORIZATIONCOLOR alpha 0, as Aero's TOPFRAME states have)
  const colourBalance = percent(f32(cb * f)) / 100;
  const blurBalance = percent(f32(1 - (1 - bb) * f)) / 100;
  const afterglowBalance = percent(ab) / 100;
  const clamp = (x) => Math.min(1, Math.max(0, x));
  const c = clamp(colourBalance), a = clamp(afterglowBalance), b = clamp(blurBalance);
  return {
    colour: rgb(glass?.argb ?? 0).map((v) => f32(v * c)),
    afterglow: rgb(glass?.afterglow ?? 0).map((v) => f32(v * a)),
    blur: f32(b),
  };
}

/** The two passes' WGSL: `accumulate` (into the quarter-width buffer) and
 *  `compose` (into the scene), over one uniform block. */
export const DWM_GLASS_WGSL = /* wgsl */ `
struct G {
  rect: vec4f,        // the quad drawn, in the target's pixels
  viewport: vec2f,    // the render target's size in pixels
  screen: vec2f,      // the screen's size W, H
  origin: vec2f,      // the source texture's top-left on the screen (x a multiple of 4)
  bufOrigin: vec2f,   // the buffer texture's top-left in buffer texels
  bufWidth: f32,      // W >> 2
  blur: f32,          // blur balance
  pad: vec2f,
  colour: vec4f,      // colour * colour balance
  afterglow: vec4f,   // afterglow * afterglow balance
  pos: array<vec4f, 2>,  // the 8 tap positions
  wt: array<vec4f, 2>,   // the 8 tap weights
};
@group(0) @binding(0) var<uniform> g: G;
@group(0) @binding(1) var src: texture_2d<f32>;

@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {
  let corner = vec2f(f32(i & 1u), f32((i >> 1u) & 1u));
  let px = g.rect.xy + corner * g.rect.zw;
  return vec4f(px.x / g.viewport.x * 2.0 - 1.0, 1.0 - px.y / g.viewport.y * 2.0, 0.0, 1.0);
}

fn tap(i: u32) -> vec2f {
  return vec2f(g.pos[i / 4u][i % 4u], g.wt[i / 4u][i % 4u]);
}

// A bilinear fetch at p (texel units, centres at n + 0.5) of the texture whose
// texel 0 is \`origin\` of a surface \`size\` texels big, clamped to that surface.
fn bilinear(p: vec2f, origin: vec2f, size: vec2f) -> vec4f {
  let q = clamp(p, vec2f(0.5), size - 0.5) - origin - 0.5;
  let i = vec2i(floor(q));
  let f = q - floor(q);
  let dim = vec2i(textureDimensions(src)) - 1;
  let a = textureLoad(src, clamp(i, vec2i(0), dim), 0);
  let b = textureLoad(src, clamp(i + vec2i(1, 0), vec2i(0), dim), 0);
  let c = textureLoad(src, clamp(i + vec2i(0, 1), vec2i(0), dim), 0);
  let d = textureLoad(src, clamp(i + vec2i(1, 1), vec2i(0), dim), 0);
  return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

@fragment fn accumulate(@builtin(position) at: vec4f) -> @location(0) vec4f {
  // at.xy: this buffer texel's centre, in buffer texels from bufOrigin
  let bx = at.x + g.bufOrigin.x;            // the buffer's own x (centre)
  let y = at.y + g.bufOrigin.y;
  let scale = g.screen.x / (4.0 * g.bufWidth);
  var c = vec3f(0.0);
  for (var i = 0u; i < 8u; i++) {
    let t = tap(i);
    let x = bx * 4.0 + (t.x - 0.5) * scale;
    c += bilinear(vec2f(x, y), g.origin, g.screen).rgb * t.y;
  }
  return vec4f(c, 1.0);
}

@fragment fn compose(@builtin(position) at: vec4f) -> @location(0) vec4f {
  // at.xy: the screen pixel's centre (the scene is the screen)
  let bx = at.x * g.bufWidth / g.screen.x;
  var c = vec3f(0.0);
  for (var i = 0u; i < 8u; i++) {
    let t = tap(i);
    c += bilinear(vec2f(bx, at.y + t.x), g.bufOrigin, vec2f(g.bufWidth, g.screen.y)).rgb * t.y;
  }
  let luma = dot(c, vec3f(0.2126, 0.7152, 0.0722));
  return vec4f(c * g.blur + luma * g.afterglow.rgb + g.colour.rgb, 1.0);
}`;

/** Bytes between two draws' uniforms: the minimum dynamic-offset alignment. */
const SLOT = 256;

/**
 * DWM's glass on a WebGPU device: `encode` records the two passes for one
 * window's glass into a command encoder, reading and writing `scene` (the
 * screen so far, `format`, with COPY_SRC and RENDER_ATTACHMENT usage).
 * `frame()` before each frame's first `encode` reuses the uniform slots.
 */
export class DwmGlass {
  constructor(device, format, { maxDraws = 256 } = {}) {
    this.device = device;
    this.format = format;
    this.maxDraws = maxDraws;
    this.taps = dwmBlurTaps();
    const module = device.createShaderModule({ code: DWM_GLASS_WGSL });
    this.layout = device.createBindGroupLayout({
      entries: [
        { binding: 0, visibility: GPUShaderStage.VERTEX | GPUShaderStage.FRAGMENT, buffer: { type: 'uniform', hasDynamicOffset: true } },
        { binding: 1, visibility: GPUShaderStage.FRAGMENT, texture: {} },
      ],
    });
    const layout = device.createPipelineLayout({ bindGroupLayouts: [this.layout] });
    const pipeline = (entryPoint, f) => device.createRenderPipeline({
      layout,
      vertex: { module, entryPoint: 'vs' },
      fragment: { module, entryPoint, targets: [{ format: f }] },
      primitive: { topology: 'triangle-strip' },
    });
    this.accumulate = pipeline('accumulate', 'rgba8unorm');
    this.compose = pipeline('compose', format);
    this.uniforms = device.createBuffer({ size: SLOT * maxDraws, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
    this.data = new Float32Array(SLOT / 4);
    this.groups = new WeakMap();
    this.src = null;
    this.buf = null;
    this.n = 0;
  }

  frame() { this.n = 0; }

  #grow(t, w, h, format) {
    if (t && t.width >= w && t.height >= h) return t;
    t?.destroy();
    return this.device.createTexture({
      size: [Math.max(w, t?.width ?? 0), Math.max(h, t?.height ?? 0)], format,
      usage: GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST | GPUTextureUsage.RENDER_ATTACHMENT,
    });
  }

  #group(tex) {
    let g = this.groups.get(tex);
    if (!g) {
      g = this.device.createBindGroup({
        layout: this.layout,
        entries: [
          { binding: 0, resource: { buffer: this.uniforms, size: SLOT } },
          { binding: 1, resource: tex.createView() },
        ],
      });
      this.groups.set(tex, g);
    }
    return g;
  }

  /** `rects`: the glass, in screen pixels; `c`: dwmGlassConstants'. The
   *  screen is `scene`, W x H. Returns false when out of uniform slots. */
  encode(encoder, scene, W, H, rects, c) {
    if (!rects.length || this.n + 1 + rects.length > this.maxDraws) return false;
    const R = DWM_BLUR_RADIUS;
    const bufWidth = Math.max(1, W >> 2);
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const r of rects) {
      x0 = Math.min(x0, Math.floor(r.x)); y0 = Math.min(y0, Math.floor(r.y));
      x1 = Math.max(x1, Math.ceil(r.x + r.w)); y1 = Math.max(y1, Math.ceil(r.y + r.h));
    }
    // The buffer texels the vertical pass reads (one either side of the
    // stretch, the taps' rows), and the screen columns those read (the taps,
    // 2 + (position - 0.5) about 4 bx, and the bilinear neighbour).
    const bx0 = Math.max(0, Math.floor(x0 / 4) - 2), bx1 = Math.min(bufWidth, Math.ceil(x1 / 4) + 2);
    const by0 = Math.max(0, y0 - R - 1), by1 = Math.min(H, y1 + R + 2);
    const sx0 = Math.max(0, 4 * bx0 - 2 * R), sx1 = Math.min(W, 4 * bx1 + 2 * R + 4);
    const bw = bx1 - bx0, bh = by1 - by0, sw = sx1 - sx0;
    if (bw <= 0 || bh <= 0 || sw <= 0) return true;
    // The back buffer's pixels and the accumulation buffer, both 8 bits a
    // channel as DWM's B8G8R8A8 surfaces.
    this.src = this.#grow(this.src, sw, bh, this.format);
    this.buf = this.#grow(this.buf, bw, bh, 'rgba8unorm');
    encoder.copyTextureToTexture({ texture: scene, origin: [sx0, by0] }, { texture: this.src }, [sw, bh]);
    const slot = (rect, viewport) => {
      const f = this.data;
      f.fill(0);
      f.set(rect, 0); f.set(viewport, 4); f[6] = W; f[7] = H;
      f[8] = sx0; f[9] = by0; f[10] = bx0; f[11] = by0;
      f[12] = bufWidth; f[13] = c.blur;
      f.set(c.colour, 16); f.set(c.afterglow, 20);
      f.set(this.taps.positions, 24); f.set(this.taps.weights, 32);
      this.device.queue.writeBuffer(this.uniforms, this.n * SLOT, f);
      return this.n++ * SLOT;
    };
    let pass = encoder.beginRenderPass({
      colorAttachments: [{ view: this.buf.createView(), loadOp: 'load', storeOp: 'store' }],
    });
    pass.setPipeline(this.accumulate);
    pass.setBindGroup(0, this.#group(this.src), [slot([0, 0, bw, bh], [this.buf.width, this.buf.height])]);
    pass.draw(4);
    pass.end();
    pass = encoder.beginRenderPass({
      colorAttachments: [{ view: scene.createView(), loadOp: 'load', storeOp: 'store' }],
    });
    pass.setPipeline(this.compose);
    for (const r of rects) {
      pass.setBindGroup(0, this.#group(this.buf), [slot([r.x, r.y, r.w, r.h], [W, H])]);
      pass.draw(4);
    }
    pass.end();
    return true;
  }
}
