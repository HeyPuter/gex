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
    this.resizers = [];
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

  onResize(fn) {
    this.resizers.push(fn);
    return { dispose: () => { this.resizers = this.resizers.filter((h) => h !== fn); } };
  }

  /** Someone dragged the window. */
  resize(rows, cols) {
    this.rows = rows;
    this.cols = cols;
    for (const h of this.resizers.slice()) h({ rows, cols });
  }

  /** What a person at the keyboard does. */
  type(text) {
    for (const h of this.handlers.slice()) h(text);
  }
}

const pause = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * Type a command and give back everything it printed.
 *
 * The waiting is on a *fresh prompt*, not on the output, and that is the
 * point. Waiting for a pattern in the output returns on the first line that
 * matches -- so a check against the rest of `stty -a`, which is eight lines,
 * ran against a buffer holding one of them. It failed about half the time and
 * looked exactly like the terminal dropping flags.
 */
async function run(term, cmd, seconds = 60) {
  const mark = term.out.length;
  term.type(`${cmd}\r`);
  for (let i = 0; i < seconds * 20; i += 1) {
    // The echo of the command ends in a newline, so a prompt at the end of
    // the slice is the shell asking for the next one.
    if (/[#$] $/.test(term.out.slice(mark))) return term.out.slice(mark);
    await pause(50);
  }
  throw new Error(`\`${cmd}\` never came back\n--- terminal ---\n${term.out.slice(mark)}`);
}

/** Wait for the guest to print something, so we type at a prompt and not before. */
async function waitFor(term, re, what, seconds = 240) {
  for (let i = 0; i < seconds * 20; i += 1) {
    if (re.test(term.out)) return true;
    await pause(50);
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
  // The terminal has a name now, and `TERM` is what a curses program looks
  // up before it draws anything.
  env: ['TERM=xterm-256color', 'PATH=/bin:/usr/bin:/sbin:/usr/sbin', 'HOME=/root'],
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

  // The line discipline, which is the kernel's now and not this page's.
  //
  // Erase has two halves and they have to agree: rub the character off the
  // screen, and take the byte back out of the line. A terminal that echoes
  // the rub-out and keeps the byte hands the guest `echo xyhello`, which
  // looks like the shell's fault and is not.
  term.type('echo x');
  term.type('y\x7f\x7fhello-edited\r');
  await waitFor(term, /hello-edited/, 'an edited line');
  check(!/x[yx]?hello-edited/.test(term.out.slice(term.out.indexOf('hello-edited') - 4)),
    'backspace takes the byte back, not just the pixels');

  // What a terminal *is*, as the guest sees it: `TCGETS` and `TIOCGWINSZ`.
  const stty = await run(term, 'stty -a');
  check(/rows 24; columns 80/.test(stty), `TIOCGWINSZ reports the size (${
    (stty.match(/rows \d+; columns \d+/) || ['nothing'])[0]})`);
  check(/intr = \^C/.test(stty), 'c_cc reaches the guest: intr is ^C');
  check(/icanon/.test(stty) && /echo/.test(stty), 'c_lflag reaches the guest');
  check(/opost/.test(stty) && /onlcr/.test(stty), 'c_oflag reaches the guest');

  // A resize is a new answer from the same ioctl, and a SIGWINCH to whoever
  // is drawing.
  term.resize(30, 100);
  const size = await run(term, 'stty size');
  check(/30 100/.test(size),
    `a resize reaches the guest (${(size.match(/\d+ \d+/) || ['nothing'])[0]})`);

  const named = await run(term, 'echo TERM=$TERM');
  check(/TERM=xterm-256color/.test(named), 'TERM names a real terminal');

  // `^C` at a prompt: the terminal shows it, the half-typed line is thrown
  // away rather than delivered with whatever is typed next, and the shell is
  // still there.
  //
  // Against the *slice*, not the whole buffer. `stty -a` printed `intr = ^C`
  // a few commands ago, so a wait for `^C` anywhere in the terminal returns
  // instantly and the check that follows reads a buffer the echo has not
  // reached yet.
  let mark2 = term.out.length;
  term.type('this-is-abandoned\x03');
  for (let i = 0; i < 200 && !/\^C/.test(term.out.slice(mark2)); i += 1) await pause(50);
  check(/\^C/.test(term.out.slice(mark2)), 'the terminal shows ^C');
  term.type('echo still-here\r');
  await waitFor(term, /still-here/, 'the shell after ^C');
  check(
    !/this-is-abandoned/.test(term.out.slice(term.out.indexOf('still-here'))),
    'the abandoned line is not delivered later',
  );

  // And `^C` to a *running* command, which is the one that cannot be done by
  // handing over a keystroke: nothing is reading, so there is nobody to hand
  // it to. It has to become SIGINT to the foreground process group.
  //
  // `sleep 60` is the whole test: an external command in the foreground
  // group, nothing reading the terminal, and a minute of patience if the
  // signal does not arrive. The measurement is *how long* the prompt takes
  // to come back -- anything near a minute means the command ran to the end
  // and the interrupt did nothing.
  mark2 = term.out.length;
  term.type('sleep 60\r');
  await new Promise((r) => setTimeout(r, 1500));
  const sentAt = Date.now();
  term.type('\x03');
  let back = 0;
  for (let i = 0; i < 300; i += 1) {
    if (/[#$] $/.test(term.out.slice(mark2))) { back = Date.now() - sentAt; break; }
    await pause(50);
  }
  check(back > 0 && back < 10_000,
    `^C interrupts a running command (prompt back after ${back || '>15000'}ms)`);

  term.type('exit\r');
  const { status } = await done;
  check(status === 0, `the shell exits cleanly (got ${status})`);
} catch (e) {
  console.log(`FAIL ${e.message}`);
  failures += 1;
}

console.log(failures ? `${failures} failed` : 'all passed');
process.exit(failures ? 1 : 0);
