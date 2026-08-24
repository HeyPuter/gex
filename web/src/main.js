// The demo page: several demos, one terminal, one runtime. Each demo says what
// it needs and the chrome around it handles fetching, progress and the terminal.

import '@xterm/xterm/css/xterm.css';
import './style.css';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import { loadRuntime, fetchBytes, runProgram } from './runtime.js';
import { untar } from './tar.js';
import { DEMOS } from './demos.js';

const $ = (id) => document.getElementById(id);

const ui = {
  status: $('status'),
  hint: $('hint'),
  title: $('demo-title'),
  blurb: $('demo-blurb'),
  actions: $('actions'),
  overlay: $('overlay'),
  overlayText: $('overlay-text'),
  engine: $('stat-engine'),
  blocks: $('stat-blocks'),
  net: $('stat-net'),
};

const term = new Terminal({
  fontFamily: 'ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Consolas, monospace',
  fontSize: 13,
  lineHeight: 1.25,
  cursorBlink: true,
  // Off deliberately: it would turn every `\n` into `\r\n`, doing the kernel's
  // `ONLCR` job and overriding a program that clears `OPOST` (`less`, `vi`).
  // The guest's tty honours the flag itself; see `gex_kernel::tty::Tty::output`.
  convertEol: false,
  scrollback: 5000,
  theme: {
    background: '#0b0d10',
    foreground: '#d6dbe4',
    cursor: '#6ee7a8',
    selectionBackground: '#2f3a46',
    black: '#1a1e25', red: '#f07a7a', green: '#6ee7a8', yellow: '#f0b866',
    blue: '#7aa2f7', magenta: '#bb9af7', cyan: '#7dcfff', white: '#c0caf5',
  },
});
// The whole scrollback, for a test rather than a person: reading `.xterm-rows`
// would lose everything above the fold, where a failure's first line is.
window.gexTerminalText = () => {
  const b = term.buffer.active;
  const out = [];
  for (let i = 0; i < b.length; i++) {
    out.push(b.getLine(i)?.translateToString(true) ?? '');
  }
  return out.join('\n');
};

// Typing, for a test: the sibling of `gexTerminalText`, letting one drive a
// prompt (with job control and a raw-mode line editor) rather than `?cmd=`'s
// script. `term.input` goes through the line discipline as a keypress does.
window.gexType = (text) => term.input(text, true);

const fit = new FitAddon();
term.loadAddon(fit);
term.loadAddon(new WebLinksAddon());
term.open($('terminal'));
fit.fit();
new ResizeObserver(() => {
  try {
    fit.fit();
  } catch {
    /* the element can be zero-sized mid-layout */
  }
}).observe($('terminal'));

// -- the page's own chrome ------------------------------------------------

const ansi = {
  dim: (s) => `\x1b[2m${s}\x1b[0m`,
  green: (s) => `\x1b[32m${s}\x1b[0m`,
  red: (s) => `\x1b[31m${s}\x1b[0m`,
  bold: (s) => `\x1b[1m${s}\x1b[0m`,
};

function status(text, kind = '') {
  ui.status.textContent = text;
  ui.status.className = kind;
  // Announced as well as displayed, so `web/test/site.mjs` can tell whether a
  // demo got where it was going.
  window.dispatchEvent(new CustomEvent('gex:status', { detail: { text, kind } }));
}

function busy(text) {
  ui.overlayText.textContent = text;
  ui.overlay.classList.remove('hidden');
}

function idle() {
  ui.overlay.classList.add('hidden');
}

function bytes(n) {
  if (n > 1 << 20) return `${(n / (1 << 20)).toFixed(1)} MiB`;
  if (n > 1024) return `${(n / 1024).toFixed(0)} KiB`;
  return `${n} B`;
}

// -- demo plumbing the demos share ---------------------------------------

/** Fetch and unpack a rootfs tar, with progress on the overlay. */
const rootfsCache = new Map();
async function rootfs(url) {
  if (rootfsCache.has(url)) return rootfsCache.get(url);
  const buf = await fetchBytes(url, (got, total) => {
    busy(total ? `fetching root filesystem — ${bytes(got)} of ${bytes(total)}` : `fetching root filesystem — ${bytes(got)}`);
  });
  busy('unpacking');
  const tree = untar(buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength));
  rootfsCache.set(url, tree);
  return tree;
}

