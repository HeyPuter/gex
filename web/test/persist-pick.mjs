// What a persist snapshot reads (web/src/persist-pick.js).
//
//   node web/test/persist-pick.mjs
//
// Checked: files the program wrote under the root are read; a stored file it
// has not written since (one restored from an earlier snapshot, with its old
// mtime) is kept without being read; a stored file that is gone is dropped;
// and nothing else under the root, such as a large lazy file, is looked at.
import assert from 'node:assert/strict';
import { pick } from '../src/persist-pick.js';

const fs = new Map([
  ['/pfx/user.reg', { file: true, size: 10, mtime: 1 }],
  ['/pfx/drive_c/new.ini', { file: true, size: 3, mtime: 1 }],
  ['/pfx/drive_c/restored.ini', { file: true, size: 4, mtime: 0 }],
  ['/pfx/drive_c/FL64.exe', { file: true, size: 3e9, mtime: 0 }],
  ['/pfx/drive_c/dir', { dir: true, file: false }],
  ['/tmp/outside', { file: true, size: 1, mtime: 1 }],
]);
const asked = [];
const meta = (p) => (asked.push(p), fs.get(p) ?? null);
const written = ['/pfx/user.reg', '/pfx/drive_c/new.ini', '/tmp/outside', '/pfx/drive_c/dir', '/pfx/user.reg'];
const known = ['/pfx/user.reg', '/pfx/drive_c/restored.ini', '/pfx/drive_c/deleted.ini'];

const got = pick('/pfx', written, known, meta);
assert.deepEqual([...got.keys()].sort(), ['/pfx/drive_c/new.ini', '/pfx/drive_c/restored.ini', '/pfx/user.reg']);
assert.equal(got.get('/pfx/user.reg').keep, undefined, 'a written stored file is read again');
assert.equal(got.get('/pfx/drive_c/new.ini').keep, undefined);
assert.equal(got.get('/pfx/drive_c/restored.ini').keep, true, 'a restored file is kept, not dropped');
assert.ok(!got.has('/pfx/drive_c/deleted.ini'), 'a stored file that is gone is dropped');
assert.ok(!asked.includes('/pfx/drive_c/FL64.exe'), 'an untouched file is never looked at');
assert.ok(!asked.includes('/tmp/outside'), 'nothing outside the root');
assert.deepEqual(pick('/pfx', [], [], meta).size, 0);
console.log('ok persist-pick');
