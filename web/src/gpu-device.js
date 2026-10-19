// The WebGPU device a page's GPU work shares: Mesa (the guest's OpenGL, on
// gex_wgpu) and the compositor that draws the scene. One device, so a GL
// window's frame is a texture the compositor samples directly -- no second
// context and no copy between APIs.
//
// It asks for what Mesa's driver is built for, clamped to the adapter.

let pending = null;

/**
 * Every GPUTexture and GPUBuffer this realm created and has not destroyed,
 * by kind and shape, with bytes: `gpuLive()`. VRAM a page cannot see any
 * other way -- nvidia-smi on the host is the only outside view -- and a leak
 * shows up here as a shape whose count climbs per frame. Objects that are
 * only dropped (no destroy()) stay counted: their memory comes back only
 * when the collector runs, which in a Worker that allocates little may be
 * never.
 */
const live = new Map(); // object -> key
const shapes = new Map(); // key -> { n, bytes, made, destroyed }
let tracking = false;
const calls = new Map(); // method -> [calls, bytes]
const FORMAT_BYTES = { rgba8unorm: 4, bgra8unorm: 4, 'rgba8unorm-srgb': 4, 'bgra8unorm-srgb': 4,
  depth24plus: 4, 'depth24plus-stencil8': 4, depth32float: 4, 'depth32float-stencil8': 8,
  rgba16float: 8, rgba32float: 16, r8unorm: 1, rg8unorm: 2, r32float: 4, depth16unorm: 2 };
function trackGpu() {
  if (tracking || typeof GPUDevice === 'undefined') return;
  tracking = true;
  const note = (obj, key, bytes) => {
    let e = shapes.get(key);
    if (!e) shapes.set(key, (e = { n: 0, bytes: 0, made: 0, destroyed: 0, each: bytes }));
    e.n++; e.made++; e.bytes += bytes;
    live.set(obj, key);
  };
  const gone = (obj) => {
    const key = live.get(obj);
    if (key === undefined) return;
    live.delete(obj);
    const e = shapes.get(key);
    e.n--; e.destroyed++; e.bytes -= e.each;
  };
  const ct = GPUDevice.prototype.createTexture;
  GPUDevice.prototype.createTexture = function (d) {
    const t = ct.call(this, d);
    const [w, h = 1, l = 1] = Array.isArray(d.size) ? d.size : [d.size.width, d.size.height ?? 1, d.size.depthOrArrayLayers ?? 1];
    const bytes = w * h * l * (FORMAT_BYTES[d.format] ?? 4) * (d.sampleCount ?? 1) * ((d.mipLevelCount ?? 1) > 1 ? 1.34 : 1);
    note(t, `texture ${w}x${h}x${l} ${d.format} usage 0x${d.usage.toString(16)}${(d.sampleCount ?? 1) > 1 ? ` x${d.sampleCount}` : ''}`, Math.round(bytes));
    return t;
  };
  const cb = GPUDevice.prototype.createBuffer;
  GPUDevice.prototype.createBuffer = function (d) {
    const b = cb.call(this, d);
    note(b, `buffer ${d.size} usage 0x${d.usage.toString(16)}`, d.size);
    return b;
  };
  // Calls by name, with bytes where a call carries them: a leak that is not
  // a texture or a buffer (a canvas frame, an upload's staging) shows up as
  // the one count that climbs with it. Only when asked for
  // (`globalThis.GEX_GPU_CALLS`, set before the device is made): these wrap
  // Mesa's per-draw calls, and a wrapper there costs frame time.
  const count = (proto, name, bytesOf) => {
    const f = proto?.[name];
    if (typeof f !== 'function' || !globalThis.GEX_GPU_CALLS) return;
    proto[name] = function (...a) {
      let e = calls.get(name);
      if (!e) calls.set(name, (e = [0, 0]));
      e[0]++;
      if (bytesOf) { try { e[1] += bytesOf(a) || 0; } catch {} }
      return f.apply(this, a);
    };
  };
  const ext = (a) => { const s = a[2] ?? a[3]; return Array.isArray(s) ? s[0] * (s[1] ?? 1) * 4 : (s?.width ?? 0) * (s?.height ?? 1) * 4; };
  count(GPUQueue.prototype, 'writeBuffer', (a) => (a[4] !== undefined ? a[4] * (a[2]?.BYTES_PER_ELEMENT ?? 1) : a[2]?.byteLength ?? 0));
  count(GPUQueue.prototype, 'writeTexture', (a) => a[1]?.byteLength ?? 0);
  count(GPUQueue.prototype, 'copyExternalImageToTexture', ext);
  count(GPUQueue.prototype, 'submit');
  count(globalThis.GPUCanvasContext?.prototype, 'getCurrentTexture');
  count(GPUTexture.prototype, 'createView');
  count(GPUBuffer.prototype, 'mapAsync');
  count(GPUBuffer.prototype, 'getMappedRange');
  count(GPUCommandEncoder.prototype, 'copyTextureToTexture', ext);
  count(GPUCommandEncoder.prototype, 'copyTextureToBuffer', ext);
  count(GPUCommandEncoder.prototype, 'copyBufferToTexture', ext);
  count(GPUCommandEncoder.prototype, 'beginRenderPass');
  count(GPUCommandEncoder.prototype, 'beginComputePass');
  for (const m of ['createBindGroup', 'createCommandEncoder', 'createQuerySet', 'createSampler', 'createShaderModule',
    'createRenderPipeline', 'createComputePipeline', 'importExternalTexture', 'createBindGroupLayout', 'createPipelineLayout'])
    count(GPUDevice.prototype, m);
  for (const proto of [GPUTexture.prototype, GPUBuffer.prototype]) {
    const destroy = proto.destroy;
    proto.destroy = function () {
      gone(this);
      return destroy.call(this);
    };
  }
}

