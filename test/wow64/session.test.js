// The session mirror (session.c): another process's window change shows
// at once. Natively, with a Wayland compositor of gexc's own, and on the
// page with Wine's null graphics driver.
import { legs, graphics } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'session', { want: /^session ok/m, native: { wayland: true }, page: { userReg: graphics('null') } });
