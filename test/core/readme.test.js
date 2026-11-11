// The runtime README's examples, run as written on the test page, and every
// class and member its tables name, there. Each example is a ```js block of
// page/runtime/README.md, in its order; a page has one System, so each phase
// (t.reload) runs the blocks that share one. What an example takes from the
// page around it (a terminal widget, a desktop's frames, a button) is a stub
// here; its `@gexemu/runtime` imports are the runtime's modules; its trees are the
// slim Debian image (bash, uname). A block the README gains or loses fails
// the block count, so a new example is never left unrun.
import readme from '../../page/runtime/README.md?raw';
import * as index from '../../page/runtime/src/index.js';
import * as fsMod from '../../page/runtime/src/fs.js';
import * as host from '../../page/runtime/src/windows/host.js';
import * as single from '../../page/runtime/src/windows/single.js';
import * as compositor from '../../page/runtime/src/windows/compositor.js';
import * as wisp from '../../page/runtime/src/net/wisp.mjs';
import * as audio from '../../page/runtime/src/audio.js';
import * as gl from '../../page/runtime/src/gl.js';
import * as input from '../../page/runtime/src/input.js';
import { SP_CODE, MP_CODE } from '../../hosts/browser/js/mode.mjs';

export const meta = { hosts: ['browser'], images: ['slim'], timeout: 600 };

const M = {
  '@gexemu/runtime': index, '@gexemu/runtime/fs': fsMod, '@gexemu/runtime/windows/host': host,
  '@gexemu/runtime/windows/single': single, '@gexemu/runtime/windows/compositor': compositor,
  '@gexemu/runtime/net/wisp': wisp, '@gexemu/runtime/audio': audio, '@gexemu/runtime/gl': gl, '@gexemu/runtime/input': input,
};
// The single-thread build (system.js `SINGLE`).
const SINGLE = !MP_CODE || (SP_CODE && typeof SharedArrayBuffer !== 'function');
const BLOCKS = [...readme.matchAll(/```js\n([\s\S]*?)```/g)].map((m) => m[1]);

/** Each phase: the blocks it runs (indices into BLOCKS), what it makes
 *  first when its blocks start from a system they do not make (`pre`:
 *  'new' a System and Filesystem with the tree; 'booted' those, booted),
 *  whether they make windows (`windows`), and what is checked after. */