/** What `trackGpu` holds: the live total and the shapes with the most
 *  bytes, as lines. */
export function gpuLive(top = 8) {
  let n = 0, bytes = 0;
  for (const e of shapes.values()) { n += e.n; bytes += e.bytes; }
  const rows = [...shapes.entries()].filter(([, e]) => e.n > 0).sort((a, b) => b[1].bytes - a[1].bytes).slice(0, top)
    .map(([k, e]) => `${k}: ${e.n} live (${(e.bytes / 1048576).toFixed(1)} MiB), ${e.made} made, ${e.destroyed} destroyed`);
  const counts = Object.fromEntries([...calls.entries()].map(([k, [c, b]]) => [k, b ? [c, +(b / 1048576).toFixed(1)] : c]));
  return { tracking, objects: n, mib: +(bytes / 1048576).toFixed(1), calls: counts, rows };
}

/** The device, made once per Worker: { adapter, device }. */
export function requestGexDevice() {
  pending ??= (async () => {
    if (!navigator.gpu) throw new Error('navigator.gpu unavailable');
    trackGpu();
    // The first request can return null while Chromium's GPU process is
    // still starting (seen on slower hosts); retry for up to ~5 s.
    let adapter = null;
    for (let i = 0; i < 20 && !adapter; i++) {
      adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
      if (!adapter) await new Promise((r) => setTimeout(r, 250));
    }
    if (!adapter) throw new Error('WebGPU adapter unavailable');
    // Each of these gates a GL extension st/mesa derives from format
    // support: BC compression, ARB_depth_buffer_float, EXT_packed_float, and
    // ARB_depth_clamp. clip-distances is not a format feature -- it is what
    // lets a translated shader declare `enable clip_distances`, which is how
    // gl_ClipDistance reaches WGSL at all. Absent ones are simply not
    // requested; the driver asks the device what it actually got.
    const optional = ['texture-compression-bc', 'depth32float-stencil8',
                      'rg11b10ufloat-renderable', 'depth-clip-control',
                      'float32-filterable', 'float32-blendable',
                      'clip-distances', 'dual-source-blending',
                      'timestamp-query'];
    const requiredFeatures = optional.filter((f) => adapter.features.has(f));
    // A device gets WebGPU's *default* limits unless it asks for more, and
    // those are well below what this adapter can do -- storage buffers in the
    // vertex stage default to none at all. st/mesa needs 13 constant buffers
    // per stage for ARB_uniform_buffer_object (GL 3.1); WebGPU guarantees only
    // 12, so the driver backs the rest with storage buffers. Eight RGBA8 draw
    // buffers is exactly WebGPU's default 32 bytes per sample, and GL 3.2
    // wants eight of whatever format the application chose.
    const want = {
      maxUniformBuffersPerShaderStage: 16,
      maxStorageBuffersPerShaderStage: 16,
      maxStorageBuffersInVertexStage: 16,
      maxStorageBuffersInFragmentStage: 16,
      maxColorAttachmentBytesPerSample: 64,
    };
    // `?gllimits=40|41|43` (GEX_GL_LIMITS, set by the compositor worker;
    // 43 when unset): GL 4.1 wants 16384 texels a side and 2048 array
    // layers, GL 4.3's compute shaders 1024 invocations a workgroup and
    // 32 KiB of shared memory; WebGPU's defaults (8192, 256; 256, 16 KiB)
    // keep the context at GL 4.0. Unity drops shaders whose target the
    // context is below: at 4.0 Muck's grass geometry shader ("Shader
    // Unsupported ... All passes removed") left the grass as dots; at 4.2
    // with mesa-wink 60202246 it draws blades (U, mpg3).
    //
    // Compute is the adapter's own limits, not just GL's minimum: a Windows
    // driver's context is 4.6 with compute, and games look for it whatever
    // version they ask for. Mosa Lina asks for 3.2 core, then gives up
    // ("OpenGL 3.2+ is required") when wglGetProcAddress has no
    // glDispatchCompute -- st/mesa only advertises ARB_compute_shader at
    // 1024 invocations a workgroup, and the device's default is 256.
    const glLimitsSet = globalThis.GEX_GL_LIMITS;
    const glLimits = glLimitsSet === undefined ? 43 : Number(glLimitsSet) || 0;
    if (glLimits >= 41) {
      Object.assign(want, { maxTextureDimension1D: 16384, maxTextureDimension2D: 16384,
        maxTextureDimension3D: 2048, maxTextureArrayLayers: 2048 });
    }
    if (glLimits >= 43) {
      for (const limit of ['maxComputeInvocationsPerWorkgroup', 'maxComputeWorkgroupSizeX',
        'maxComputeWorkgroupSizeY', 'maxComputeWorkgroupSizeZ', 'maxComputeWorkgroupStorageSize',
        'maxComputeWorkgroupsPerDimension']) want[limit] = Infinity;
    }
    const requiredLimits = {};
    for (const [limit, wanted] of Object.entries(want)) {
      const available = adapter.limits[limit];
      if (typeof available === 'number' && available > 0) requiredLimits[limit] = Math.min(available, wanted);
    }
    const device = await adapter.requestDevice({ requiredFeatures, requiredLimits });
    // Out of reach of the collector for as long as the page lives: a device
    // that is collected resolves `lost` with "destroyed" and stops without
    // failing a single call.
    (globalThis.__gexWebGPU ||= []).push({ adapter, device, queue: device.queue });
    return { adapter, device };
  })();
  pending.catch(() => { pending = null; });
  return pending;
}

