// The demo desktop (docs/gexcc.md §9): one page that is a desktop.
//
// The compositor's output fills the screen; the panel, the launcher and the
// console are the page's own DOM, so the shell chrome costs the guest's
// rootfs nothing. At start two windows are open: an xterm running bash (on the
// X server in this page) and the launcher. pid 1 is bash reading command lines
// from the page's terminal -- the console -- and the launcher starts a program
// by typing its command line there, after adding its files to the running
// system (overlay-worker.js).

import '@xterm/xterm/css/xterm.css';
import './desktop.css';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { loadRuntime, fetchBytes, runProgram } from './runtime.js';
import { untar } from './tar.js';
import { unzstd } from './zfetch.js';

const $ = (id) => document.getElementById(id);
const q = new URLSearchParams(location.search);
// `?strace=1` / `?stracepid=N`: narrate system calls (to the console), as
// the other pages do.
if (q.get('strace')) globalThis.GEX_STRACE = 1;
if (q.has('stracepid')) globalThis.GEX_STRACE_PID = Number(q.get('stracepid'));
const T0 = performance.now();
const marks = {};
const mark = (k) => {
  marks[k] = Math.round(performance.now() - T0);
};
globalThis.gexDesktop = { marks, ready: false };

// The page-wide unit cache, as the demo page has it (main.js).
globalThis.GEX_UNIT_CACHE = q.has('unitcache') ? Number(q.get('unitcache')) : 1;
if (q.get('debug')) {
  globalThis.GEX_DEBUG_EXIT = 1;
  globalThis.GEX_TRAP_STACK = 1;
}

// -- chrome -------------------------------------------------------------------

const kib = (n) => (n >= 1 << 20 ? `${(n / (1 << 20)).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`);
function splash(text, frac) {
  $('splash-text').textContent = text;
  if (frac !== undefined) $('splash-bar').style.width = `${Math.round(Math.min(1, frac) * 100)}%`;
}
function status(text) {
  $('pstatus').textContent = text;
}
setInterval(() => {
  $('clock').textContent = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}, 1000);
$('clock').textContent = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });

/** A page-side window: draggable by its title, closed by its button. */
let zTop = 20;
function dwin(el, { x, y, onClose } = {}) {
  if (x !== undefined) {
    el.style.left = `${x}px`;
    el.style.top = `${y}px`;
  }
  const raise = () => {
    el.style.zIndex = String(++zTop);
  };
  el.addEventListener('pointerdown', raise);
  const title = el.querySelector('.dwin-title');
  title.addEventListener('pointerdown', (e) => {
    if (e.target.closest('button')) return;
    const r = el.getBoundingClientRect();
    const dx = e.clientX - r.left;
    const dy = e.clientY - r.top;
    title.setPointerCapture(e.pointerId);
    const move = (m) => {
      el.style.left = `${Math.max(0, Math.min(innerWidth - 60, m.clientX - dx))}px`;
      el.style.top = `${Math.max(30, Math.min(innerHeight - 30, m.clientY - dy))}px`;
    };
    const up = () => {
      title.removeEventListener('pointermove', move);
      title.removeEventListener('pointerup', up);
    };
    title.addEventListener('pointermove', move);
    title.addEventListener('pointerup', up);
  });
  el.querySelector('.dwin-close').addEventListener('click', () => {
    el.classList.add('hidden');
    onClose?.();
  });
  return {
    show() {
      el.classList.remove('hidden');
      raise();
    },
    hide() {
      el.classList.add('hidden');
    },
    toggle() {
      if (el.classList.contains('hidden')) this.show();
      else this.hide();
      return !el.classList.contains('hidden');
    },
  };
}

const launcherWin = dwin($('launcher'), {
  x: Math.max(16, innerWidth - 340 - 40),
  y: 60,
  onClose: () => $('apps').classList.remove('on'),
});
const consoleWin = dwin($('console'), {
  x: 60,
  y: Math.max(60, innerHeight - 440),
  onClose: () => $('console-btn').classList.remove('on'),
});
$('apps').addEventListener('click', () => $('apps').classList.toggle('on', launcherWin.toggle()));

