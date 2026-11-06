// A bundle laid while its program runs, failing with a fetch error: the
// files that never came are EIO, never ENOENT, and the system's phase is
// 'failed' (test/lib/early-init.mjs).
import { earlyInit } from '../lib/early-init.mjs';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 180, single: 'its bundle is laid whole before the program starts (no early start: a read cannot wait in the one Worker)' };

export default (t) => earlyInit(t, 'fetch');
