// A C++ rethrow from a catch block (fixtures/wow64/cxxrethrow.cpp, MSVC's
// x64 ABI through msvcrt): the frames below the catch were unwound when it
// was entered, and the rethrow's unwind must go on from the frame that
// caught, not unwind them a second time (a __finally run twice; under
// __CxxFrameHandler4, which keeps no state in the frame, destructors run
// again with another frame's state). Natively and on the page.
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { FIXTURES, wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

const llvm = (tool) => (existsSync(`/usr/lib/llvm-22/bin/${tool}`) ? `/usr/lib/llvm-22/bin/${tool}` : tool);

export default async function (t) {
  const w = wow64(t);
  const dir = join(w.work, 'cxxrethrow');
  mkdirSync(dir, { recursive: true });
  const defs = join(FIXTURES, '..', 'wine', 'src');
  const libs = ['msvcrt', 'kernel32'].map((n) => {
    const lib = join(dir, `${n}.lib`);
    execFileSync(llvm('llvm-dlltool'), ['-m', 'i386:x86-64', '-d', join(defs, `${n}-x64.def`), '-l', lib]);
    return lib;
  });
  const obj = join(dir, 'cxxrethrow.obj'), exe = join(dir, 'cxxrethrow.exe');
  execFileSync(llvm('clang-cl'), ['--target=x86_64-pc-windows-msvc', '/O1', '/GS-', '/EHa', '/c', join(FIXTURES, 'cxxrethrow.cpp'), `/Fo${obj}`]);
  execFileSync(llvm('lld-link'), ['/nodefaultlib', '/entry:entry', '/subsystem:console', '/dynamicbase', '/Brepro', obj, ...libs, `/out:${exe}`]);

  const want = /(ok [^\n]*\n)+0 failures/;
  const n = await w.native(['C:\\cxxrethrow.exe'], { files: { 'cxxrethrow.exe': exe } });
  t.check(`native: ${want.source}`, want.test(n.out.replace(/\r/g, '')), w.detail(n));
  const p = await w.page(['/t/cxxrethrow.exe'], { files: { '/t/cxxrethrow.exe': exe } });
  t.check(`page: ${want.source}`, want.test(p.out.replace(/\r/g, '')), w.detail(p));
  w.clean();
}
