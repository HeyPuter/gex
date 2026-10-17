// The GL device's WebGPU validation policy (web/src/gl-validation.js): errors
// are counted and the first kept, the first few logged, then one line per
// interval however many arrive. `node web/test/gl-validation.mjs`; prints
// "gl-validation ok".
import { ValidationLog } from '../src/gl-validation.js';

let failed = 0;
const check = (name, cond) => { console.log(`${cond ? 'ok  ' : 'FAIL'} ${name}`); if (!cond) failed++; };

let t = 0;
const lines = [];
const v = new ValidationLog({ first: 3, every: 1000, now: () => t, log: (...a) => lines.push(a.join(' ')) });
check('nothing yet', v.count === 0 && v.first === null && v.summary() === '');
for (let i = 0; i < 3; i++) v.note(`err ${i}`);
check('the first three logged', lines.length === 3 && lines[0].includes('err 0'));
// One a frame for a second: no more lines until the interval has passed.
for (let i = 3; i < 63; i++) { t += 10; v.note(`err ${i}`); }
check('quiet within the interval', lines.length === 3);
t += 1000;
v.note('err 63');
check('one line after it, with the count', lines.length === 4 && lines[3].includes('61 more') && lines[3].includes('64 in all'));
check('the first is kept', v.first === 'err 0' && v.last === 'err 63' && v.count === 64);
check('summary names it', v.summary().startsWith('validation=64 (first: err 0)'));

console.log(failed ? `gl-validation: ${failed} failed` : 'gl-validation ok');
process.exit(failed ? 1 : 0);
