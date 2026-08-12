// The demo page.
//
// Three demos, one terminal, one runtime. Each demo says what it needs (a
// binary, a root filesystem, a network) and the shell around it handles the
// fetching, the progress and the terminal.

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
  // `convertEol` is off, and that is the change rather than an omission.
  //
  // It turns every `\n` into `\r\n` unconditionally, which is the terminal
  // doing the kernel's job: `ONLCR` in `c_oflag` is what a Unix program is
  // relying on when it writes a bare `\n`, and a program that *clears*
  // `OPOST` -- `less`, `vi`, anything placing its own cursor -- is asking for
  // the opposite and was not getting it. The guest's terminal now honours the
  // flag it was always told about; see `gex_kernel::tty::Tty::output`.
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
// The whole scrollback, for something other than a person reading the page.
// `.xterm-rows` is the viewport and nothing else, so a run that scrolls --
// which is every interesting one -- loses everything above the fold, which
// is where a failure's first line always is.
window.gexTerminalText = () => {
  const b = term.buffer.active;
  const out = [];
  for (let i = 0; i < b.length; i++) {
    out.push(b.getLine(i)?.translateToString(true) ?? '');
  }
  return out.join('\n');
};

// Typing, for something other than a person.
//
// The sibling of `gexTerminalText`: that one reads the screen, this one puts
// keystrokes in. `?cmd=` writes a script and runs it, which is a different
// program from a person at a prompt -- `bash -i` turns on job control,
// `tcsetpgrp` and a raw-mode line editor, and a script turns on none of them.
// A fault that only shows on the *second* command typed at a prompt was
// therefore beyond every test here.
//
// `term.input` is xterm's own entry point for input, so this goes through the
// line discipline exactly as a keypress does rather than around it.
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
  // Announced as well as displayed, so something other than a person can
  // tell whether a demo got where it was going. `web/test/site.mjs` drives
  // the built bundle through a real browser and listens for this; without it
  // the only way to check the page was to look at it.
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
      // Hand the keyboard back to the terminal. A button keeps the focus
      // after it is clicked, and one that disables itself -- which the Boot
      // buttons do -- drops it to the body instead, where keystrokes go
      // nowhere at all.
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
  /// Undo something when the demo is left. A demo that puts anything on the
  /// page -- the gears put a canvas there -- has to take it away again, and
  /// doing that here means the demo does not have to know when it happens.
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

// Anything that throws where nobody is catching, into the terminal.
//
// The site reports what a demo's own promise rejects with, and nothing else.
// A throw in a Worker, in the page's own code, or in a promise nobody awaited
// went to a console that a person watching a terminal never opens -- so a run
// that died looked like a run that had gone quiet, and the demo sat at its
// last heartbeat with the guest's pc frozen where the Worker had left it.
// "apt update crashes and there's a throw that never gets printed" is exactly
// this, and it stayed unidentified for as long as it did because the evidence
// was one panel away from the failure.
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
  // `?harness=1` is not a demo. It boots the runtime against a rootfs served
  // from a real directory and then answers commands over HTTP for as long as
  // the driver is there -- see `web/test/harness.mjs`. Nothing of the site's
  // UI is involved, because the point is to pay for the browser once and ask
  // many questions, rather than reload the page for each one.
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
  // `?demo=` as well as `#demo`, so a link can carry the demo's own options
  // in the query alongside it.
  const params = new URLSearchParams(location.search);
  // The same switches the harness page has, on the demo page too.
  //
  // These existed only inside `harness.js`, so the one page a test can
  // actually drive -- the built site, with its own rootfs and its own
  // network -- could not be told to defer differently or to narrate a crash.
  // Every question about the demo had to be answered on a different page
  // running a different program, and the two disagreed: the harness ran apt
  // against a `file:` repository and passed for days while the site crashed
  // on every run. A switch that cannot be reached where the failure happens
  // is not a switch.
  if (params.has('adopt')) globalThis.GEX_ADOPT_AFTER = Number(params.get('adopt'));
  // `?recompile=N`: KiB of mapped text per object to statically recompile
  // once it proves hot, and 0 to leave the guest to arrive one dispatch miss
  // at a time. It is on by default here and off by default under
  // `run-node.mjs`, which is exactly the kind of difference that has to be
  // switchable from outside: three `ls` in this page lift 78,000 blocks each
  // while the shared store reports `hit=241 miss=0`, and whether that is
  // whole-object recompilation redoing work already stored cannot be decided
  // without turning it off.
  if (params.has('recompile')) globalThis.GEX_RECOMPILE_OBJECTS = Number(params.get('recompile'));
  if (params.has('permiss')) globalThis.GEX_PER_MISS = Number(params.get('permiss'));
  // `?link=1`: keep the address space the guest's `ld.so` produced and start
  // the next process of the same program from it. Through a global for the
  // same reason as the rest of these -- the page's demos call `runProgram`
  // with their own arguments and there is no seam to pass one through.
  if (params.has('link')) globalThis.GEX_LINK = Number(params.get('link'));
  // `?heartbeat=N`: every N syscalls, print the census -- how many of each
  // kind and how long was spent inside them. The one instrument that says
  // whether a process's time is in its syscalls or between them, and it was
  // reachable only by editing the page.
  if (params.has('heartbeat')) globalThis.GEX_HEARTBEAT = Number(params.get('heartbeat'));
  if (params.get('debug')) {
    globalThis.GEX_DEBUG_EXIT = 1;
    globalThis.GEX_TRAP_STACK = 1;
  }
  const asked = params.get('demo') || location.hash.slice(1);
  const want = DEMOS.find((d) => d.id === asked) || DEMOS[0];
  await select(want);
  // `?run=1` presses the demo's own first button. A link that shows someone
  // the thing rather than a button that shows it -- and the only way a test
  // can drive the page without reaching inside it.
  if (params.get('run')) {
    // Waited for, not assumed. A demo registers its actions during setup,
    // and setup may await a fetch first -- so clicking the instant `select`
    // resolves finds nothing, and `?.click()` on nothing is a silent no-op.
    // Every demo on the site then sat there doing exactly what it was asked
    // to do, which was nothing, and the page test reported "timeout, last
    // status: (none)" for all of them.
    for (let i = 0; i < 100; i++) {
      const b = document.querySelector('#actions .act.primary');
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
