// A Windows program on gex's NT layer in a page (docs/nt-layer.md §10.2):
// the bundle is a tar (a staged Wine prefix under /wine plus the program),
// the program's windows are toplevels in the page's compositor, and its GL
// (opengl32 → /dev/gex-gl) runs on Mesa in the compositor's worker.
//
//   /nt.html?tar=/gex/nt/bundle.tar&exe=/t/window.exe&args=30&size=1024x768
//            [&gl=1&glbackend=wgpu][&env=K=V,K=V][&cwd=/t]
//
// What a driver reads: `window.__nt` = { out, status, compositor }.

import { untar } from './tar.js';
import { fetchBytes, runProgram } from './runtime.js';

globalThis.GEX_ARCH = 'x86';
const q = new URLSearchParams(location.search);
const logEl = document.getElementById('log');
const state = { out: '', status: null, error: null, phase: 'loading' };
globalThis.__nt = state;
const say = (s) => {
  state.out += s;
  logEl.textContent = state.out.slice(-4000);
};

const term = {
  rows: 40,
  cols: 120,
  write(d) {
    say(typeof d === 'string' ? d : new TextDecoder().decode(d));
  },
  writeln(s) {
    say(`${s}\n`);
  },
  onData() {
    return { dispose() {} };
  },
  onResize() {
    return { dispose() {} };
  },
  focus() {},
};

async function main() {
  const [w, h] = (q.get('size') || '1024x768').split('x').map(Number);
  const canvas = document.getElementById('out');
  canvas.width = w;
  canvas.height = h;
  canvas.style.width = `${w}px`;
  canvas.style.height = `${h}px`;
  state.phase = 'fetching';
  const tar = await fetchBytes(q.get('tar') || '/gex/nt/bundle.tar');
  const tree = untar(tar);
  const files = { ...tree.files };
  const exe = q.get('exe') || '/t/window.exe';
  if (!files[exe]) throw new Error(`${exe} is not in the bundle`);
  const args = [exe, ...(q.get('args') ? q.get('args').split(' ') : [])];
  const env = (q.get('env') || '').split(',').filter(Boolean);
  if (!env.some((e) => e.startsWith('GEX_WINE_PREFIX='))) env.push('GEX_WINE_PREFIX=/wine');
  if (!env.some((e) => e.startsWith('GEX_NT_STRICT='))) env.push('GEX_NT_STRICT=1');
  const gl = q.get('gl') === '1';
  state.phase = 'running';
  const { status } = await runProgram(term, {
    elf: files[exe],
    files,
    dirs: tree.dirs,
    modes: tree.modes,
    env,
    args,
    cwd: q.get('cwd') || exe.replace(/\/[^/]*$/, '') || '/',
    tty: false,
    gl: gl ? 'compositor' : null,
    glBackend: q.get('glbackend') || 'wgpu',
    wl: canvas,
    wlInput: canvas,
  });
  state.status = status;
  state.phase = 'done';
}

main().catch((e) => {
  state.error = String(e?.stack ?? e);
  state.phase = 'failed';
  say(`\n[nt-page] ${state.error}\n`);
});
