// Code at reused addresses from several threads (vmreuse.c): a call
// returns the value just written. Natively, with a Wayland compositor of
// gexc's own, and on the page.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'vmreuse', { want: /^vmreuse ok/m, native: { wayland: true } });
