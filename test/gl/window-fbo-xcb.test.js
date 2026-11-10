// gl/window-xcb presenting as wgpu's GL backend does (gpui's, under x11rb):
// /probes/gles-window-fbo-xcb draws each frame into a renderbuffer of its own
// with no surface current, then makes the window's surface current, blits
// the renderbuffer to framebuffer 0, swaps, and leaves nothing current. Its
// window starts at 240x180 and is made 200x150 before one last frame, after
// which nothing is drawn: that frame must reach the page (the resize the
// swap notices must not take its buffer before the page shows it).
import { glWindow } from './xwindow.mjs';

export const meta = { hosts: ['browser'], arches: ['x86'], images: ['gl-probes'], timeout: 300, html: `<style>html, body { margin: 0; } #desk { position: relative; width: 800px; height: 600px; }</style>
<div id="desk"></div>` };

export default (t) => glWindow(t, 'gles-window-fbo-xcb', (out) => {
  t.eq('surface', out.find((l) => l.startsWith('surface')), 'surface 240x180 visual same');
});
