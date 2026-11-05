// Many processes making and closing handles around door calls
// (handles.c): the server's heap stays whole. Natively, with a Wayland
// compositor of gexc's own, and on the page; gex's log off in both.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'handles', {
  want: /^handles ok/m,
  native: { wayland: true, env: { GEX_LOG: 'off' } },
  page: { env: { GEX_LOG: 'off' } },
});