const ctx = {
  term,
  ansi,
  status,
  busy,
  idle,
  bytes,
  rootfs,
  fetchBytes,
  runProgram,
  setNet: (s) => {
    ui.net.textContent = s;
  },
  setBlocks: (s) => {
    ui.blocks.textContent = s;
  },
  actions(list) {
    ui.actions.replaceChildren();
    for (const a of list) {
      const b = document.createElement('button');
      b.className = `act${a.primary ? ' primary' : ''}`;
      b.textContent = a.label;
      // Hand the keyboard back to the terminal: a button that disables itself
      // (the Boot buttons do) drops focus to the body, where keystrokes go
      // nowhere.
      b.onclick = (ev) => {
        const r = a.onClick(ev);
        term.focus();
        return r;
      };
      if (a.id) b.id = a.id;
      ui.actions.append(b);
    }
  },
  hint(text) {
    ui.hint.textContent = text;
  },
  /// Undo something when the demo is left, so a demo that adds to the page (the
  /// gears' canvas) need not know when the teardown happens.
  onLeave(fn) {
    leaving.push(fn);
  },
};

/// What the current demo asked to have undone.
let leaving = [];

// -- routing --------------------------------------------------------------

let current = null;
let running = false;

async function select(demo) {
  if (running && !confirm('A program is still running. Leave it?')) return;
  running = false;
  for (const fn of leaving.splice(0)) {
    try {
      fn();
    } catch {
      // A demo that fails to tidy up must not stop the next one starting.
    }
  }
  current = demo;
  for (const el of document.querySelectorAll('.demo')) {
    el.setAttribute('aria-current', String(el.dataset.id === demo.id));
  }
  ui.title.textContent = demo.title;
  ui.blurb.textContent = demo.blurb;
  ctx.hint('');
  ctx.setNet(demo.network ? 'wisp' : 'off');
  term.reset();
  location.hash = demo.id;
  await demo.enter(ctx);
}

const nav = $('demos');
DEMOS.forEach((d, i) => {
  const b = document.createElement('button');
  b.className = 'demo';
  b.dataset.id = d.id;
  b.innerHTML = `<span class="n">${String(i + 1).padStart(2, '0')}</span><span class="t"></span><span class="d"></span>`;
  b.querySelector('.t').textContent = d.title;
  b.querySelector('.d').textContent = d.blurb;
  b.onclick = () => select(d);
  nav.append(b);
});

// -- start ----------------------------------------------------------------

// Anything that throws where nobody is catching, into the terminal as well as
// the console: a demo's own rejection was all the site reported, so a throw in
// a Worker or an unawaited promise made a dead run look like a quiet one.
for (const [event, describe] of [
  ['error', (e) => `${e.message} at ${e.filename}:${e.lineno}`],
  ['unhandledrejection', (e) => `unhandled rejection: ${e.reason?.stack || e.reason?.message || String(e.reason)}`],
]) {
  self.addEventListener(event, (e) => {
    const text = describe(e);
    try {
      term.writeln('');
      term.writeln(ansi.red(`gex: ${text}`));
    } catch {
      // The terminal is the nicety; the console is the guarantee.
    }
    console.error(`[gex] ${text}`);
  });
}

