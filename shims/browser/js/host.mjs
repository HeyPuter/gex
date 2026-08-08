// The page's half of a guest system: one Worker per guest process.
//
// This is what replaced the snapshot switcher. That switcher existed because
// the page had one wasm stack and one address space, so running a second
// process meant writing the first one out and reading the second one in --
// 223 MiB per switch during the gears demo. A Worker per process removes the
// cause rather than the cost: each has a `WebAssembly.Memory` of its own, so
// nothing collides and nothing is snapshotted.
//
// What the host has to do is small, and it is all here:
//
//   * make the first Worker and give it a program,
//   * make another whenever a Worker says its guest forked, and hand that one
//     the record the fork produced,
//   * carry output out and input in,
//   * hold the run open until the *first* process exits, which is the one the
//     caller asked for.
//
// Everything the processes share -- the process table, the filesystem, the
// pipes -- is in the global kernel, a second wasm module every Worker
// instantiates against one shared memory. None of it passes through here.
// That is deliberate: a syscall that had to be a message to this file and a
// wait for the answer would be slower than the switcher it replaced.

/**
 * A guest system: one process, and whatever it forks.
 *
 * `makeWorker` is the only part that differs between a page and Node, and it
 * is a callback rather than a branch because this file has no business
 * knowing which it is running under.
 */
export class ProcessHost {
  #makeWorker; #shim; #guest; #kernel; #gexUrl;
  #onOutput; #onFork; #readStdin;
  #workers = new Map();
  #pending = new Map();
  #first = null;
  #forked = 0;

  constructor({
    makeWorker,
    shim,
    guest,
    kernel = null,
    gexUrl = null,
    onOutput = () => {},
    onFork = () => {},
    readStdin = null,
  }) {
    this.#makeWorker = makeWorker;
    this.#shim = shim;
    this.#guest = guest;
    this.#kernel = kernel;
    this.#gexUrl = gexUrl;
    this.#onOutput = onOutput;
    this.#onFork = onFork;
    this.#readStdin = readStdin;
  }

  /** How many children the guest has forked. */
  get forked() { return this.#forked; }

  /** Every live process, by pid. Pid 1 is the one `start` was given. */
  get pids() { return [...this.#workers.keys()]; }

  /**
   * Run `elf` as pid 1 and resolve with its exit status.
   *
   * Children outlive it only as long as it takes to stop them: a page that
   * has finished with a program has finished with the daemons it started,
   * and leaving a Worker running would leave the guest's clock ticking in a
   * tab nobody is looking at.
   */
  async start({ elf, files = {}, args = [], opts = {} }) {
    const status = await this.#spawn(1, {
      kind: 'run',
      elf,
      files,
      args,
      opts,
    });
    this.stop();
    return status;
  }

  /** Deliver a line of standard input to pid 1. */
  pushStdin(bytes) {
    this.#first?.postMessage({ kind: 'stdin', bytes });
  }

  /** Say there will be no more input. */
  endStdin() {
    this.#first?.postMessage({ kind: 'eof' });
  }

  /** Stop every Worker, whatever it is doing. */
  stop() {
    for (const w of this.#workers.values()) w.terminate();
    this.#workers.clear();
  }

  /**
   * One process in one Worker.
   *
   * `msg` is either `run` -- load a program -- or `adopt`, which is what a
   * forked child gets: a copy of its parent's memory and the few words that
   * differ between the two.
   */
  #spawn(pid, msg) {
    return new Promise((resolve, reject) => {
      const w = this.#makeWorker();
      this.#workers.set(pid, w);
      if (pid === 1) this.#first = w;
      w.addEventListener?.('message', (e) => this.#onMessage(pid, w, e.data, resolve, reject));
      w.on?.('message', (m) => this.#onMessage(pid, w, m, resolve, reject));
      const fail = (e) => {
        this.#workers.delete(pid);
        reject(e instanceof Error ? e : new Error(e?.message ?? String(e)));
      };
      w.addEventListener?.('error', fail);
      w.on?.('error', fail);
      this.#pending.set(pid, { ...msg, pid });
    });
  }

  #onMessage(pid, w, m, resolve, reject) {
    switch (m?.kind) {
      case 'ready': {
        const msg = this.#pending.get(pid);
        this.#pending.delete(pid);
        if (msg) {
          w.postMessage({
            ...msg,
            gexUrl: this.#gexUrl ?? undefined,
            shim: this.#shim,
            guest: this.#guest,
            kernel: this.#kernel ?? undefined,
          });
        }
        break;
      }
      case 'output':
        this.#onOutput(m.fd, m.text, pid);
        break;
      case 'fork':
        this.#forked++;
        this.#onFork(m.child, pid);
        // Started and then let run. A child is a process in its own right;
        // the difference from a switch is exactly that nobody waits for it.
        this.#spawn(m.child.pid, { kind: 'adopt', child: m.child }).catch((e) => {
          this.#onOutput(2, `gex: process ${m.child.pid} failed: ${e.message}\n`, pid);
        });
        break;
      case 'stdin': {
        // Only pid 1 has a terminal behind it; a child that reads standard
        // input reads the descriptor it inherited, which is the kernel's
        // business rather than this file's.
        if (!this.#readStdin) {
          w.postMessage({ kind: 'eof' });
          break;
        }
        Promise.resolve(this.#readStdin()).then((bytes) => {
          if (bytes && bytes.length) w.postMessage({ kind: 'stdin', bytes });
          else w.postMessage({ kind: 'eof' });
        });
        break;
      }
      case 'exit':
        this.#workers.delete(pid);
        w.terminate();
        resolve(m.status);
        break;
      case 'threw':
        this.#workers.delete(pid);
        w.terminate();
        reject(new Error(m.message));
        break;
    }
  }
}
