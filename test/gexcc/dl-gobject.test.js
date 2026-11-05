// A GObject program against a wasm32 GLib, natively under gexc and in the
// browser runtime. GEX_TEST_GLIB: the GLib install (include/glib-2.0,
// lib/), else a skip. The check is fixtures/gexcc/dl/gobject.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => {
  if (!process.env.GEX_TEST_GLIB) t.skip('GEX_TEST_GLIB: a wasm32 GLib install');
  return script(t, 'gobject', { env: { GLIB_DIR: process.env.GEX_TEST_GLIB } });
};
