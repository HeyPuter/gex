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
      if (!dev.compositor) throw new Error('MesaGl draws into the windows: make HostWindows, CompositorWindows or SingleCanvas too');
      dev.glChannel = await dev.rt.openGlChannel(d3d);
      this.#start = dev.rt.glStarter(dev.compositor, dev.glChannel, { dom: dev.dom, source }, dev.term);
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
