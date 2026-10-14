// The guest's WebGPU device (/dev/gex-gpu, guest/gpu/transport-dev.h),
// reached from a Worker. The wire's server (web/build-gexgpu-server.sh) runs
// where the page's WebGPU is; `gpu_submit` is a wasm import called inside an
// ioctl, so the call crosses a SharedArrayBuffer with `Atomics.wait`, as the
// GL device's does (gl-bridge.mjs): the Worker copies the message in, wakes
// the server, and blocks until the reply is in the same buffer.
//
// Each open of the device is a connection, with a server of its own: object
// ids are the client's to choose, and two processes choose the same ones.

const STATE = 0; // IDLE, REQUEST, REPLY
const RESULT = 1;
const CONN = 2;
// The data buffer is one, so a request claims it: the claimant's token.
const LOCK = 3;
const OP = 4;
const LENGTH = 5;
const CONTROL_WORDS = 8;
const IDLE = 0;
const REQUEST = 1;
const REPLY = 2;

const OP_OPEN = 1;
const OP_SUBMIT = 2;
const OP_CLOSE = 3;

// guest/gpu/transport-dev.h
const STATUS_OK = 0;
const STATUS_HELD = 1;
const STATUS_MALFORMED = 2;
const WIRE_VERSION = 1;
const E2BIG = 7;
const EINVAL = 22;
const EIO = 5;

export const GPU_DATA_BYTES = 64 * 1024 * 1024;

export function makeGpuChannel() {
  return {
    control: new SharedArrayBuffer(CONTROL_WORDS * 4),
    data: new SharedArrayBuffer(GPU_DATA_BYTES),
  };
}

/** The wire's server, as built for the page: a connection per open. */
export class GpuServer {
  #m;
  #servers = new Map();
  #next = 1;

  static async load(url) {
    const { default: create } = await import(/* @vite-ignore */ url);
    return new GpuServer(await create());
  }

  constructor(module) {
    this.#m = module;
  }

  open() {
    const server = this.#m._gexgpu_web_create();
    if (!server) return -1;
    const id = this.#next++;
    this.#servers.set(id, server);
    return id;
  }

  async close(id) {
    const server = this.#servers.get(id);
    if (!server) return;
    this.#servers.delete(id);
    await this.#m._gexgpu_web_destroy(server);
  }

  /** Runs `message` (bytes) on connection `id`: { reply, ok }. */
  async handle(id, message) {
    const m = this.#m;
    const server = this.#servers.get(id);
    if (!server) return { reply: new Uint8Array(0), ok: false };
    const at = m._malloc(message.length || 1);
    m.HEAPU8.set(message, at);
    let ok;
    try {
      ok = await m._gexgpu_web_handle(server, at, message.length);
    } finally {
      m._free(at);
    }
    const reply = m._gexgpu_web_reply();
    const size = m._gexgpu_web_reply_size();
    // HEAPU8 again: the call may have grown the memory.
    return { reply: m.HEAPU8.slice(reply, reply + size), ok: !!ok };
  }
}

/**
 * The server's half: answer the Workers' requests on `channel` with
 * `server` (a `GpuServer`). One request at a time; a message that waits
 * on the GPU holds the others back, as one queue would.
 */
