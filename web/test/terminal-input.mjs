// The page's terminal, without a page.
//
//   node web/test/terminal-input.mjs
//
// `runProgram` and `TerminalEnv` are the only parts of the browser path that
// no other test covers: the shim has its own tests, and node's `run-node.mjs`
// exercises the same run loop against a pipe. What is untested in between is
// the line discipline -- the page collects a line, echoes it, and delivers it
// only when the guest is not doing that itself -- and the promise that parks a
// guest read until a key arrives.
//
// It is worth having a test for exactly because the failure is silent. A
// keystroke that never reaches the guest looks like a hung program, and a
// terminal that is not listening looks the same as one that is.
//
// So: a fake xterm with a `type()` method, a real BusyBox shell, and a check
// that what was typed came back out.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const HERE = dirname(fileURLToPath(import.meta.url));
const WEB = join(HERE, '..');

// `runtime.js` fetches the runtime by URL, because in a page it is served.
// Answering those from disk is the whole of what makes it runnable here.
globalThis.fetch = async (url) => {
  const path = join(WEB, 'public', String(url));
  let body;
  try {
    body = readFileSync(path);
  } catch {
    return { ok: false, status: 404, statusText: 'not found' };
  }
  return {
    ok: true,
    status: 200,
    headers: { get: () => String(body.length) },
    body: null,
    arrayBuffer: async () => body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength),
  };
};

const { runProgram } = await import('../src/runtime.js');
const { untar } = await import('../src/tar.js');

/** Enough of xterm.js for the runtime to talk to, plus a way to type at it. */
class FakeTerm {
  constructor() {
    this.out = '';
    this.handlers = [];
    this.rows = 24;
    this.cols = 80;
    this.focused = 0;
  }

  write(data) {
    const s = typeof data === 'string' ? data : new TextDecoder().decode(data);
    this.out += s;
    if (process.env.GEX_TEST_VERBOSE) process.stdout.write(s);
  }

  writeln(data) {
    this.write(`${data}\n`);
  }

  focus() {
    this.focused += 1;
  }

  onData(fn) {
    this.handlers.push(fn);
    return { dispose: () => { this.handlers = this.handlers.filter((h) => h !== fn); } };
  }

  onResize() {
    return { dispose() {} };
  }

  /** What a person at the keyboard does. */
  type(text) {
    for (const h of this.handlers.slice()) h(text);
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Wait for the guest to print something, so we type at a prompt and not before. */
async function waitFor(term, re, what, seconds = 240) {
  for (let i = 0; i < seconds * 20; i += 1) {
    if (re.test(term.out)) return true;
    await sleep(50);
  }
  throw new Error(`timed out waiting for ${what}\n--- terminal ---\n${term.out}`);
}

const tar = readFileSync(join(WEB, 'public', 'rootfs', 'busybox.tar'));
const tree = untar(tar.buffer.slice(tar.byteOffset, tar.byteOffset + tar.byteLength));
const files = { ...tree.files };
// The applet links, resolved the way the demo resolves them: one binary
// answering to every name in /bin.
for (const [path, target] of Object.entries(tree.links)) {
  const abs = target.startsWith('/')
    ? target
    : `${path.slice(0, path.lastIndexOf('/'))}/${target}`.replace(/\/{2,}/g, '/');
  if (files[abs]) files[path] = files[abs];
}

// Exactly the demo's invocation. `-i` earns its place: without it the shell
// reads commands without printing a prompt, so a test that waits for one
// waits forever against a shell that is working perfectly.
const term = new FakeTerm();
const done = runProgram(term, {
  elf: files['/bin/busybox'],
  files,
  args: ['/bin/sh', '-i'],
  cwd: '/root',
  tty: true,
});
// A rejection here is the interesting failure -- the guest never starting --
// and without this it would surface only as a timeout with an empty terminal.
done.catch((e) => { console.log(`runProgram threw: ${e && e.stack ? e.stack : e}`); });

let failures = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failures += 1;
};

try {
  // The shell prints a prompt when it is ready to read, which is also when
  // the run loop has parked and `readStdin` is waiting on a promise.
  await waitFor(term, /[#$] $/, 'a shell prompt');
  check(term.focused > 0, 'the runtime focuses the terminal so keystrokes arrive');

  const mark = term.out.length;
  term.type('echo hello-from-the-page\r');
  await waitFor(term, /hello-from-the-page/, 'the echoed line');
  const after = term.out.slice(mark);
  check(/hello-from-the-page/.test(after), 'a typed line reaches the guest');

  // Typed before the guest asks for it: the environment has to hold it, not
  // drop it. This is the case a page hits constantly, because a person types
  // ahead of a shell that is still starting a command.
  term.type('echo second-line\r');
  await waitFor(term, /second-line/, 'a line typed ahead');
  check(true, 'input typed before the guest reads is not lost');

  term.type('exit\r');
  const { status } = await done;
  check(status === 0, `the shell exits cleanly (got ${status})`);
} catch (e) {
  console.log(`FAIL ${e.message}`);
  failures += 1;
}

console.log(failures ? `${failures} failed` : 'all passed');
process.exit(failures ? 1 : 0);