// The console: pid 1's terminal. Hidden unless asked for.
const term = new Terminal({
  fontFamily: 'ui-monospace, "SF Mono", Menlo, Consolas, monospace',
  fontSize: 12,
  convertEol: false,
  scrollback: 5000,
  theme: { background: '#0b0d10', foreground: '#d6dbe4' },
});
const fit = new FitAddon();
term.loadAddon(fit);
term.open($('term'));
$('console-btn').addEventListener('click', () => {
  const on = consoleWin.toggle();
  $('console-btn').classList.toggle('on', on);
  if (on) fit.fit();
});
window.gexTerminalText = () => {
  const b = term.buffer.active;
  const out = [];
  // A wrapped row continues the line above it, as it was written.
  for (let i = 0; i < b.length; i++) {
    const l = b.getLine(i);
    const t = l?.translateToString(true) ?? '';
    if (l?.isWrapped && out.length) out[out.length - 1] += t;
    else out.push(t);
  }
  return out.join('\n');
};

// -- the launcher ---------------------------------------------------------------

/** What the launcher offers, from the build's pkgs/index.json. */
async function catalogue() {
  try {
    const r = await fetch('/pkgs/index.json', { cache: 'no-cache' });
    if (r.ok && (r.headers.get('content-type') || '').includes('json')) return await r.json();
  } catch {}
  return { items: [] };
}

let system = null;
let sysReady = null;
const sysUp = new Promise((r) => (sysReady = r));
const installed = new Set();
const installing = new Map();
let overlayWorker = null;
let overlayStack;
const overlayWaiters = [];

function overlay() {
  if (overlayWorker) return overlayWorker;
  overlayWorker = new Worker(new URL('./overlay-worker.js', import.meta.url), { type: 'module' });
  overlayWorker.onmessage = (e) => {
    const m = e.data;
    if (m.progress) {
      const w = overlayWaiters.find((x) => x.name === m.progress);
      w?.onProgress?.(m.got, m.total);
      return;
    }
    const i = overlayWaiters.findIndex((x) => x.name === (m.done ?? m.name));
    if (i < 0) return;
    const [w] = overlayWaiters.splice(i, 1);
    if (m.error) w.reject(new Error(m.error));
    else w.resolve(m);
  };
  return overlayWorker;
}

/** Add one package's overlay to the running system (once). */
function install(item, cat, onProgress) {
  if (installed.has(item.id)) return Promise.resolve(null);
  if (installing.has(item.id)) return installing.get(item.id);
  const p = (async () => {
    for (const d of item.deps ?? []) {
      const dep = cat.items.find((x) => x.id === d) ?? cat.hidden?.find((x) => x.id === d);
      if (dep) await install(dep, cat, onProgress);
    }
    await sysUp;
    while (overlayStack === undefined) {
      overlayStack = system.host.takeKernelStack('overlay');
      if (overlayStack === undefined) await new Promise((r) => setTimeout(r, 100));
    }
    const r = await new Promise((resolve, reject) => {
      overlayWaiters.push({ name: item.id, resolve, reject, onProgress });
      overlay().postMessage({
        spec: system.kernel,
        stackTop: overlayStack,
        url: `/pkgs/${item.pkg ?? item.id}.tar.zst`,
        name: item.id,
      });
    });
    installed.add(item.id);
    console.log(`[desktop] ${item.id}: ${r.files} files, ${kib(r.bytes)} from ${kib(r.compressed)}; ` +
      `fetch ${r.fetchMs} ms, decode ${r.decodeMs} ms, write ${r.writeMs} ms`);
    globalThis.gexDesktop.installs = { ...(globalThis.gexDesktop.installs ?? {}), [item.id]: r };
    return r;
  })();
  installing.set(item.id, p);
  p.finally(() => installing.delete(item.id));
  return p;
}

/** `s` as one shell word. */
const shq = (s) => `'${s.replaceAll("'", "'\\''")}'`;

