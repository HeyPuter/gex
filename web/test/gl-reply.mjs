// A batch's reply against the ring's reply area (web/src/gl-reply.js): what
// fits goes back whole; more is cut to the area, as the native host does,
// never refused. `node web/test/gl-reply.mjs`; prints "gl-reply ok".
import { fitReply } from '../src/gl-reply.js';

let failed = 0;
const check = (name, cond) => { console.log(`${cond ? 'ok  ' : 'FAIL'} ${name}`); if (!cond) failed++; };
const cap = 32 * 1024 * 1024;
let f = fitReply(0, cap);
check('no reply', f.len === 0 && f.cut === 0);
f = fitReply(cap, cap);
check('exactly the area', f.len === cap && f.cut === 0);
f = fitReply(cap + 4096, cap);
check('over it: cut to the area', f.len === cap && f.cut === 4096);
console.log(failed ? `gl-reply: ${failed} failed` : 'gl-reply ok');
process.exit(failed ? 1 : 0);
