// A process created suspended runs nothing until it is resumed
// (suspended.c): the child of CreateProcess(CREATE_SUSPENDED) must not make
// its marker file while the parent waits, and must make it once resumed,
// ROUNDS times, with the parent's pipe busy meanwhile. Natively (gexc run;
// each round a native WoW64 child start) and on the page's shim.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 3300 };

const ROUNDS = 50;
const WAIT_MS = 2000;

export default (t) => legs(t, 'suspended', {
  want: new RegExp(`^suspended ok ${ROUNDS}`, 'm'),
  args: [ROUNDS, WAIT_MS],
  native: { timeout: 1500000 },
  page: { timeout: 1500000 },
});