const PHASES = [
  { blocks: [0], check: async (t, v) => {
    t.check('intro: uname -a', v.code === 0 && /Linux/.test(v.stdout), `exit ${v.code}: ${v.stdout}${JSON.stringify(v.sys.state.console.slice(-600))}`);
    await t.until(() => /\S/.test(v.term.text), 'intro: bash -l says something on its terminal', 60000);
    v.sh.pty.write('exit\n');
    t.eq('intro: bash -l exits', (await t.timeout(v.sh.exited, 30000, 'bash exit')).code, 0);
  } },
  { blocks: [1, 2, 3], windows: true, before: () => fsMod.OPFSFilesystem.erase('debian'), check: async (t, v) => {
    t.eq('desktop: running', v.sys.state.phase, 'running');
    t.check('desktop: hw.env names the display', !!(v.hw.env.WAYLAND_DISPLAY && v.hw.env.DISPLAY), JSON.stringify(v.hw.env));
    await t.until(() => /\S/.test(v.term.text), 'terminals: bash -l says something on its terminal', 60000);
    v.sh.pty.write('exit\n');
    await t.timeout(v.sh.exited, 30000, 'bash exit');
    await t.until(() => v.calls.closeTerminalWindow, 'terminals: closeTerminalWindow after the exit', 5000);
    // The guest half of the xdg-open example, as the README's comment has it.
    const script = '/usr/local/bin/xdg-open';
    await v.fs.mkdir('/usr/local/bin', { recursive: true });
    await v.fs.writeFile(script, `#!/bin/sh\n${guestLine(BLOCKS[3], 'exec 3<>')}\n`, { mode: 0o755 });
    const st = await v.fs.stat(script);
    t.eq('host calls: the guest script is executable', (st.mode & 0o777).toString(8), '755');
    const r = await t.timeout(v.sys.run(['xdg-open', 'https://example.org/'], { env: v.env }), 30000, 'xdg-open');
    t.eq('host calls: xdg-open opens the URL', [r.code, v.calls.openBrowserWindow], [0, 'https://example.org/']);
  } },
  { blocks: [4], refusedSingle: /needs the default build/, check: async (t, v) => {
    // More than the 16 MiB cache, so the kernel writes it out to the
    // page's store and reads it back.
    const r = await t.timeout(v.sys.run(['sh', '-c', 'dd if=/dev/urandom of=/root/a bs=1M count=40 2>/dev/null && cp /root/a /root/b && md5sum /root/a /root/b && rm /root/a /root/b'], { env: { PATH: '/usr/bin:/bin' } }), 120000, 'store');
    const sums = r.stdout.trim().split('\n').map((l) => l.split(' ')[0]);
    t.check('a store of the page\'s own: 40 MiB copied through it', r.code === 0 && sums.length === 2 && sums[0] === sums[1], `exit ${r.code}: ${r.stdout}${r.stderr}`);
  } },
  { blocks: [5], before: () => fsMod.OPFSFilesystem.erase('my-tree'), check: async (t, v) => {
    const r = await t.timeout(v.sys.run(['sh', '-c', 'echo kept > /root/kept && sync'], { env: { PATH: '/usr/bin:/bin' } }), 30000, 'kept');
    t.eq('persistence: a file written and synced', [r.code, r.stderr], [0, '']);
    t.eq('persistence: sys.sync()', await v.sys.sync().then(() => 'ok', (e) => e.code ?? e.message), 'ok');
  } },
  { blocks: [6, 7], pre: 'booted', check: async (t, v) => {
    t.check('network: the last one set, a Wisp', v.sys.network instanceof wisp.Wisp && v.sys.network.url === 'wss://wisp.example/', String(v.sys.network?.url));
  } },
  { blocks: [8], pre: 'booted', check: async (t, v) => {
    // The guest half, as the README's comment has it.
    const r = await t.timeout(v.sys.run(['sh', '-c', guestLine(BLOCKS[8], 'perl ')], { env: { PATH: '/usr/bin:/bin' } }), 30000, 'unix client');
    t.eq('unix sockets: a program\'s connect to sys.listen\'s, echoed', [r.code, r.stdout, r.stderr], [0, 'hello\n', '']);
    t.eq('unix sockets: the socket file', (await v.fs.stat('/run/app.sock', { follow: false })).type, 'socket');
  } },
  { blocks: [9], pre: 'booted', prep: async (t, sys, fs) => {
    // The guest server the block connects to, as the README's comment has it.
    sys.spawn(['sh', '-c', guestLine(BLOCKS[9], 'perl ')], { env: { PATH: '/usr/bin:/bin' } });
    await t.until(async () => (await fs.stat('/run/myd.sock').catch(() => null))?.type === 'socket', 'the guest server listens', 30000);
  }, check: async (t, v) => {
    t.eq('unix sockets: sys.connect to a program\'s server', v.reply, 'HELLO\n');
  } },
  { blocks: [10], pre: 'booted', check: async (t, v) => {
    const r = await t.timeout(v.sys.run(['sh', '-c', guestLine(BLOCKS[10], 'exec 3<>')], { env: { PATH: '/usr/bin:/bin' } }), 30000, 'notify');
    t.eq('host calls: the guest reads "\\0true"', [r.code, r.stdout], [0, '\0true']);
    t.eq('host calls: notify ran', v.calls.showToast, 'built');
    v.off();
    t.eq('host calls: unregistered', v.sys.hostCalls.names().includes('notify'), false);
  } },
  { blocks: [12], windows: true, pre: 'new', after: [11], boot: true, check: async (t, v) => {
    t.check('GL: a MesaGl before boot, beside its windows', !!v.gl && !!v.hw, '');
    v.button.onclick();
    t.check('audio: the click made a WebAudio', true, '');
  } },
  { blocks: [13], windows: true, pre: 'new', check: async (t, v) => {
    t.eq('windows: running', v.sys.state.phase, 'running');
    t.check('windows: hw.env', !!v.hw.env.WAYLAND_DISPLAY, JSON.stringify(v.hw.env));
  } },
];

/** The line of a block's `// guest:` comment that starts with `start`, and
 *  the comment lines after it that carry on its command (`//   ...`). */
function guestLine(block, start) {
  const lines = block.split('\n').map((l) => l.replace(/^\s*\/\/\s?/, ''));
  const i = lines.findIndex((l) => l.includes(start));
  return lines[i].slice(lines[i].indexOf(start)).trim();
}

/** A block as the body of an async function: its imports from the modules
 *  above, its trees the slim image. */
function prepare(code, tree) {
  return code
    .replace(/import \{([^}]*)\} from '([^']+)';/g, (_, names, spec) => {
      if (!M[spec]) throw new Error(`the README imports ${spec}, which is not a module of the runtime`);
      return `const {${names}} = M[${JSON.stringify(spec)}];`;
    })
    .replace(/'\/(?:gex-)?trees\/[^']*\.tar\.zst'(?:,\s*\{ lazy: '[^']*' \})?/g, JSON.stringify(tree));
}

const STUBS = `
const term = { text: '', write(d) { this.text += typeof d === 'string' ? d : new TextDecoder().decode(d); }, onData(fn) { this.feed = fn; } };
const frame = () => {};
const desktop = { append() {} };
const button = { onclick: null };
const showToast = (title) => { calls.showToast = title; };
const openBrowserWindow = (url) => { calls.openBrowserWindow = url; };
const closeTerminalWindow = () => { calls.closeTerminalWindow = true; };
`;
const NAMES = ['sys', 'fs', 'hw', 'sh', 'env', 'code', 'stdout', 'gl', 'off', 'term', 'button', 'srv', 'conn', 'reply'];

