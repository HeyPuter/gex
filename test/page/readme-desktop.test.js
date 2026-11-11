// page/runtime/README.md's "In a web desktop", run as written: its code
// blocks, read from the README at run time, in one module, with only the
// changes SUBST lists (this test's tree is the busybox image, not Debian).
// The page plays the desktop. Its terminal widget, browser window and frames
// are fakes that record what reaches them. Its window manager frames each
// guest window as the README's Windows paragraph says. It moves, resizes and
// closes an X11 window (rt-x11, the tree's X client), and takes a Wayland
// window's (rt-wl's) own move request and maximizes it, and checks that each
// reaches the client and comes back as the README says it does.
export const meta = {
  hosts: ['browser'], images: ['busybox'], timeout: 180,
  html: '<style>html, body { margin: 0; } #desk { position: relative; width: 800px; height: 600px; }</style><div id="desk"></div>',
};

// The README's module specifiers, and the runtime's sources they are.
const MODULES = {
  '@gexemu/runtime': 'index.js',
  '@gexemu/runtime/fs': 'fs.js',
  '@gexemu/runtime/net/wisp': 'net/wisp.mjs',
  '@gexemu/runtime/windows/host': 'windows/host.js',
};
// `--package`: the built entries instead (lib/package.mjs's names).
const BUILT = { 'index.js': 'index.mjs', 'fs.js': 'fs.mjs', 'net/wisp.mjs': 'net-wisp.mjs', 'windows/host.js': 'windows-host.mjs' };

// Not `new URL(..., import.meta.url)` written out: vite would rewrite that
// as an asset's URL.
const HERE = import.meta.url;