/**
 * A canvas whose 'webgpu' context draws into a texture this side owns.
 *
 * Mesa presents into a canvas (emdawnwebgpu: configure, then
 * getCurrentTexture each frame, and gex_gl_display copies the frame there).
 * Given this one, the frame lands in `texture()` instead: a texture on the
 * shared device, sampled by the compositor like any window's, and kept until
 * the next frame replaces it (a real canvas texture expires at present).
 */
export function textureCanvas() {
  const canvas = new OffscreenCanvas(16, 16);
  let config = null;
  let texture = null;
  let serial = 0;
  const context = {
    canvas,
    configure(c) {
      config = c;
    },
    unconfigure() {
      config = null;
    },
    getConfiguration() {
      return config;
    },
    getCurrentTexture() {
      if (!config) throw new Error('the GL canvas is not configured');
      if (!texture || texture.width !== canvas.width || texture.height !== canvas.height || texture.format !== config.format) {
        texture?.destroy();
        texture = config.device.createTexture({
          size: [canvas.width, canvas.height],
          format: config.format,
          usage: (config.usage ?? GPUTextureUsage.RENDER_ATTACHMENT)
            | GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST | GPUTextureUsage.COPY_SRC,
          viewFormats: config.viewFormats ?? [],
        });
      }
      serial++;
      return texture;
    },
  };
  const own = canvas.getContext.bind(canvas);
  canvas.getContext = (type, options) => (type === 'webgpu' ? context : own(type, options));
  return { canvas, texture: () => texture, serial: () => serial };
}
