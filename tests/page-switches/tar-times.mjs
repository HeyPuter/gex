// The page's tar reader keeps a directory's time as well as a file's: the
// guest's caches built over a directory (KDE's ksycoca) compare with it.
//
//   node tests/page-switches/tar-times.mjs
import { execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, utimesSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { untar } from '../../web/src/tar.js';

const w = mkdtempSync(join(tmpdir(), 'gex-tar-times-'));
mkdirSync(join(w, 't/d'), { recursive: true });
writeFileSync(join(w, 't/d/f'), 'hi');
utimesSync(join(w, 't/d/f'), 1_100_000_000, 1_100_000_000);
utimesSync(join(w, 't/d'), 1_000_000_000, 1_000_000_000);
execFileSync('tar', ['-C', join(w, 't'), '-cf', join(w, 't.tar'), 'd']);
const t = untar(new Uint8Array(readFileSync(join(w, 't.tar'))));
rmSync(w, { recursive: true, force: true });
const got = { d: t.mtimes['/d'], f: t.mtimes['/d/f'] };
const ok = got.d === 1_000_000_000 && got.f === 1_100_000_000;
console.log(`${ok ? 'ok' : 'FAIL'} tar times: directory ${got.d}, file ${got.f}`);
process.exit(ok ? 0 : 1);
