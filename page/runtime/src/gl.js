// `MesaGl`: the guest's OpenGL, OpenGL ES (gunk), Vulkan (vick) and, with
// `d3d`, Direct3D (dirk) on the host's Mesa over WebGPU, drawn into the
// windows' own surfaces. Made before boot beside a windows class; Mesa
// (about 20 MB before compression) is fetched when a program first opens
// its GL, or at `start()`, not at boot.

import { internals } from './internal.js';

export class MesaGl extends EventTarget {
  #start = null;
  #started = null;

  /**
   * `eager`: fetch and start Mesa at boot rather than at first use; `d3d`:
   * the Direct3D device too (a Mesa built with dirk); `source(name)`: Mesa's
   * files from the page ('gex-wgpu-replay.js', '.wasm', '.deferred.wasm'),
   * resolving bytes or null for "fetch it" from the system's baseUrl.
   * Events: `started` (`description`), `failed` (`error`).
   */
  constructor(sys, { eager = false, d3d = false, source = null } = {}) {
    super();
    const i = internals(sys, 'MesaGl');
    i.attach('gl', this, { beforeBoot: true });
    // The GL ring the guest's /dev/gex-gl talks over, served by Mesa in the
    // compositor: started here at boot (`eager`), or by `start`, or by a
    // program's first GL call.
    i.devices(async (dev) => {
      // The single-thread build: Mesa (its non-pthread build, gex/st/) runs
      // in the system Worker, beside the guests' memories: in its scene
      // compositor, or beside a DOM desktop's arena (gl-in-realm.js). A
      // program's first GL call is said by a `gl-wanted` event.
      if (dev.single) {
        if (!dev.single.wl) throw new Error('MesaGl draws into the windows: make HostWindows, CompositorWindows or SingleCanvas too');
        const dom = dev.single.wl.dom;
        dev.single.gl = {
          d3d,
          attach: (compositor, listeners, k) => {
            this.#start = async () => {
              const { assetUrl, assetFiles } = await import('../../src/page-env.js');
              const opts = { api: 'core', url: assetUrl('/gex/st/gex-wgpu-replay.js'), files: assetFiles('/gex/st/') };
              // Beside a DOM desktop the device is the Worker's, relayed.
              if (dom) return compositor.hostGl(null, { device: (await import('../../src/gl-relay.js')).relayedGl(k, listeners, opts) });
              return compositor.hostGl(null, opts);
            };
            if (!eager) listeners.add((m) => { if (m.ev === 'gl-wanted') this.start().catch(() => {}); });
          },
        };
        return;
      }
      if (!dev.compositor) throw new Error('MesaGl draws into the windows: make HostWindows, CompositorWindows or SingleCanvas too');
      dev.glChannel = await dev.rt.openGlChannel(d3d);
      this.#start = dev.rt.glStarter(dev.compositor, dev.glChannel, { dom: dev.dom, source, ...(i.preset.glBackend ? { backend: i.preset.glBackend } : {}) }, dev.term);
      if (!eager) dev.rt.glOnFirstRequest(dev.glChannel, this.#start);
    }, 1);
    i.afterBoot(() => {
      if (eager) this.start();
    });
  }

  /**
   * Start Mesa now (a page that knows GL is coming saves the program the
   * wait). Resolves Mesa's description, or rejects if it would not start;
   * a program's first GL call starts it too, and both share one start.
   */
  start() {
    if (!this.#start) return Promise.reject(new Error('MesaGl: the system is not running'));
    this.#started ??= Promise.resolve(this.#start()).then((d) => {
      if (d == null) throw new Error(globalThis.__gexGlError ?? 'Mesa did not start');
      this.dispatchEvent(Object.assign(new Event('started'), { description: d }));
      return d;
    }, (e) => { throw e; });
    this.#started.catch((error) => this.dispatchEvent(Object.assign(new Event('failed'), { error })));
    return this.#started;
  }
}
