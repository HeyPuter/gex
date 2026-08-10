// A page that boots the runtime once and then answers commands for ever.
//
// The demo site is the wrong shape for development. Every measurement means a
// page load, and a page load means Chromium starting, the bundle parsing, and
// a rootfs tar being fetched and unpacked into the guest filesystem -- and
// then the demo runs *one* command and exits. Three `ls` cost 295 seconds
// that way, against 20 under Node, so every question about the browser was
// asked through a five-minute keyhole and most were not asked at all.
//
// Almost all of that is setup, and setup is the same every time. So: boot
// once, keep an interactive shell alive, and take commands over HTTP from
// whatever is driving. The second command costs what the guest costs and
// nothing else.
//
// The rootfs comes from a real directory on the host, streamed as a tar the
// server makes on demand (`/harness/rootfs.tar`). No image to rebuild when a
// file changes, and the same tree the Node harness passes to `--dir`, so the
// two hosts are measured against identical bytes.
import { untar } from './tar.js';
import { fetchBytes, runProgram } from './runtime.js';
import { metadataSection } from '../../shims/browser/js/gex.mjs';

/** An xterm-shaped object that keeps the bytes instead of drawing them. */
function collector(rows = 40, cols = 120) {
  const dec = new TextDecoder();
  let text = '';
  let onData = null;
  let waiters = [];
  const term = {
    rows,
    cols,
    write(d) {
      text += typeof d === 'string' ? d : dec.decode(d, { stream: true });
      const still = [];
      for (const w of waiters) {
        if (w.test(text)) w.resolve();
        else still.push(w);
      }
      waiters = still;
    },
    onData(cb) {
      onData = cb;
      return { dispose() {} };
    },
    onResize() {
      return { dispose() {} };
    },
    focus() {},
  };
  return {
    term,
    send: (s) => onData?.(s),
    /** Wait until the collected output satisfies `test`, then take it all. */
    until(test, ms) {
      if (test(text)) {
        const t = text;
        text = '';
        return Promise.resolve(t);
      }
      return new Promise((resolve, reject) => {
        const w = {
          test,
          resolve: () => {
            clearTimeout(timer);
            const t = text;
            text = '';
            resolve(t);
          },
        };
        const timer = setTimeout(() => {
          waiters = waiters.filter((x) => x !== w);
          const t = text;
          text = '';
          reject(new Error(`timed out after ${ms}ms; output so far:\n${t}`));
        }, ms);
        waiters.push(w);
      });
    },
    take() {
      const t = text;
      text = '';
      return t;
    },
  };
}

/** Everything the page logged, so a command can be answered with it. */
const logs = [];
function captureConsole() {
  const real = console.log.bind(console);
  console.log = (...a) => {
    logs.push(a.map(String).join(' '));
    real(...a);
  };
}

