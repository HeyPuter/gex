// SQLite WAL readers against writers in other processes
// (fixtures/gexcc/locks): walheld.c's writer exits after each commit,
// walkeep.c's stays open. Each is built with the host's cc and with gexcc
// against the wasm32 tree's libsqlite3 and run on both sides: the outputs
// and exit statuses must match.
//
// GEX_TEST_SQLITE_LIB: a directory holding the wasm32 tree's
// libsqlite3.so.0 (a runtime tree's usr/lib/wasm32-linux-musl is enough).
// The host's libsqlite3 is the native side's, and the host's sqlite3 CLI
// walheld's writer on both (so x86-64 only: the guest runs it too).
import { existsSync, mkdirSync, readdirSync, symlinkSync } from 'node:fs';
import { join } from 'node:path';
import { FIXTURES, gexcc } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const lib = process.env.GEX_TEST_SQLITE_LIB;
  if (!lib) t.skip('GEX_TEST_SQLITE_LIB: the wasm32 tree\'s libsqlite3.so.0 directory');
  if (!existsSync(join(lib, 'libsqlite3.so.0'))) t.skip(`no libsqlite3.so.0 in ${lib}`);
  const multi = readdirSync('/usr/lib').find((d) => d.endsWith('-linux-gnu') && existsSync(`/usr/lib/${d}/libsqlite3.so.0`));
  const hostlib = multi ? `/usr/lib/${multi}/libsqlite3.so.0` : '-lsqlite3';
  if (!existsSync('/usr/bin/sqlite3')) t.skip('no /usr/bin/sqlite3 (walheld\'s writer)');
  const c = gexcc(t);
  const d = c.dir('locks');
  mkdirSync(join(d, 'lib'));
  // the link name -lsqlite3 looks for, beside the runtime's soname
  symlinkSync(join(lib, 'libsqlite3.so.0'), join(d, 'lib', 'libsqlite3.so'));
  for (const name of ['walheld', 'walkeep']) {
    const src = join(FIXTURES, 'locks', `${name}.c`);
    await c.hostBuild(['-O1', '-w', '-o', join(d, `${name}.native`), src, hostlib]);
    await c.build(['-O1', '-w', '-o', join(d, name), src, `-L${join(d, 'lib')}`, '-lsqlite3']);
    const want = await c.run([join(d, `${name}.native`)], { cwd: d, timeout: 300000 });
    const got = await c.run([join(d, name)], { cwd: d, timeout: 300000 });
    t.check(`${name} printed something natively`, want.out.trim() !== '');
    t.eq(name, { code: got.code, out: got.out }, { code: want.code, out: want.out });
  }
  c.clean();
}
