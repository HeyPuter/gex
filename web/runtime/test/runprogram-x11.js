// The page runtime's own boot-time X server (runProgram's `hostX`), as the
// existing demo pages start it, beside the runtime API's on-demand one: both
// go through runtime.js startXServer.
import { main, check, until } from './lib.js';
import { setGlobals } from '../../src/page-globals.js';
import { runProgram } from '../../src/runtime.js';
import { untar } from '../../src/tar.js';
import { unzstd } from '../../src/zfetch.js';
import { bareDesktop } from '../../src/bare-desktop.js';

main(async () => {
  globalThis.GEX_PAGE_SEARCH = '?workergraph=0';
  setGlobals((n) => (n === 'workergraph' ? '0' : null), { arch: 'aarch64' });
  const t = untar(await unzstd(new Uint8Array(await (await fetch('/__rt/bb.tar.zst')).arrayBuffer())));
  const desk = document.getElementById('desk');
  let d = null;
  let out = '';
  const term = {
    rows: 24, cols: 80,
    write: (s) => { out += typeof s === 'string' ? s : new TextDecoder().decode(s); },
    writeln: (s) => { out += `${s}\n`; },
    onData: () => ({ dispose() {} }), onResize: () => ({ dispose() {} }), focus() {},
  };
  runProgram(term, {
    elf: t.files['/bin/rt-x11'], args: ['/bin/rt-x11', 'demo'], files: t.files, dirs: t.dirs, modes: t.modes, symlinks: t.links, hard: t.hard,
    env: ['DISPLAY=:0', 'PATH=/bin'], wl: desk, hostX: true, onDesktop: (x) => { d = bareDesktop(x, desk); },
  }).then((r) => { out += `\n[ended ${JSON.stringify(r?.status)}]`; }, (e) => { out += `\n[threw ${e}]`; });
  await until(async () => (await d?.describe?.())?.titles?.includes('demo'), 'the X window', 60000)
    .catch(async (e) => { throw new Error(`${e.message}: ${out.slice(-900)} ${JSON.stringify(await d?.describe?.())}`); });
  check('runProgram hostX shows an X window', true);
});
