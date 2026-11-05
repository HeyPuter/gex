// A guest function called from native code catches an exception its callee
// raised, in its own frame, and returns (strayret.c): a comparator with a
// function-tier unit, called by qsort, whose callee raises and whose SEH
// handler resumes past the call; then the same on new threads, a handler
// resuming in the start routine and one resuming past a qsort whose
// comparator raised. Prints "strayret ok N". Under WoW64 the raise goes
// through RtlRaiseException (strayret.c "rtl"): RaiseException is a PE
// there, and its registers are the raise's context. Natively (gexc run) and
// on the page's shim with the exe's unit, and on the page on tier-1. (The
// 32-bit legs' GEX_STRAY_TRAP=1 leg has no WoW64 meaning: its qsort is guest
// code, and no return goes to the pad.)
import { unitLegs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

export default (t) => unitLegs(t, 'strayret', { want: /^strayret ok [1-9]/m, args: ['rtl'] });