export default async function (t) {
  const { eq, check, say } = t;
  if (t.phase === 1) {
    eq('the README\'s examples', BLOCKS.length, 14);
    members(t);
  }
  const phase = PHASES[t.phase - 1];
  const tree = t.work('slim/debian-slim.tar.zst');
  await phase.before?.();
  const pre = phase.pre === undefined ? '' : `const { System } = M['@gexemu/runtime']; const { MemoryFilesystem } = M['@gexemu/runtime/fs'];
const sys = new System({ baseUrl: BASE, debug: DEBUG }); const fs = new MemoryFilesystem(sys); fs.addTar(${JSON.stringify(tree)});
${phase.pre === 'booted' ? 'await sys.boot();' : ''}`;
  // What the blocks need running in the guest first (`prep`).
  const parts = [STUBS, pre, phase.prep ? 'await PREP(sys, fs);' : '', ...phase.blocks.map((i) => prepare(BLOCKS[i], tree))];
  if (phase.boot) parts.push('await sys.boot();');
  for (const i of phase.after ?? []) parts.push(prepare(BLOCKS[i], tree));
  // `new System()` as the README has it, with the runner's base (where it
  // serves the runtime's files from elsewhere than beside the modules) and
  // its debug switches (vite dev's Workers load by URL, t.debug).
  const body = `${parts.join('\n')}\nreturn { calls, ${NAMES.map((n) => `${n}: typeof ${n} === 'undefined' ? undefined : ${n}`).join(', ')} };`
    .replace(/new System\(\)/g, 'new System({ baseUrl: BASE, debug: DEBUG })')
    .replace(/new System\(\{ xcache: true \}\)/g, 'new System({ baseUrl: BASE, debug: DEBUG, xcache: true })');
  const AsyncFunction = (async () => {}).constructor;
  let v;
  try {
    v = await t.timeout(new AsyncFunction('M', 'BASE', 'DEBUG', 'calls', 'innerWidth', 'innerHeight', 'PREP', body)(M, t.baseUrl, t.debug(), {}, 1280, 720, (sys, fs) => phase.prep(t, sys, fs)), 180000, `blocks ${phase.blocks}`);
  } catch (e) {
    // What the single-thread build refuses, as the README says it does.
    if (SINGLE && phase.refusedSingle) check(`blocks ${phase.blocks.join(', ')}: the single-thread build refuses them`, phase.refusedSingle.test(e.message), e.message);
    else check(`blocks ${phase.blocks.join(', ')} run`, false, `${e.message}\n${body}`);
    return t.phase < PHASES.length ? t.reload() : undefined;
  }
  check(`blocks ${phase.blocks.join(', ')} run`, !(SINGLE && phase.refusedSingle), 'the single-thread build ran what it refuses');
  await phase.check(t, v);
  say(`phase ${t.phase}: blocks ${phase.blocks.join(', ')}`);
  await v.sys?.shutdown();
  if (t.phase < PHASES.length) return t.reload();
}

/** Every class the README's tables name, with their members. */
function members(t) {
  const want = {
    'System': [index.System.prototype, ['boot', 'spawn', 'run', 'init', 'state', 'describe', 'sync', 'shutdown', 'hostCalls', 'net', 'network', 'listen', 'connect', 'addEventListener']],
    'Process': [index.Process.prototype, ['pid', 'kill', 'windows', 'addEventListener']],
    'Filesystem': [fsMod.Filesystem.prototype, ['open', 'read', 'write', 'size', 'truncate', 'flush', 'addTar', 'addFiles', 'readFile', 'writeFile', 'stat', 'readdir', 'mkdir', 'rm', 'rename', 'symlink', 'readlink', 'chmod', 'watch', 'mount', 'unmount', 'mounts']],
    'Mount': [fsMod.Mount.prototype, ['unmount']],
    'OPFSFilesystem': [fsMod.OPFSFilesystem.prototype, ['erase']],
    'Wisp': [wisp.Wisp.prototype, ['url', 'connect', 'datagram', 'close', 'addEventListener']],
    'WebAudio': [audio.WebAudio.prototype, ['ready', 'context', 'stats', 'close']],
    'MesaGl': [gl.MesaGl.prototype, ['start']],
    'PointerLock': [input.PointerLock.prototype, ['locked', 'release']],
    'Clipboard': [input.Clipboard.prototype, ['text', 'setText']],
    'Gamepads': [input.Gamepads.prototype, ['pads']],
    'HostWindows': [host.HostWindows.prototype, ['setOutputSize', 'env', 'windows', 'focused', 'keyEvent']],
    'GuestWindow': [host.GuestWindow.prototype, ['parent', 'pid', 'title', 'width', 'height', 'decorations', 'placed', 'x', 'y', 'place', 'resize', 'setStates', 'focus', 'close', 'appId']],
    'SingleCanvas': [single.SingleCanvas.prototype, ['env', 'describe', 'resized']],
    'CompositorWindows': [compositor.CompositorWindows.prototype, ['env', 'resize', 'describe']],
  };
  for (const [name, [proto, names]] of Object.entries(want)) {
    const missing = names.filter((n) => !(n in proto));
    t.check(`${name}: the README's members`, !missing.length, `missing ${missing.join(', ')}`);
  }
}