export async function start() {
  captureConsole();
  // Anything that throws where nobody is catching, said out loud.
  //
  // `runProgram`'s rejection was the only failure this reported, so a throw
  // in the page's own code, in a Worker, or in a promise nobody awaited left
  // the driver waiting for an answer that could not come -- and the run
  // looked like a slow one rather than a broken one. `apt-get update` has
  // apparently never worked in a browser, and this is why nobody could see
  // what it does.
  self.addEventListener('error', (e) => {
    report({ fatal: `${e.message} at ${e.filename}:${e.lineno}` });
  });
  self.addEventListener('unhandledrejection', (e) => {
    const r = e.reason;
    report({ fatal: `unhandled rejection: ${(r && (r.stack || r.message)) || String(r)}` });
  });
  const params = new URLSearchParams(location.search);
  const shell = params.get('shell') || '/bin/dash';
  // `?recompile=0` runs the browser the way `run-node.mjs` runs by default,
  // and `?debug=1` mirrors every guest write to the console and narrates a
  // crash where it happens. Both exist because the two hosts disagreed about
  // whether a child process works at all, and neither difference could be
  // switched off from outside the bundle.
  const recompileObjects = params.has('recompile') ? Number(params.get('recompile')) : 8192;
  if (params.get('debug')) globalThis.GEX_DEBUG_EXIT = 1;
  // \`?adopt=N\`: how many traps a forked child takes before it stops waiting
  // to be an execve and takes its parent's modules in bulk. 1 is the old
  // eager behaviour, which is what a bisect needs.
  if (params.has('adopt')) globalThis.GEX_ADOPT_AFTER = Number(params.get('adopt'));
  // And the engine's own stack for a trap, which names the wasm function the
  // trap happened in. The rewrapped error says what went wrong and where the
  // *guest* was; when the fault is in the shim rather than in translated
  // code, only this says which of the shim's own functions was running.
  if (params.get('debug')) globalThis.GEX_TRAP_STACK = 1;
  // A page cannot open a TCP socket, so the guest's network is carried over
  // Wisp to a proxy. Without one `apt-get update` reaches no mirror, which is
  // the only command anybody wants to measure here.
  let network = null;
  if (params.get('wisp')) {
    const { WispNetwork } = await import('./net.js');
    network = new WispNetwork(params.get('wisp'));
    await network.start();
  }
  const t0 = performance.now();

  const tar = await fetchBytes('/harness/rootfs.tar');
  const tree = untar(tar);
  const files = { ...tree.files };
  // Symlinks, resolved the way the demos do it: the guest filesystem holds
  // bytes, so a link becomes another name for the same bytes.
  for (const [path, target] of Object.entries(tree.links || {})) {
    const abs = target.startsWith('/')
      ? target
      : `${path.slice(0, path.lastIndexOf('/'))}/${target}`.replace(/\/{2,}/g, '/');
    if (files[abs]) files[path] = files[abs];
  }
  const elf = files[shell];
  if (!elf) throw new Error(`the rootfs has no ${shell}`);

  // The closure, when the driver was given one: the image module, every
  // library beside it, and the layout they were placed at. This is the
  // configuration the project exists for, and no page could be given one
  // before -- so the browser had only ever been measured translating at
  // runtime, which is the secondary path.
  let closure = null;
  if (params.get('closure')) {
    const m = await (await fetch('/harness/closure.json')).json();
    // The libraries, compiled here and once. A closure is hundreds of
    // megabytes and every Worker would otherwise be sent the bytes and
    // compile them itself; a `WebAssembly.Module` structured-clones as
    // itself and the engine shares the code behind it, so all a Worker needs
    // besides the module is the small `gex.meta` section that says which
    // pages it covers.
    // A few at a time, not all nineteen at once.
    //
    // `Promise.all` over the lot had every library in flight together: 694 MB
    // of wasm fetched and handed to `WebAssembly.compile` concurrently, on
    // the main thread, before the shell has run an instruction. A 77 MB
    // closure boots a shell in 1.65s; the 694 MB one never posted ready at
    // all, and nothing said whether it was fetching, compiling or installing.
    //
    // Four is a guess and the log below is how to improve on it. What is not
    // a guess is that unbounded was wrong: the peak is what matters here, and
    // the peak was all of it.
    const AT_A_TIME = 4;
    const libs = [];
    const began = performance.now();
    for (let i = 0; i < m.libs.length; i += AT_A_TIME) {
      const batch = m.libs.slice(i, i + AT_A_TIME);
      const done = await Promise.all(
        batch.map(async (n) => {
          const t0 = performance.now();
          const bytes = await fetchBytes(`/harness/closure/${encodeURIComponent(n)}`);
          const fetched = performance.now();
          const mod = await WebAssembly.compile(bytes);
          console.log(
            `closure ${n}: ${(bytes.length / 1048576).toFixed(1)} MiB, `
            + `${Math.round(fetched - t0)}ms fetching, `
            + `${Math.round(performance.now() - fetched)}ms compiling`,
          );
          return { meta: metadataSection(bytes), mod };
        }),
      );
      libs.push(...done);
    }
    console.log(`closure ready: ${libs.length} libraries in ${Math.round(performance.now() - began)}ms`);
    // The image module is one program's own text, and pid 1 here is a shell.
    // Handing dash a module built for `apt-get` starts the shell inside
    // another program's code, and it exits without a syscall -- which is
    // exactly what the first attempt did. Only on request, and only when the
    // driver knows the shell *is* that program.
    const image = params.get('image')
      ? await fetchBytes(`/harness/closure/${encodeURIComponent(m.image)}`)
      : null;
    closure = { image, libs, pins: m.pins };
  }

  const io = collector();
  const unpacked = performance.now() - t0;

  // `?program=` runs one program instead of opening a shell.
  //
  // A closure is a layout for one program: `gexc build --closure` pins every
  // object it reaches at a fixed address, and those addresses start at
  // 0x5000000 -- which is exactly where a shell's own image and libraries
  // land. Booting dash and then handing it apt's pins put two programs at one
  // address and the shell died before it printed a prompt. So the
  // configuration this project exists for cannot be measured through a
  // persistent shell at all, and this is the mode that measures it: the page
  // boots, runs the program, and reports what it printed and what it exited
  // with.
  if (params.get('program')) {
    const path = params.get('program');
    const argv = [path, ...(params.getAll('arg') || [])];
    const exe = files[path];
    if (!exe) throw new Error(`the rootfs has no ${path}`);
    const began = performance.now();
    let status = -1;
    let failed = null;
    try {
      // Against a deadline, because this path awaits the guest and nothing
      // else. A program that wedges -- and a closure run does, see #105 --
      // otherwise leaves the harness waiting for ever with no output and no
      // diagnosis, which is a worse failure than a wrong answer: the run has
      // to be noticed, found and killed by hand.
      const running = runProgram(io.term, {
        elf: exe,
        files,
        args: argv,
        cwd: '/',
        tty: false,
        uid: 0,
        recompileObjects,
        network,
        closure,
      });
      const limitMs = Number(params.get('wait') || 0);
      const r = limitMs
        ? await Promise.race([
            running,
            new Promise((_, bad) => {
              const fire = () => bad(new Error(`the program did not finish in ${limitMs}ms`));
              globalThis.setTimeout(fire, limitMs);
            }),
          ])
        : await running;
      status = r.status;
    } catch (e) {
      failed = String((e && e.stack) || e);
    }
    await fetch('/harness/ready', {
      method: 'POST',
      body: JSON.stringify({ unpackMs: Math.round(unpacked), files: Object.keys(files).length }),
    });
    await report({
      id: 0,
      out: io.take(),
      error: failed,
      status,
      ms: Math.round(performance.now() - began),
      log: logs.slice(),
    });
    return;
  }
  // Not awaited: an interactive shell does not exit, and this is the process
  // every command runs inside.
  const done = runProgram(io.term, {
    elf,
    files,
    args: [shell, '-i'],
    cwd: '/',
    tty: true,
    uid: 0,
    recompileObjects,
    network,
    closure,
  });
  done.catch((e) => report({ fatal: String(e && e.message) }));

  // The prompt, whatever it looks like: the shell has started when it stops
  // producing output for a moment.
  await settle(io, 30000);
  await fetch('/harness/ready', {
    method: 'POST',
    body: JSON.stringify({ unpackMs: Math.round(unpacked), files: Object.keys(files).length }),
  });

  // And now answer commands until the driver goes away.
  for (let n = 0; ; n++) {
    let job;
    try {
      const r = await fetch('/harness/next');
      if (r.status !== 200) continue;
      job = await r.json();
    } catch {
      // The driver stopped. Nothing useful left to do.
      return;
    }
    // Split across a pair of empty quotes, so the characters *typed* never
    // spell the marker -- the terminal echoes input back in cooked mode, and a
    // literal marker in the command line matched the instant it was sent.
    // Every command then "finished" in 0ms with no output.
    // Bracketed, not trimmed. The output used to be everything before the end
    // marker with its first line dropped, on the assumption that the first
    // line is the terminal echoing what was typed -- and when the shell does
    // not echo, that is the answer being thrown away: `echo alive` reported
    // nothing at all.
    // A rebuilt bundle: drop everything and come back as a new page. The
    // driver is waiting on `/harness/ready`, which `start` posts again.
    if (job.reload) {
      location.reload();
      return;
    }
    const mark = `__GEX_${n}_DONE__`;
    const typed = `__GEX_${n}_DO""NE__`;
    const startMark = `__GEX_${n}_HERE__`;
    const startTyped = `__GEX_${n}_HE""RE__`;
    logs.length = 0;
    const started = performance.now();
    let out;
    let error = null;
    try {
      io.take();
      io.send(`echo ${startTyped}; ${job.cmd}; echo ${typed}\n`);
      const raw = await io.until((t) => t.includes(mark), job.timeoutMs || 120000);
      // Between the markers, and nothing else: whatever the terminal did or
      // did not echo falls outside them.
      const from = raw.indexOf(startMark);
      const body = from < 0 ? raw : raw.slice(from + startMark.length);
      out = body.split(mark)[0].replace(/^\r?\n/, '');
    } catch (e) {
      out = '';
      error = String(e.message);
    }
    await report({
      id: job.id,
      out,
      error,
      ms: Math.round(performance.now() - started),
      log: logs.slice(),
    });
  }
}

function report(body) {
  return fetch('/harness/result', { method: 'POST', body: JSON.stringify(body) }).catch(() => {});
}

/** Resolve once the guest has been quiet for 400ms. */
function settle(io, ms) {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + ms;
    let last = '';
    const tick = setInterval(() => {
      const now = io.take();
      last += now;
      if (!now && last) {
        clearInterval(tick);
        resolve(last);
      } else if (Date.now() > deadline) {
        clearInterval(tick);
        reject(new Error('the shell never printed a prompt'));
      }
    }, 400);
  });
}