export default async function (t) {
  const readme = await (await fetch(new URL('../../page/runtime/README.md', HERE))).text();
  const from = readme.indexOf('\n## In a web desktop\n');
  const section = readme.slice(from, readme.indexOf('\n## ', from + 5));
  if (from < 0) return t.check('the README has its "In a web desktop" section', false);
  const blocks = [...section.matchAll(/```js\n([\s\S]*?)```/g)].map((m) => m[1]);
  t.eq('its code blocks (boot, terminal, host call)', blocks.length, 3);

  // This test's tree and page, in place of the README's.
  const tree = JSON.stringify(t.work('bb.tar.zst'));
  const SUBST = [
    // The tree: the busybox image, which has no lazy list.
    [`fs.addTar('/gex-trees/debian.tar.zst', { lazy: '/gex-trees/debian-lazy.json' });`, `fs.addTar(${tree});`],
    // The runner's assets and its debug switches (vite dev's Workers).
    ['new System({ xcache: true })', 'new System({ xcache: true, baseUrl: __t_base, debug: __t_debug })'],
    // Busybox's shell; no /root in this tree.
    [`sys.spawn(['bash', '-l'], `, `sys.spawn(['sh', '-l'], `],
    [`cwd: '/root' }`, `cwd: '/' }`],
  ];
  let code = blocks.join('\n');
  for (const [a, b] of SUBST) {
    t.check(`the README still says ${JSON.stringify(a)}`, code.includes(a));
    code = code.replace(a, b);
  }
  const base = t.baseUrl === '/' ? null : t.baseUrl;
  code = code.replace(/from '(@gexemu\/runtime[^']*)'/g, (m, spec) => {
    const src = MODULES[spec];
    if (!src) throw new Error(`the README imports ${spec}, which this test does not map`);
    return `from ${JSON.stringify(base ? new URL(BUILT[src], new URL(base, location.href)).href : new URL(`../../page/runtime/src/${src}`, HERE).href)}`;
  });

  // The desktop's fakes, the names the README's blocks use.
  const termOut = [];
  let termIn = null;
  const opened = [];
  let closedTerminal = false;
  Object.assign(globalThis, {
    __t_base: t.baseUrl,
    __t_debug: t.debug(),
    term: { write: (d) => termOut.push(typeof d === 'string' ? d : new TextDecoder().decode(d)), onData: (fn) => { termIn = fn; } },
    closeTerminalWindow: () => { closedTerminal = true; },
    openBrowserWindow: (url) => opened.push(url),
  });
  // The README's store keeps its root across loads: a run starts from the tree.
  await (await navigator.storage.getDirectory()).removeEntry('gex-fs-debian', { recursive: true }).catch(() => {});
  const url = URL.createObjectURL(new Blob([`${code}\nexport { sys, fs, hw, sh };\n`], { type: 'text/javascript' }));
  const { sys, hw, sh } = await t.timeout(import(/* @vite-ignore */ url), 90000, 'the README\'s blocks');
  t.eq('the system runs', sys.state.phase, 'running');

  // The terminal: keys in through the widget's onData, output out to write.
  const said = () => termOut.join('');
  termIn('echo readme-$PWD\n');
  await t.until(() => said().includes('readme-/'), 'the terminal\'s output').catch(() => {});
  t.check('a key typed into the terminal reaches the shell, and its output the terminal', said().includes('readme-/'), JSON.stringify(said().slice(-200)));

  // The host call, as the README's guest script makes it.
  termIn(`exec 3<>/dev/gex-host; printf 'open-url\\0{"url":"%s"}' "https://example.org/" >&3; cat <&3 > /dev/null; echo called\n`);
  await t.until(() => opened.length && said().includes('called'), 'the host call').catch(() => {});
  t.eq('the guest\'s xdg-open reached the desktop', opened, ['https://example.org/']);

  // The window manager, as the README's Windows paragraph has it.
  const desk = document.getElementById('desk');
  const frames = new Map();
  const seen = [];
  hw.addEventListener('window', ({ window: w }) => {
    if (w.kind === 'toplevel' && w.decorations === 'host') {
      const frame = document.createElement('div');
      frame.style.position = 'absolute';
      const bar = document.createElement('div');
      bar.textContent = w.title;
      w.addEventListener('title', () => { bar.textContent = w.title; });
      frame.append(bar, w.canvas);
      desk.append(frame);
      frames.set(w, frame);
      w.place(0, bar.offsetHeight);
      w.addEventListener('resized', () => seen.push(['resized', w.width, w.height]));
      w.addEventListener('request', (e) => seen.push(['request', e.kind]));
      w.addEventListener('closed', () => { seen.push(['closed']); frame.remove(); frames.delete(w); });
    } else {
      w.canvas.style.position = 'absolute';
      desk.append(w.canvas);
    }
  });
  const env = { PATH: '/usr/local/bin:/usr/bin:/bin', ...hw.env };
  const x = sys.spawn(['rt-x11', 'readme-x', 'where'], { env });
  const out = t.lines(x);
  x.stderr.pipeTo(new WritableStream()).catch(() => {});
  const w = await t.until(() => [...frames.keys()].find((f) => f.title === 'readme-x'), 'the X window framed', 60000).catch(() => null);
  t.check('an X11 window arrives as a host-decorated toplevel and is framed', w, JSON.stringify(hw.windows().map((g) => [g.kind, g.decorations, g.title])));
  if (w) {
    // A move by the frame: place(x, y), which X programs read.
    frames.get(w).style.left = '120px';
    frames.get(w).style.top = '40px';
    w.place(120, 40 + 20);
    await t.until(() => out.some((l) => /^at 120 60 /.test(l)), 'the client at its place').catch(() => {});
    t.check('place(x, y) reaches the X client as its position on the root', out.some((l) => /^at 120 60 /.test(l)), JSON.stringify(out.slice(-4)));
    // A resize by the frame: the client hears it, and it comes back as resized.
    w.resize(300, 200);
    await t.until(() => w.width === 300 && w.height === 200, 'resized').catch(() => {});
    t.eq('resize(w, h) reaches the client and comes back as its size', [w.width, w.height], [300, 200]);
    t.check('and as a resized event', seen.some(([k, a, b]) => k === 'resized' && a === 300 && b === 200), JSON.stringify(seen));
    // A desktop's work area change.
    hw.setOutputSize(1024, 700);
    await t.until(() => out.some((l) => l === 'root 1024 700'), 'the new output size').catch(() => {});
    t.check('setOutputSize(w, h) reaches the X client as its root\'s size', out.some((l) => l === 'root 1024 700'), JSON.stringify(out.filter((l) => l.startsWith('root'))));
    // The frame's close button: a polite close, the window's closed event,
    // the client's end.
    w.close();
    const st = await t.timeout(x.exited, 20000, 'the X client ending').catch((e) => ({ code: -1, error: e.message }));
    await t.until(() => seen.some(([k]) => k === 'closed'), 'closed').catch(() => {});
    t.eq('close() ends the client', st, { code: 0, signal: null });
    t.check('the window says closed, and the frame went', seen.some(([k]) => k === 'closed') && !frames.has(w), JSON.stringify(seen));
    t.check('the client heard the close', out.includes('close'), JSON.stringify(out));
  }

  // A Wayland window that draws its own title bar's actions: a press on it
  // asks the desktop's manager to move it (`request`), and the manager's
  // maximize reaches it as states (`setStates`).
  const y = sys.spawn(['rt-wl', 'readme-wl', 'move'], { env });
  const yout = t.lines(y);
  y.stderr.pipeTo(new WritableStream()).catch(() => {});
  const v = await t.until(() => [...frames.keys()].find((f) => f.title === 'readme-wl'), 'the Wayland window framed', 60000).catch(() => null);
  t.check('a Wayland window arrives as a host-decorated toplevel and is framed', v, JSON.stringify(hw.windows().map((g) => [g.kind, g.decorations, g.title])));
  if (v) {
    await t.until(() => yout.some((l) => l.startsWith('drawn')), 'drawn').catch(() => {});
    const r = v.canvas.getBoundingClientRect();
    await t.move(r.left + 10, r.top + 10);
    await t.click(r.left + 10, r.top + 10);
    await t.until(() => seen.some(([k, kind]) => k === 'request' && kind === 'move'), 'a move request').catch(() => {});
    t.check('the client\'s own move comes to the manager as a request event', seen.some(([k, kind]) => k === 'request' && kind === 'move'), JSON.stringify([seen.slice(-4), yout.slice(-4)]));
    v.setStates({ maximized: true }, 1024, 700);
    await t.until(() => yout.some((l) => /^configure 1024 700 \S*m/.test(l)), 'maximized').catch(() => {});
    t.check('setStates({ maximized }, w, h) reaches the client', yout.some((l) => /^configure 1024 700 \S*m/.test(l)), JSON.stringify(yout.slice(-4)));
    v.close();
    t.eq('close() ends it', await t.timeout(y.exited, 20000, 'the Wayland client ending').catch((e) => ({ code: -1, error: e.message })), { code: 0, signal: null });
  }

  // The shell's end reaches the terminal's window.
  termIn('exit\n');
  await t.timeout(sh.exited, 20000, 'the shell ending').catch(() => {});
  await t.until(() => closedTerminal, 'closeTerminalWindow').catch(() => {});
  t.check('the shell\'s exit closes the terminal window', closedTerminal);
  await sys.shutdown();
}
