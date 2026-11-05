// RtlCaptureContext in a WoW64 process (capctx.c): the i386 layout and
// fields Wine Mono's call wrapper relies on. Natively (gexc run) and on the
// page's shim (run-node.mjs).
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'capctx', { want: /^capctx ok/m, build: { flags: ['-fno-omit-frame-pointer'], libs: ['-lntdll'] } });
