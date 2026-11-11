// gex_window_placement_v1's desktop layer (version 3) in the scene compositor
// (CompositorWindows, WebGPU; says so and passes where the browser has none):
// a window system's own desktop window -- Wine's virtual desktop, which draws
// the background and its icons -- under every other window, placed or not. A
// normal window sent to the bottom of its layer (place_below 0xffffffff)
// stays above it, and a press over the bare desktop reaches the desktop's
// client (test/lib/desktop-layer.mjs). The element layout's is
// desktop-layer-single.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { CompositorWindows } from '../../page/runtime/src/windows/compositor.js';
import { desktopLayer } from '../lib/desktop-layer.mjs';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 180, html: `<style>html, body { margin: 0; }</style>
<canvas id="out" width="640" height="480"></canvas>` };

export default async function (t) {
  const adapter = await navigator.gpu?.requestAdapter().catch(() => null);
  if (!adapter) return t.skip('no WebGPU adapter here');
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  new MemoryFilesystem(sys).addTar(t.work('bb.tar.zst'));
  const canvas = document.getElementById('out');
  const out = new CompositorWindows(sys, { canvas, scale: 1 });
  await desktopLayer(t, sys, out, canvas.getBoundingClientRect());
}
