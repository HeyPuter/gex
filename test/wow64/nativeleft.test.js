// A guest exception handler that leaves native frames by jumping, as an MSVC
// __except handler does, then calls native code before it returns to its
// native caller (nativeleft.c): qsort calls a comparator with a
// function-tier unit, which calls qsort again with a comparator that
// raises; the outer comparator's handler unwinds and jumps back into the
// outer comparator, which calls GetCurrentThreadId and returns to the outer
// qsort. Then the same on new threads. Natively (gexc run) and on the
// page's shim, with the exe's unit and on tier-1.
import { unitLegs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

export default (t) => unitLegs(t, 'nativeleft', { want: /^nativeleft ok [1-9]/m });
