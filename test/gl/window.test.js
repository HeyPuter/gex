// A GL window beside HostWindows: an X client's EGL window surface
// (/probes/gles-window, cleared to magenta), its frames presented into the
// window's own GL canvas over the page's window (dom-compositor.js
// `showGlCanvas`), and the page showing them, as the browser composited it.
// x86 only: gles-window is in that tree alone (it has the host's Xlib).
import { glWindow } from './xwindow.mjs';

export const meta = { hosts: ['browser'], arches: ['x86'], images: ['gl-probes'], timeout: 300, html: `<style>html, body { margin: 0; } #desk { position: relative; width: 800px; height: 600px; }</style>
<div id="desk"></div>` };

export default (t) => glWindow(t, 'gles-window');
