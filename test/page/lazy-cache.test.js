// The page's cache of lazy files keys a file's chunks by its version: a file
// changed under its URL reads as its new bytes (fixtures/page/lazy-cache-changed.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'lazy-cache-changed.mjs'));
