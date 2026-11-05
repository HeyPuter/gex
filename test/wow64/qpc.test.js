// The performance counter for an i386 program (fixtures/wow64/qpc.c), a
// WoW64 process, natively and on the page's shim: PE ntdll answers
// NtQueryPerformanceCounter from gex.clock_ns with no system call, and what
// it answers must be the Unix side's counter: its 10 MHz frequency, monotonic
// within and across 4 threads, deltas agreeing with the system time.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'qpc', { want: /^qpc ok 5 5000/m, args: [4, 5000], build: { libs: ['-lntdll'] } });
