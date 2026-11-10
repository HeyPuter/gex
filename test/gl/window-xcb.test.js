// gl/window for an XCB client: /probes/gles-window-xcb makes its window with
// libxcb alone (no Xlib in the process, as x11rb and so wgpu under gpui) and
// its EGL display and surface through EGL_EXT_platform_xcb. The surface has
// the window's size and the config the screen's root visual.
import { glWindow } from './xwindow.mjs';

export const meta = { hosts: ['browser'], arches: ['x86'], images: ['gl-probes'], timeout: 300, html: `<style>html, body { margin: 0; } #desk { position: relative; width: 800px; height: 600px; }</style>
<div id="desk"></div>` };

export default (t) => glWindow(t, 'gles-window-xcb', (out) => {
  t.eq('surface', out.find((l) => l.startsWith('surface')), 'surface 200x150 visual same');
});
