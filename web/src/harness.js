// A page that boots the runtime once and then answers commands over HTTP for
// ever, so the browser is paid for once instead of per measured command.
//
// The rootfs comes from a real host directory streamed as a tar the server
// makes on demand (`/harness/rootfs.tar`) -- the same tree the Node harness
// passes to `--dir`, so the two hosts are measured against identical bytes.
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
  // Anything that throws where nobody is catching, reported -- otherwise a
  // throw in a Worker or an unawaited promise leaves the driver waiting for an
  // answer that never comes, and the run looks slow rather than broken.
  self.addEventListener('error', (e) => {
    report({ fatal: `${e.message} at ${e.filename}:${e.lineno}` });
  });
  self.addEventListener('unhandledrejection', (e) => {
    const r = e.reason;
    report({ fatal: `unhandled rejection: ${(r && (r.stack || r.message)) || String(r)}` });
  });
  const params = new URLSearchParams(location.search);
  const shell = params.get('shell') || '/bin/dash';
  // `?recompile=0` runs the browser the way `run-node.mjs` does by default.
  const recompileObjects = params.has('recompile') ? Number(params.get('recompile')) : 8192;
  // `?link=1` keeps the address space the guest's `ld.so` produced and starts
  // the next process of the same program from it, rather than linking again.
  const link = Number(params.get('link') ?? 0);
  if (params.get('debug')) globalThis.GEX_DEBUG_EXIT = 1;
  // `?adopt=N`: how many traps a forked child takes before it stops waiting to
  // be an execve and takes its parent's modules in bulk. 1 is the old eager
  // behaviour, which is what a bisect needs.
  if (params.has('adopt')) globalThis.GEX_ADOPT_AFTER = Number(params.get('adopt'));
  // `GEX_TRAP_STACK`: the engine's own stack for a trap, which names the wasm
  // function it happened in -- the only thing that says which of the shim's own
  // functions was running when the fault is in the shim, not translated code.
  if (params.get('debug')) globalThis.GEX_TRAP_STACK = 1;
  // A page cannot open a TCP socket, so the network is carried over Wisp.
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
  // library beside it, and the layout they were placed at.
  let closure = null;
  if (params.get('closure')) {
    const m = await (await fetch('/harness/closure.json')).json();
    // Compiled here and once: a `WebAssembly.Module` structured-clones as
    // itself with the engine sharing the code behind it, so a Worker needs only
    // the module and the `gex.meta` section saying which pages it covers.
    // A few at a time, not all at once: `Promise.all` over the lot had 694 MB of
    // wasm compiling concurrently on the main thread and never posted ready.
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
    // The image module is one program's own text: only on request, and only
    // when the driver knows pid 1 *is* that program, or the shell starts inside
    // another program's code and exits without a syscall.
    const image = params.get('image')
      ? await fetchBytes(`/harness/closure/${encodeURIComponent(m.image)}`)
      : null;
    closure = { image, libs, pins: m.pins };
  }

  const io = collector();
  const unpacked = performance.now() - t0;

  // `?program=` runs one program instead of opening a shell.
  //
  // A closure pins every object at a fixed address starting at 0x5000000, which
  // is where a shell's own image lands too -- so a closure cannot be measured
  // through the persistent shell without two programs colliding at one address.
  // This mode boots, runs the program, and reports its output and exit.
  if (params.get('program')) {
    const path = params.get('program');
    const argv = [path, ...(params.getAll('arg') || [])];
    const exe = files[path];
    if (!exe) throw new Error(`the rootfs has no ${path}`);
    const began = performance.now();
    let status = -1;
    let failed = null;
    try {
      // Against a deadline (`?wait=`): a program that wedges -- a closure run
      // does, see #105 -- otherwise leaves the harness waiting for ever.
      const running = runProgram(io.term, {
        elf: exe,
        files,
        args: argv,
        cwd: '/',
        tty: false,
        uid: 0,
        recompileObjects,
        link,
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
    link,
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
    // Markers are split across empty quotes so the characters *typed* never
    // spell them (the terminal echoes input in cooked mode), and the answer is
    // taken bracketed between them rather than trimmed.
    // `job.reload`: drop everything and come back as a new page. The driver is
    // waiting on `/harness/ready`, which `start` posts again.
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
      // Between the markers: whatever the terminal echoed falls outside them.
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