/** Type a command line to pid 1, which runs it in the background. */
function run(cmd) {
  system.tty?.input(`${cmd}\n`);
}
globalThis.gexDesktop.run = (cmd) => run(cmd);

async function launch(item, li, cat) {
  const state = li.querySelector('.istate');
  if (li.classList.contains('busy')) return;
  li.classList.add('busy');
  state.className = 'istate';
  try {
    if (!installed.has(item.id)) {
      state.textContent = 'downloading';
      await install(item, cat, (got, total) => {
        state.textContent = total ? `${Math.round((100 * got) / total)}%` : kib(got);
      });
    }
    state.textContent = 'starting';
    await sysUp;
    // The program, run as a shell would run it: the session's environment and
    // nothing for this item alone. A GL program starts the host's Mesa with
    // its first request, the same way from here as from the xterm (runtime.js
    // `glLazy`), so there is one path to the screen, not two.
    //
    // In a session of its own, off pid 1's terminal: a program that takes
    // the terminal (gtk3-demo does) would otherwise leave pid 1 unable to
    // read the next command line. Its output goes to /tmp/launch-ID.log.
    run(`setsid -f sh -c ${shq(item.cmd)} </dev/null >/tmp/launch-${item.id}.log 2>&1`);
    state.textContent = 'open';
    state.className = 'istate ok';
  } catch (e) {
    state.textContent = 'failed';
    state.className = 'istate bad';
    state.title = String(e.message ?? e);
    console.error(e);
  } finally {
    li.classList.remove('busy');
  }
}

async function fillLauncher() {
  const cat = await catalogue();
  const ul = $('items');
  ul.replaceChildren();
  for (const item of cat.items) {
    const li = document.createElement('li');
    li.dataset.id = item.id;
    const icon = document.createElement('div');
    icon.className = 'icon';
    icon.style.background = item.color ?? '#7aa2f7';
    icon.textContent = item.glyph ?? item.label.slice(0, 1).toUpperCase();
    const text = document.createElement('div');
    text.innerHTML = '<div class="iname"></div><div class="idesc"></div>';
    text.querySelector('.iname').textContent = item.label;
    text.querySelector('.idesc').textContent = item.desc ?? '';
    const state = document.createElement('div');
    state.className = 'istate';
    state.textContent = item.size ? kib(item.size) : '';
    if (item.base) {
      installed.add(item.id);
      state.textContent = 'installed';
    }
    li.append(icon, text, state);
    li.addEventListener('click', () => launch(item, li, cat));
    ul.append(li);
  }
  globalThis.gexDesktop.launch = (id) => {
    const li = ul.querySelector(`li[data-id="${id}"]`);
    const item = cat.items.find((x) => x.id === id);
    return item && li ? launch(item, li, cat) : Promise.reject(new Error(`no item ${id}`));
  };
  globalThis.gexDesktop.install = (id) => {
    const item = cat.items.find((x) => x.id === id) ?? cat.hidden?.find((x) => x.id === id);
    return item ? install(item, cat) : Promise.reject(new Error(`no item ${id}`));
  };
  return cat;
}

// -- boot ---------------------------------------------------------------------------

/** The page's density: the output is in device pixels (see `boot`). */
const DPR = Math.max(1, Number(q.get('dpr')) || globalThis.devicePixelRatio || 1);

/** X resources for this density. The X server's screen is in device pixels
 *  (X clients draw 1:1, never resampled), so at a high density an X client
 *  is told the DPI to size its fonts by, and xterm draws with an outline
 *  font instead of the 13-pixel bitmap `fixed`, which would be half size. */
function xdefaults() {
  if (DPR < 1.25) return '';
  return [
    `Xft.dpi: ${Math.round(96 * DPR)}`,
    'Xft.antialias: true',
    'Xft.hinting: true',
    'Xft.hintstyle: hintslight',
    'Xft.rgba: none',
    'XTerm*faceName: DejaVu Sans Mono',
    'XTerm*faceSize: 9',
    `Xcursor.size: ${Math.round(16 * DPR)}`,
    '',
  ].join('\n');
}

