// handle.drop()'s argument checks (page/src/drop.js checkDrop): what passes,
// and what each mistake says. node test/fixtures/page/drop-options.mjs
import { pathToFileURL as gexPathToFileURL } from 'node:url';
import { join as gexJoin } from 'node:path';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || gexJoin(import.meta.dirname, '..', '..', '..');
const gexUrl = (p) => gexPathToFileURL(gexJoin(GEX_ROOT, p)).href;
const { checkDrop } = await import(gexUrl('page/src/drop.js'));

let failed = 0;
const files = [new File(['a'], 'a.txt')];
const ok = (data, opts) => {
  try { checkDrop(data, opts); } catch (e) { failed++; console.log(`FAIL ${String(opts && JSON.stringify(opts))} threw: ${e.message}`); }
};
const no = (data, opts, want) => {
  try {
    checkDrop(data, opts);
    failed++;
    console.log(`FAIL ${JSON.stringify(opts)} passed`);
  } catch (e) {
    if (!e.message.includes(want)) { failed++; console.log(`FAIL ${JSON.stringify(opts)}: "${e.message}" lacks "${want}"`); }
  }
};

ok(files);
ok(files, {});
ok([], undefined);
ok(files, { x: 10, y: 20.5, dir: '/tmp/in', fallback: '/home/u/.wine/drive_c/users/u/Desktop/', onProgress: () => {} });
ok(files, { signal: new AbortController().signal, dir: null, fallback: null });

no(null, {}, 'drop(null): a DataTransfer, its items, a FileList or an array of Files');
no('a.txt', {}, 'drop("a.txt")');
no([{ name: 'a.txt' }], {}, 'an array of Files');
no(files, 'fast', 'the options are an object');
no(files, { onprogress: () => {} }, 'drop(data, { onprogress }): no such option (the options: x, y, dir,');
no(files, { x: '10', y: 20 }, "drop(data, { x: \"10\" }): a number, the drop event's clientX");
no(files, { x: 10 }, "drop(data, { x: 10 }): needs y too (the drop event's clientX and clientY)");
no(files, { x: 10, y: NaN }, 'drop(data, { y: null })');
no(files, { dir: 'tmp/in' }, 'drop(data, { dir: "tmp/in" }): an absolute guest path without .., not /');
no(files, { dir: '/tmp/../etc' }, 'an absolute guest path without .., not /');
no(files, { dir: '/' }, 'drop(data, { dir: "/" }): an absolute guest path without .., not /');
no(files, { fallback: 'C:\\Desktop' }, 'drop(data, { fallback: "C:\\\\Desktop" })');
no(files, { signal: {} }, 'drop(data, { signal: {} }): an AbortSignal');
no(files, { onProgress: 1 }, 'drop(data, { onProgress: 1 }): a function');

console.log(failed ? `${failed} failed` : 'drop options: all checks pass');
process.exit(failed ? 1 : 0);