(async function start() {
  // `?harness=1` is not a demo: it answers commands over HTTP with no site UI
  // involved -- see `web/test/harness.mjs` and `harness.js`.
  if (new URLSearchParams(location.search).get('harness')) {
    const { start: harness } = await import('./harness.js');
    await harness();
    return;
  }
  ui.engine.textContent = navigator.userAgent.includes('Firefox') ? 'spidermonkey' : 'v8';
  term.writeln(ansi.bold('gex') + ansi.dim(' — AArch64 Linux programs, recompiled to WebAssembly'));
  term.writeln('');
  busy('fetching the runtime');
  try {
    const { shimBytes, seedBytes } = await loadRuntime((got, total) => {
      busy(total ? `fetching the runtime — ${bytes(got)} of ${bytes(total)}` : `fetching the runtime — ${bytes(got)}`);
    });
    term.writeln(ansi.dim(`  runtime ${bytes(shimBytes.length)}, seed module ${bytes(seedBytes.length)}`));
    term.writeln(ansi.dim('  the seed translates nothing; every block is compiled on first use'));
    term.writeln('');
    status('ready', 'good');
  } catch (e) {
    term.writeln(ansi.red(`could not load the runtime: ${e.message}`));
    status(String(e.message), 'bad');
  } finally {
    idle();
  }
  // `?demo=` as well as `#demo`, so a link can carry the demo's options.
  const params = new URLSearchParams(location.search);
  // The same switches the harness page has, through globals because the demos
  // call `runProgram` with their own arguments and there is no seam to pass one
  // through.
  if (params.has('adopt')) globalThis.GEX_ADOPT_AFTER = Number(params.get('adopt'));
  // `?recompile=N`: KiB of mapped text per object to statically recompile once
  // hot, 0 to arrive one dispatch miss at a time. On by default here, off under
  // `run-node.mjs` -- a difference that has to be switchable to attribute a
  // fault to one host.
  if (params.has('recompile')) globalThis.GEX_RECOMPILE_OBJECTS = Number(params.get('recompile'));
  if (params.has('permiss')) globalThis.GEX_PER_MISS = Number(params.get('permiss'));
  // `?link=1`: keep the address space the guest's `ld.so` produced and start
  // the next process of the same program from it.
  if (params.has('link')) globalThis.GEX_LINK = Number(params.get('link'));
  // `?heartbeat=N`: every N syscalls, print the census -- how many of each kind
  // and how long was spent inside them.
  if (params.has('heartbeat')) globalThis.GEX_HEARTBEAT = Number(params.get('heartbeat'));
  if (params.get('debug')) {
    globalThis.GEX_DEBUG_EXIT = 1;
    globalThis.GEX_TRAP_STACK = 1;
  }
  // `?trapstack=1`: the engine's stack for a trap -- which names the shim
  // function the fault was in -- without `?debug`'s mirroring of every guest
  // write to the console. That mirroring is not free: it changed the timing
  // enough to turn an `unreachable` in a forked apt method into a different
  // fault, so the one switch could not be used to diagnose the other.
  if (params.get('trapstack')) globalThis.GEX_TRAP_STACK = 1;
  // `?forkskip=N`: a forked child does not copy any inherited region larger
  // than N MiB, leaving those pages zero. A diagnostic, and a destructive one:
  // if the guest reads one it gets zeros. It answers the question a real
  // copy-on-write would answer for free -- how much of what a fork copies the
  // child ever actually looks at. See `adoptProcess`.
  if (params.has('forkskip')) globalThis.GEX_FORK_SKIP_MIB = Number(params.get('forkskip'));
  // `?adopttrace=1`: narrate adopt phases only (no debugExit write-flood), fast
  // enough to reach a deep fork and show where its adopt hangs.
  if (params.get('adopttrace')) globalThis.GEX_ADOPT_TRACE = 1;
  // `?attachtrace=1`: narrate a thread Worker's attach/adopt, to catch a
  // stranded thread whose wasm instantiate never returns.
  if (params.get('attachtrace')) globalThis.GEX_ATTACH_TRACE = 1;
  // `?virgltrace=1`: log every vtest feed, to tell a rendering client from one
  // stuck before its first draw.
  if (params.get('virgltrace')) globalThis.GEX_VIRGL_TRACE = 1;
  const asked = params.get('demo') || location.hash.slice(1);
  const want = DEMOS.find((d) => d.id === asked) || DEMOS[0];
  await select(want);
  // `?run=1` presses the demo's own first button, so a test can drive the page
  // without reaching inside it. `?run=N` for N>1 presses the N-th action button
  // instead -- e.g. `run=2` on the gears demos is "Draw for 5 minutes", the
  // budget a cold Mesa start needs when translation makes the first frame far
  // more than the usual forty seconds away.
  const runN = params.get('run');
  if (runN) {
    const idx = Math.max(1, parseInt(runN, 10) || 1) - 1;
    // Waited for, not assumed: a demo registers its actions during setup, which
    // may await a fetch first, so clicking the instant `select` resolves finds
    // nothing.
    for (let i = 0; i < 100; i++) {
      const buttons = document.querySelectorAll('#actions .act');
      const b = buttons[idx] || document.querySelector('#actions .act.primary');
      if (b) {
        b.click();
        break;
      }
      await new Promise((r) => setTimeout(r, 50));
    }
  }
})();

// Demos declare when they start and stop so navigation can warn.
window.addEventListener('gex:running', (e) => {
  running = e.detail;
});
void current;