/** The guest's session: pid 1. */
function session(cat) {
  const xterm = q.get('xterm') ?? cat.xterm ?? 'xterm -geometry 92x30+24+24 -e bash -l';
  return [
    '# The desktop session (web/src/desktop.js): pid 1.',
    'export DISPLAY=:0 XDG_RUNTIME_DIR=/tmp WAYLAND_DISPLAY=wayland-0',
    'export HOME=/root SHELL=/bin/bash TERM=xterm-256color LANG=C.UTF-8',
    'export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin',
    'cd /root',
    `${xterm} &`,
    '# The launcher types a command line here; each runs in the background.',
    'while IFS= read -r line; do',
    '  [ -n "$line" ] || continue',
    '  ( eval "$line" ) &',
    'done',
    '',
  ].join('\n');
}

async function boot() {
  const canvas = $('out');
  // The backing store in device pixels, so nothing the compositor shows is
  // stretched by the browser: at devicePixelRatio 2 a canvas sized in CSS
  // pixels was every window's pixels doubled and smoothed.
  const deviceSize = () => {
    const r = canvas.getBoundingClientRect();
    return [Math.max(320, Math.round(r.width * DPR)), Math.max(240, Math.round(r.height * DPR))];
  };
  [canvas.width, canvas.height] = deviceSize();

  splash('fetching the runtime', 0);
  const rt = loadRuntime((got, total) => {
    splash(`fetching the runtime — ${kib(got)}${total ? ` of ${kib(total)}` : ''}`, total ? (0.5 * got) / total : undefined);
  });
  const catP = fillLauncher();
  const fsUrl = q.get('rootfs') || '/rootfs/base.tar.zst';
  const fsP = fetchBytes(fsUrl).then(async (z) => {
    mark('rootfsFetched');
    const raw = fsUrl.endsWith('.zst') ? await unzstd(z) : z;
    return { tree: untar(raw.buffer.slice(raw.byteOffset, raw.byteOffset + raw.byteLength)), z: z.length, raw: raw.length };
  });
  // The network (Wisp), for apt and anything else that reaches out: started
  // beside the downloads, and the desktop does not wait for a slow proxy
  // past the rootfs. `?wisp=` picks the proxy, `?wisp=` empty turns it off.
  const wisp = q.has('wisp') ? q.get('wisp') : 'wss://anura.pro/';
  const netP = wisp
    ? import('./net.js').then(async ({ WispNetwork }) => {
      const n = new WispNetwork(wisp, (e) => status(`network: ${e.split(' ')[0]}`));
      await n.start();
      mark('network');
      return n;
    }).catch((e) => {
      console.warn(`[desktop] no network: ${e.message ?? e}`);
      return null;
    })
    : Promise.resolve(null);
  await rt;
  mark('runtime');
  splash('fetching the system', 0.6);
  const [{ tree, z, raw }, cat] = await Promise.all([fsP, catP]);
  const network = await Promise.race([netP, new Promise((r) => setTimeout(() => r(null), 3000))]);
  if (!network) status('network: off');
  mark('rootfs');
  globalThis.gexDesktop.rootfs = { compressed: z, bytes: raw, files: Object.keys(tree.files).length };
  splash('starting X and the shell', 0.8);
  const files = { ...tree.files, '/etc/gex-desktop/session': new TextEncoder().encode(session(cat)) };
  const xd = xdefaults();
  if (xd) files['/root/.Xdefaults'] = new TextEncoder().encode(xd);
  const elf = files['/usr/bin/bash'];
  if (!elf) throw new Error(`${fsUrl} has no /usr/bin/bash`);

  // Both windows are up when the compositor has mapped one (the xterm: the
  // launcher is the page's own).
  const watch = setInterval(() => {
    const c = system?.compositor?.counters?.();
    if (c && c.windows >= 1) {
      clearInterval(watch);
      mark('desktop');
      $('splash').classList.add('gone');
      launcherWin.show();
      $('apps').classList.add('on');
      status('');
      globalThis.gexDesktop.ready = true;
      console.log(`[desktop] up: ${JSON.stringify(marks)}`);
      // The `auto` items (the core tools) come in behind the desktop, so the
      // xterm's shell has `ls` a moment later without the base carrying it.
      if (!q.has('noauto')) {
        for (const item of cat.items.filter((x) => x.auto)) {
          const li = $('items').querySelector(`li[data-id="${item.id}"]`);
          const st = li?.querySelector('.istate');
          if (st) st.textContent = 'adding';
          install(item, cat)
            .then(() => {
              if (st) { st.textContent = 'installed'; st.className = 'istate ok'; }
              mark(`auto:${item.id}`);
            })
            .catch((e) => console.error(e));
        }
      }
    }
  }, 50);

  const { status: st } = await runProgram(term, {
    elf,
    files,
    dirs: tree.dirs,
    modes: tree.modes,
    hard: tree.hard,
    symlinks: tree.links,
    env: [
      'PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin',
      'HOME=/root',
      'TERM=xterm-256color',
      'LANG=C.UTF-8',
      'DEBIAN_FRONTEND=noninteractive',
    ],
    args: ['/bin/bash', '/etc/gex-desktop/session'],
    wl: canvas,
    wlInput: canvas,
    wlScale: DPR,
    hostX: true,
    // GL programs marshal to Mesa in the compositor's Worker, started by the
    // first GL request from any program, so the first load does not carry it.
    gl: 'compositor',
    glLazy: true,
    cwd: '/root',
    tty: true,
    uid: 0,
    network,
    onSystem: (s) => {
      system = s;
      mark('system');
      // The output follows the window.
      let pending = 0;
      addEventListener('resize', () => {
        clearTimeout(pending);
        pending = setTimeout(() => s.compositor?.resize(...deviceSize()), 150);
      });
      sysReady(s);
    },
  });
  clearInterval(watch);
  status(`session ended (${st})`);
}

