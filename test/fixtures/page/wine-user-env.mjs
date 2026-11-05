// The Windows user a Wine page runs as (page/src/wine-user.js): USER and
// LOGNAME gex, the user its prefix's profile is made for, unless the caller
// names one.
//
//   node test/fixtures/page/wine-user-env.mjs
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';
import assert from 'node:assert/strict';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || join(import.meta.dirname, '..', '..', '..');
const { pageUser } = await import(pathToFileURL(join(GEX_ROOT, 'page/src/wine-user.js')).href);
assert.deepEqual(pageUser(['WINEPREFIX=/pfx']), ['WINEPREFIX=/pfx', 'USER=gex', 'LOGNAME=gex'], 'none named: gex');
assert.deepEqual(pageUser(['USER=ann']), ['USER=ann', 'LOGNAME=gex'], "the caller's USER stays");
assert.deepEqual(pageUser(['LOGNAME=bob', 'USER=bob']), ['LOGNAME=bob', 'USER=bob'], "the caller's both stay");
assert.deepEqual(pageUser(['USERPROFILE=x', 'LOGNAMES=y']), ['USERPROFILE=x', 'LOGNAMES=y', 'USER=gex', 'LOGNAME=gex'], 'a longer name is not USER or LOGNAME');
const src = (await import('node:fs')).readFileSync(join(GEX_ROOT, 'page/src/app.js'), 'utf8');
assert.match(src, /pageUser\(env\)/, "app.js gives a Wine page's environment its user");
console.log('ok   wine user env: USER and LOGNAME gex unless the caller names them (5 cases)');
