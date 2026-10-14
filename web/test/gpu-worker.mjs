// A guest with /dev/gex-gpu, for gpu-compositor.html.
import { Gex, Env } from '@gex/gex.mjs';
import { GpuProxy } from '@gex/gpu-bridge.mjs';

onmessage = async (e) => {
  const dec = new TextDecoder();
  class Out extends Env { write(fd, b) { postMessage(dec.decode(b)); } }
  const gex = new Gex(new Out());
  gex.useGpu(new GpuProxy(e.data));
  const get = async (u) => new Uint8Array(await (await fetch(u)).arrayBuffer());
  try {
    const entry = await gex.load(await get('/gex/gex_browser.wasm'), await get('/gex/test/seed.plain.wasm'),
      await get('/gex/test/gpu-smoke'), {}, ['/gpu-smoke'], {});
    postMessage('exit ' + await gex.run(entry));
  } catch (err) {
    postMessage('exit error ' + err + '\n' + err.stack);
  }
};
