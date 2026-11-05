/**
 * Does every switch reach a Worker as the page or the environment gave it?
 *
 * options.mjs holds the runtime's switches in one table. A page parses its
 * URL onto the globals (`pageOptions`), runtime.js copies them into a start
 * message (`startOptions`), worker.mjs sets them again in the Worker
 * (`workerOptions`), and run-node.mjs builds the same message from the
 * environment (`envOptions`). For each kind of value this checks what one
 * string becomes at each step, that a Worker ends with the page's globals,
 * and that the environment gives the message the page would.
 *
 * Usage: node test/fixtures/page/options.mjs
 */
import { pathToFileURL as gexPathToFileURL } from 'node:url';
import { join as gexJoin } from 'node:path';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || gexJoin(import.meta.dirname, '..', '..', '..');
const gexUrl = (p) => gexPathToFileURL(gexJoin(GEX_ROOT, p)).href;
const { OPTIONS, pageOptions, startOptions, workerOptions, envOptions } = await import(gexUrl('hosts/browser/js/options.mjs'));

let bad = 0;
const check = (what, ok, more = '') => {
  if (!ok) bad++;
  if (!ok) console.log(`FAIL ${what} ${more}`);
};
const clear = () => { for (const r of OPTIONS) delete globalThis[r[1]]; };
const row = (name) => OPTIONS.find((r) => r[0] === name);

// One row of each kind: [name, URL value, global wanted, message value wanted].
const cases = [
  ['trapStack', '1', 1, 1], ['trapStack', 'yes', 1, 1], ['trapStack', '', undefined, undefined],
  ['nullGuard', '1', 1, 1], ['nullGuard', '2', undefined, undefined],
  ['pinRegion', '4096', 4096, 4096], ['pinRegion', '0', 0, undefined],
  ['forkSkip', '0', 0, 0], ['forkSkip', '64', 64, 64], ['adoptAfter', '', 0, 0],
  ['doorStats', 'on', 1, 1], ['doorStats', '5000', 5000, 5000],
  ['wasmDlls', 'libz.so=/z.wasm', 'libz.so=/z.wasm', 'libz.so=/z.wasm'], ['wasmDlls', '', undefined, undefined],
  ['stracePid', '7', 7, 7],
];
for (const [name, val, global, sent] of cases) {
  const [, g, url, envKey, , to, worker] = row(name);
  const what = `${name}=${JSON.stringify(val)}`;
  clear();
  pageOptions((k) => (k === url ? val : null));
  check(`${what}: page global`, globalThis[g] === global, `(${globalThis[g]}, want ${global})`);
  const msg = startOptions(to);
  check(`${what}: start message`, msg[name] === sent, `(${msg[name]}, want ${sent})`);
  check(`${what}: nothing else sent`, Object.keys(msg).every((k) => k === name));
  if (worker) {
    clear();
    workerOptions(msg);
    const want = sent === undefined ? undefined : global;
    check(`${what}: Worker global`, globalThis[g] === want, `(${globalThis[g]}, want ${want})`);
  }
  if (envKey) {
    const e = envOptions({ [envKey]: val }, to);
    check(`${what}: from the environment`, e[name] === sent, `(${e[name]}, want ${sent})`);
  }
}

// Every row at once: the URL keys and names are distinct, and a page with all
// of them set sends each one where its row says, and a Worker gets them back.
const names = new Set(OPTIONS.map((r) => r[0]));
const urls = new Set(OPTIONS.map((r) => r[2]));
check('names distinct', names.size === OPTIONS.length);
check('URL keys distinct', urls.size === OPTIONS.length);
clear();
pageOptions(() => '3');
const extra = startOptions('extra');
const opts = startOptions('opts');
for (const [name, g, , , kind, to, worker] of OPTIONS) {
  const want = kind === 'one' ? undefined : kind === 'str' ? '3' : kind === 'flag' ? 1 : 3;
  const got = (to === 'extra' ? extra : opts)[name];
  check(`all set: ${name} sent`, got === want, `(${got}, want ${want})`);
  check(`all set: ${name} only in ${to}`, !(name in (to === 'extra' ? opts : extra)));
  if (worker && want !== undefined) {
    const kept = globalThis[g];
    delete globalThis[g];
    workerOptions(extra);
    check(`all set: ${name} in the Worker`, globalThis[g] === kept, `(${globalThis[g]}, want ${kept})`);
  }
}
clear();
check('nothing set: nothing sent', !Object.keys(startOptions('extra')).length && !Object.keys(envOptions({}, 'extra')).length);
console.log(bad ? `${bad} failed` : `options: ${OPTIONS.length} switches reach a Worker as given`);
process.exit(bad ? 1 : 0);
