// A page prefix carries nothing of the build machine's user
// (wine/page-prefix.sh): from a fake installed prefix, win32 and win64, made
// as user "builder" whose Downloads Wine linked to the build machine's
// ~/Downloads (a file planted there), with a nameless profile (users/AppData,
// a game's save marker in it) and another user's folder, the template must
// have:
//   - no planted file, no byte of it, no link but dosdevices' c: and z:;
//   - users/ holding only Public and gex, builder's own files under gex and
//     each of its links an empty folder;
//   - no C:\users\builder, \users\builder (HOMEPATH) or USERNAME builder in
//     its registry, which names gex instead.
import { existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { exec, tail, ROOT } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 120 };

const PLANT = 'PLANTED-HOST-FILE-7f3a';

/** Every path under `dir`, with lstat's answer, not following links. */
function* walk(dir) {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    yield [p, e];
    if (e.isDirectory()) yield* walk(p);
  }
}

const reg = (arch, rel, body) => `WINE REGISTRY Version 2\n;; All keys relative to \\\\${rel}\n\n#arch=${arch}\n\n${body}`;

export default async function (t) {
  const P = join(ROOT, 'wine/page-prefix.sh');
  if (!existsSync(P)) t.skip(`no ${P}`);
  const W = join(t.workDir, 'native', `page-prefix-user-${t.arch}-${process.pid}`);
  rmSync(W, { recursive: true, force: true });
  const mk = (...p) => mkdirSync(join(...p), { recursive: true });
  mk(W, 'home/Downloads');
  mk(W, 'dist/share/wine');
  writeFileSync(join(W, 'home/Downloads/secret.txt'), `${PLANT}\n`);
  writeFileSync(join(W, 'dist/share/wine/wine.inf'), '; wine.inf\n');
  writeFileSync(join(W, 'none.reg'), 'REGEDIT4\n'); // no session keys: only the users are tested
  for (const arch of ['win32', 'win64']) {
    const s = join(W, `src-${arch}`);
    const o = join(W, `out-${arch}`);
    const u = join(s, 'drive_c/users');
    for (const d of ['builder/Desktop', 'builder/AppData/Roaming', 'Public/Desktop', 'AppData/LocalLow/Game_Studio/save00', 'other/Documents']) mk(u, d);
    mk(s, 'drive_c/windows/system32');
    symlinkSync(join(W, 'home/Downloads'), join(u, 'builder/Downloads'));
    symlinkSync(join(W, 'home'), join(u, 'builder/Music'));
    writeFileSync(join(u, 'builder/Desktop/keep.txt'), 'keep\n');
    writeFileSync(join(u, 'AppData/LocalLow/Game_Studio/save00/DONT PLACE ANY FILES HERE'), '');
    writeFileSync(join(u, 'other/Documents/theirs.txt'), 'theirs\n');
    writeFileSync(join(s, 'drive_c/windows/system32/kernel32.dll'), 'dll\n');
    writeFileSync(join(s, 'system.reg'), reg(arch, 'Machine',
      '[Software\\\\Microsoft\\\\Windows NT\\\\CurrentVersion\\\\ProfileList\\\\S-1-5-21-0-0-0-1000] 1791289000\n"ProfileImagePath"="C:\\\\users\\\\builder"\n'));
    writeFileSync(join(s, 'user.reg'), reg(arch, 'User\\\\S-1-5-21-0-0-0-1000',
      '[Software\\\\Microsoft\\\\Windows\\\\CurrentVersion\\\\Explorer\\\\Shell Folders] 1791289000\n"Desktop"="C:\\\\users\\\\builder\\\\Desktop"\n' +
      '"{374DE290-123F-4565-9164-39C4925E467B}"="C:\\\\users\\\\builder\\\\Downloads"\n"Builders"="C:\\\\users\\\\builders\\\\x"\n\n' +
      '[Volatile Environment] 1791289000\n"HOMEPATH"="\\\\users\\\\builder"\n"USERNAME"="builder"\n"PATH"="C:\\\\windows;C:\\\\users\\\\builder\\\\bin"\n'));
    writeFileSync(join(s, 'userdef.reg'), '');
    const r = await exec(['bash', P, s, join(W, 'dist'), o, join(W, 'none.reg')], { env: { ...process.env, HOME: join(W, 'home') }, timeout: 60000 });
    if (r.code !== 0) {
      t.check(`${arch}: page-prefix.sh runs`, false, tail(r.stdout + r.stderr));
      continue;
    }
    const all = [...walk(o)];
    const files = all.filter(([, e]) => e.isFile());
    const links = all.filter(([p, e]) => e.isSymbolicLink() && !p.startsWith(join(o, 'dosdevices') + '/')).map(([p]) => p);
    const users = readdirSync(join(o, 'drive_c/users')).sort();
    const dl = join(o, 'drive_c/users/gex/Downloads');
    const userReg = readFileSync(join(o, 'user.reg'), 'utf8');
    const sysReg = readFileSync(join(o, 'system.reg'), 'utf8');
    const regs = readdirSync(o).filter((f) => f.endsWith('.reg')).map((f) => readFileSync(join(o, f), 'utf8')).join('\n');
    t.check(`${arch}: the planted file is not in it`, !files.some(([p]) => p.endsWith('/secret.txt')));
    t.check(`${arch}: nor its bytes`, !files.some(([p]) => readFileSync(p).includes(PLANT)));
    t.check(`${arch}: no links but dosdevices'`, links.length === 0, links.join(' '));
    t.eq(`${arch}: users holds Public and gex`, users, ['Public', 'gex']);
    t.check(`${arch}: builder's Desktop file is under gex`, existsSync(join(o, 'drive_c/users/gex/Desktop/keep.txt')));
    t.check(`${arch}: gex's Downloads is an empty folder`, existsSync(dl) && lstatSync(dl).isDirectory() && readdirSync(dl).length === 0);
    t.check(`${arch}: the registry does not name \\users\\builder`, !/users\\\\builder(\\\\|"|;)/i.test(regs));
    t.check(`${arch}: USERNAME is not builder`, !/^"USERNAME"="builder"/m.test(userReg));
    t.check(`${arch}: HOMEPATH names gex`, /^"HOMEPATH"="\\\\users\\\\gex"/m.test(userReg));
    t.check(`${arch}: Shell Folders names gex`, userReg.includes('"Desktop"="C:\\\\users\\\\gex\\\\Desktop"'));
    t.check(`${arch}: ProfileList names gex`, sysReg.includes('"ProfileImagePath"="C:\\\\users\\\\gex"'));
    t.check(`${arch}: a longer name that starts with builder is kept`, userReg.includes('users\\\\builders\\\\x'));
  }
  rmSync(W, { recursive: true, force: true });
}
