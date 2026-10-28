// start()'s option checks (web/src/start-options.js): what passes, and what
// each mistake says. node web/test/check-start-options.mjs
import { checkOptions, OPTIONS } from '../src/start-options.js';

let failed = 0;
const ok = (opts) => {
  try { checkOptions(opts); } catch (e) { failed++; console.log(`FAIL ${JSON.stringify(opts)} threw: ${e.message}`); }
};
const no = (opts, want) => {
  try {
    checkOptions(opts);
    failed++;
    console.log(`FAIL ${JSON.stringify(opts)} passed`);
  } catch (e) {
    if (!e.message.includes(want)) { failed++; console.log(`FAIL ${JSON.stringify(opts)}: "${e.message}" lacks "${want}"`); }
  }
};

// What the callers in gex pass (nt-page.js, wine32-shell-min.html, the eject index.html).
ok({});
ok({ canvas: {}, wisp: 'wss://anura.pro/', size: { w: 1024, h: 768 }, baseUrl: new URL('http://x/'), onLog: () => {}, build: {} });
ok({ container: {}, size: 'fit', args: ['C:\\winlogon.exe'], env: { GEX_WIN_STATS: '1' }, onExit: () => {} });
ok({ container: {}, onProgress: () => {}, wisp: 'ws://localhost:6001' });
ok({ size: '800x600', env: ['A=1', 'B='], strace: 42, hostx: 'rootful', persist: { dir: '/home', db: 'x' }, debug: { cgen: 1 } });
ok({ strace: true, persist: false, uid: 0, ready: 'manual', compositor: false, args: undefined, env: null });
ok(Object.fromEntries(OPTIONS.map((k) => [k, undefined])));

no({ contaner: {} }, 'did you mean container?');
no({ onlog: () => {} }, 'did you mean onLog?');
no({ arg: [] }, 'did you mean args?');
no({ sizee: 'fit' }, 'did you mean size?');
no({ zzz: 1 }, 'start({ zzz }): no such option (the options: container,');
no({ size: '800*600' }, "start({ size: \"800*600\" }): 'fit', {w, h} or 'WxH'");
no({ size: { w: 0, h: 600 } }, "'fit', {w, h}");
no({ size: 800 }, "'fit', {w, h}");
no({ args: 'a b' }, 'start({ args: "a b" }): an array of strings');
no({ args: [1] }, 'an array of strings');
no({ env: ['NOEQUALS'] }, "['NAME=value', ...] or {NAME: value}");
no({ env: 'A=1' }, "['NAME=value', ...]");
no({ strace: 'yes' }, 'true (every process) or a pid');
no({ wisp: 'anura.pro' }, 'ws://... or wss://...');
no({ hostx: 'yes' }, "true, false or 'rootful'");
no({ persist: '/home' }, 'false, or {dir, db}');
no({ onExit: 'done' }, 'start({ onExit: "done" }): a function');
no({ uid: -1 }, 'a user id');
no({ cwd: 5 }, 'start({ cwd: 5 }): a string');
no(null, 'options is an object');

console.log(failed ? `${failed} failed` : 'start options: all checks pass');
process.exit(failed ? 1 : 0);