// `?diag=1`: what the compositor drew and what the page's canvas holds, twice
// a second, in a corner. A screen that does not change while "drawn" counts
// up and "held" changes is a browser not displaying frames the page has; a
// "drawn" that stays put is the compositor not seeing the client's frames.
// The corner box repaints the page twice a second, which may itself bring a
// stalled canvas up to date; `?diag=log` says the same in the console
// instead, every 2 s, and changes nothing on the page.
if (q.get('diag')) {
  const quiet = q.get('diag') === 'log';
  const box = document.createElement('div');
  box.style.cssText = 'position:fixed;right:8px;bottom:8px;z-index:9;font:12px monospace;'
    + 'background:#000c;color:#9f9;padding:4px 6px;pointer-events:none;white-space:pre';
  if (!quiet) document.body.append(box);
  let lastHeld = '';
  let ticks = 0;
  let heldChanges = 0;
  setInterval(() => {
    const comp = globalThis.__gexCompositor;
    const c = document.getElementById('out');
    let held = '-';
    try {
      const t = new OffscreenCanvas(c.width, c.height);
      const g = t.getContext('2d', { willReadFrequently: true });
      g.drawImage(c, 0, 0);
      const d = g.getImageData(0, 0, c.width, c.height).data;
      let h = 2166136261;
      for (let i = 0; i < d.length; i += 16) h = Math.imul(h ^ d[i] ^ (d[i + 1] << 8) ^ (d[i + 2] << 16), 16777619) >>> 0;
      held = h.toString(16);
    } catch (e) {
      held = `! ${e?.message ?? e}`;
    }
    if (held !== lastHeld) heldChanges++;
    lastHeld = held;
    const n = comp?.counters?.() ?? {};
    box.textContent = `drawn ${n.frames ?? '-'}  held ${held} (${heldChanges} changes)  ${c.width}x${c.height} dpr ${devicePixelRatio}`;
    if (quiet && ticks++ % 4 === 0) console.log(`[diag] ${box.textContent}`);
  }, 500);
}

boot().catch((e) => {
  console.error(e);
  splash(`could not start: ${e.message ?? e}`);
  status('failed');
  globalThis.gexDesktop.error = String(e?.stack ?? e);
});
