// The root certificate store (rootstore.c): crypt32 fills ROOT from the
// host's bundle natively and from the dist's on the page.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'rootstore', { want: /^rootstore ok/m, build: { libs: ['-lcrypt32'] } });
