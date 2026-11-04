// runProgram's /dev/dsp (page/src/run-program.js `playAudio`): a program's
// tone is played by the page. runProgram's system starts every process
// adopted (host.mjs `spawnExec`), and the page starts its player when a
// process's Worker says where the audio ring is (worker.mjs 'audio'); an
// adopted process that said nothing left /dev/dsp played to nobody in every
// page built on runProgram (an ejected site's among them).
import { runProgram } from '../../page/src/run.js';
import { setGlobals } from '../../page/src/page-globals.js';
import { untar } from '../../page/src/tar.js';
import { unzstd } from '../../page/src/zfetch.js';
import { bytesOf } from '../../page/runtime/src/internal.js';

// The single-thread build's runProgram plays no /dev/dsp: its 'audio' message
// names a plain memory, which the page's player cannot read (WebAudio's
// mirror is how that build plays, test/core/audio.test.js).
export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 180, single: 'runProgram plays no /dev/dsp in this build (WebAudio does)' };

export default async function (t) {
  const { say, check, eq, until, timeout } = t;
  const dbg = t.debug();
  const sw = Object.fromEntries(Object.entries(dbg).filter(([k]) => k !== 'arch'));
  globalThis.GEX_PAGE_SEARCH = `?${new URLSearchParams(sw)}`;
  globalThis.GEX_ASSET_BASE = t.baseUrl;
  setGlobals((n) => sw[n] ?? null, { arch: dbg.arch ?? 'aarch64' });
  const tree = untar(await unzstd(await bytesOf(t.work('bb.tar.zst'))));

  let out = '';
  const term = {
    rows: 24, cols: 80,
    write: (s) => { out += typeof s === 'string' ? s : new TextDecoder().decode(s); },
    writeln: (s) => { out += `${s}\n`; },
    onData: () => ({ dispose() {} }),
    onResize: () => ({ dispose() {} }),
    focus() {},
  };
  check('no player before the program', globalThis.__gexAudio == null);
  const run = runProgram(term, {
    elf: tree.files[tree.hard['/bin/sh'] ?? '/bin/sh'],
    // Three seconds of 440 Hz, then a wait for the player's reports.
    args: ['/bin/sh', '-c', 'rt-probe tone 440 3000; echo "tone $?"; sleep 3'],
    files: tree.files,
    dirs: tree.dirs ?? [],
    modes: tree.modes,
    hard: tree.hard,
    symlinks: tree.links ?? {},
    xattrs: tree.xattrs,
    filesystem: 'memory',
    env: ['PATH=/bin:/usr/bin', 'HOME=/root'],
    cwd: '/',
  });
  run.catch((e) => { out += `\n[runProgram threw: ${e?.stack ?? e}]`; });
  const started = await until(() => globalThis.__gexAudio != null, 'the page\'s player', 60000).then(() => true, () => false);
  check('the page started its player', started, out.slice(-600));
  const loud = () => (globalThis.__gexAudio?.history ?? []).filter((r) => r.rms > 0.05);
  await until(() => loud().length > 0, 'a report with the tone', 30000).catch(() => {});
  const heard = globalThis.__gexAudio?.history ?? [];
  check('it played the tone (a report with nonzero rms)', loud().length > 0, JSON.stringify(heard.slice(-3)));
  const hz = loud().map((r) => r.hz).sort((a, b) => a - b)[loud().length >> 1];
  if (loud().length) check(`at 440 Hz (${hz?.toFixed(0)})`, Math.abs(hz - 440) < 30);
  say(`  ${heard.length} report(s), context ${globalThis.__gexAudio?.ctx?.state}`);
  const r = await timeout(run, 60000, 'the end');
  check('the tone was written', /^tone 0$/m.test(out.replace(/\r/g, '')), out.slice(-400));
  eq('the program\'s status', r.status, 0);
  await globalThis.__gexAudio?.close?.();
}
