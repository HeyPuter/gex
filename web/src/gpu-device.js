// The WebGPU device a page's GPU work shares: Mesa (the guest's OpenGL, on
// gex_wgpu) and the compositor that draws the scene. One device, so a GL
// window's frame is a texture the compositor samples directly -- no second
// context and no copy between APIs.
//
// It asks for what Mesa's driver is built for, clamped to the adapter.

let pending = null;

/** The device, made once per Worker: { adapter, device }. */
export function requestGexDevice() {
  pending ??= (async () => {
    if (!navigator.gpu) throw new Error('navigator.gpu unavailable');
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
      // Not yet: asking for GL 4.1's texture limits (16384 texels, 2048
      // layers) and GL 4.3's compute ones makes the context GL 4.6, and then
      // Unity's 4.x shader variants draw Muck's 3D scene black (U, mp26).
      // Back once that is fixed; see docs/gl-path.md.
    };
    // `?gllimits=41|43` (GEX_GL_LIMITS, set by the compositor worker): GL 4.1
    // wants 16384 texels a side and 2048 array layers, GL 4.3's compute
    // shaders 1024 invocations a workgroup and 32 KiB of shared memory;
    // WebGPU's defaults (8192, 256; 256, 16 KiB) keep the context at GL 4.0.
    // Unity drops shaders whose target the context is below (Muck's grass
    // geometry shader: "Shader Unsupported ... All passes removed").
    const glLimits = Number(globalThis.GEX_GL_LIMITS) || 0;
    if (glLimits >= 41) {
      Object.assign(want, { maxTextureDimension1D: 16384, maxTextureDimension2D: 16384,
        maxTextureDimension3D: 2048, maxTextureArrayLayers: 2048 });
    }
    if (glLimits >= 43) {
      Object.assign(want, { maxComputeInvocationsPerWorkgroup: 1024, maxComputeWorkgroupSizeX: 1024,
        maxComputeWorkgroupSizeY: 1024, maxComputeWorkgroupSizeZ: 64, maxComputeWorkgroupStorageSize: 32768 });
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
