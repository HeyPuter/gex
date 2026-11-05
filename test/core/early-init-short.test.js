// A bundle laid while its program runs, failing with an archive that ends short: the
// files that never came are EIO, never ENOENT, and the system's phase is
// 'failed' (test/lib/early-init.mjs).
import { earlyInit } from '../lib/early-init.mjs';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 180 };

export default (t) => earlyInit(t, 'short');
