// A process killed while its threads are inside door calls (killdoor.c):
// the server stays whole. Natively, with a Wayland compositor of gexc's
// own, and on the page.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'killdoor', { want: /^killdoor ok/m, native: { wayland: true } });