export function serveGpu(server, channel) {
  const ctl = new Int32Array(channel.control);
  const data = new Uint8Array(channel.data);
  let active = true;
  const serve = async () => {
    const conn = ctl[CONN];
    let result = 0;
    try {
      switch (ctl[OP]) {
        case OP_OPEN:
          result = server.open();
          break;
        case OP_CLOSE:
          await server.close(conn);
          break;
        case OP_SUBMIT: {
          const { reply, ok } = await server.handle(conn, data.slice(0, ctl[LENGTH]));
          if (reply.length > data.length) {
            result = -E2BIG;
            break;
          }
          data.set(reply, 0);
          ctl[LENGTH] = reply.length;
          result = ok ? STATUS_OK : STATUS_MALFORMED;
          break;
        }
        default:
          result = -EINVAL;
      }
    } catch (e) {
      // Whatever happened, the Worker is blocked in its ioctl until this
      // answers.
      console.error('[gpu] the server threw', e);
      result = -EIO;
    }
    ctl[RESULT] = result;
    Atomics.store(ctl, STATE, REPLY);
    Atomics.notify(ctl, STATE);
  };
  (async () => {
    while (active) {
      const state = Atomics.load(ctl, STATE);
      if (state === REQUEST) {
        await serve();
        continue;
      }
      const waiting = Atomics.waitAsync(ctl, STATE, state, 1000);
      if (waiting.async) await waiting.value;
    }
  })();
  return { stop: () => { active = false; Atomics.notify(ctl, STATE); } };
}

/** The Worker's half: what `gpu_open`, `gpu_submit` and `gpu_close` call. */
export class GpuProxy {
  #ctl;
  #data;
  #token;
  // Replies too big for the guest's buffer, by connection, until fetched.
  #held = new Map();

  constructor(channel) {
    this.#ctl = new Int32Array(channel.control);
    this.#data = new Uint8Array(channel.data);
    this.#token = (Math.random() * 0x7fffffff) | 1;
  }

  /** One request, the data buffer already filled if it needs to be. */
  #call(op, conn, length, fill) {
    const ctl = this.#ctl;
    while (Atomics.compareExchange(ctl, LOCK, 0, this.#token) !== 0) {
      Atomics.wait(ctl, LOCK, Atomics.load(ctl, LOCK), 100);
    }
    try {
      fill?.(this.#data);
      ctl[OP] = op;
      ctl[CONN] = conn;
      ctl[LENGTH] = length;
      Atomics.store(ctl, STATE, REQUEST);
      Atomics.notify(ctl, STATE);
      while (Atomics.load(ctl, STATE) !== REPLY) Atomics.wait(ctl, STATE, REQUEST, 1000);
      const result = ctl[RESULT];
      const reply = op === OP_SUBMIT && result >= 0 ? this.#data.slice(0, ctl[LENGTH]) : null;
      Atomics.store(ctl, STATE, IDLE);
      return { result, reply };
    } finally {
      Atomics.store(ctl, LOCK, 0);
      Atomics.notify(ctl, LOCK);
    }
  }

  open() {
    return this.#call(OP_OPEN, 0, 0).result;
  }

  close(conn) {
    this.#held.delete(conn);
    this.#call(OP_CLOSE, conn, 0);
  }

  /** `struct gex_gpu_submit` at `arg` in `memory`: run its message (or,
   *  `fetch`, hand over the held reply). Zero, or a negative errno. */
  submit(memory, conn, arg, fetch) {
    const view = new DataView(memory.buffer);
    const u64 = (at) => Number(view.getBigUint64(arg + at, true));
    let answer;
    if (fetch) {
      answer = this.#held.get(conn);
      this.#held.delete(conn);
      if (!answer) return -EINVAL;
    } else {
      if (view.getUint32(arg + 44, true) !== WIRE_VERSION) return -EINVAL;
      const data = u64(0);
      const size = u64(8);
      if (size > GPU_DATA_BYTES) return -E2BIG;
      const message = new Uint8Array(memory.buffer, data, size);
      const { result, reply } = this.#call(OP_SUBMIT, conn, size, (d) => d.set(message, 0));
      if (result < 0) return result;
      answer = { reply, status: result };
    }
    const buffer = u64(16);
    const capacity = u64(24);
    view.setBigUint64(arg + 32, BigInt(answer.reply.length), true);
    if (answer.reply.length > capacity) {
      this.#held.set(conn, answer);
      view.setUint32(arg + 40, STATUS_HELD, true);
      return 0;
    }
    new Uint8Array(memory.buffer, buffer, answer.reply.length).set(answer.reply);
    view.setUint32(arg + 40, answer.status, true);
    return 0;
  }
}
