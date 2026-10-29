// What the runtime's classes share and a page does not use: a system's
// internals, under a symbol rather than a name a page could reach by
// accident.

export const INTERNAL = Symbol.for('gex.runtime.internal');

/** The internals of `sys`, checked to be a System's. */
export function internals(sys, who) {
  const i = sys?.[INTERNAL];
  if (!i) throw new TypeError(`${who} takes a System as its first argument`);
  return i;
}

/** Linux's signal numbers, which every guest architecture gex runs shares. */
export const SIGNALS = Object.freeze({
  SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGILL: 4, SIGTRAP: 5, SIGABRT: 6, SIGBUS: 7, SIGFPE: 8,
  SIGKILL: 9, SIGUSR1: 10, SIGSEGV: 11, SIGUSR2: 12, SIGPIPE: 13, SIGALRM: 14, SIGTERM: 15,
  SIGCHLD: 17, SIGCONT: 18, SIGSTOP: 19, SIGTSTP: 20, SIGTTIN: 21, SIGTTOU: 22, SIGWINCH: 28,
});

export function signalNumber(s) {
  if (typeof s === 'number' && Number.isInteger(s) && s >= 0 && s < 65) return s;
  const n = SIGNALS[String(s).toUpperCase().replace(/^(?!SIG)/, 'SIG')];
  if (n === undefined) throw new TypeError(`no signal ${s}`);
  return n;
}

export function signalName(n) {
  return Object.keys(SIGNALS).find((k) => SIGNALS[k] === n) ?? `SIG${n}`;
}

/** `{K: V}` or `['K=V']` as `['K=V']`. */
export function envList(env) {
  if (!env) return [];
  if (Array.isArray(env)) return env.map(String);
  return Object.entries(env).map(([k, v]) => `${k}=${v}`);
}

/** Bytes from what a page may hand over as a file's contents or a tar:
 *  bytes, a buffer, a Blob, a Response, a stream, or a URL (fetched). */
export async function bytesOf(src, what = 'source') {
  if (src instanceof Uint8Array) return src;
  if (src instanceof ArrayBuffer || (typeof SharedArrayBuffer === 'function' && src instanceof SharedArrayBuffer)) return new Uint8Array(src);
  if (ArrayBuffer.isView(src)) return new Uint8Array(src.buffer, src.byteOffset, src.byteLength);
  if (typeof Blob === 'function' && src instanceof Blob) return new Uint8Array(await src.arrayBuffer());
  if (typeof Response === 'function' && src instanceof Response) return new Uint8Array(await src.arrayBuffer());
  if (typeof ReadableStream === 'function' && src instanceof ReadableStream) return new Uint8Array(await new Response(src).arrayBuffer());
  if (src instanceof URL || typeof src === 'string') {
    // Under Node (the runtime's own tests), a file URL is a file.
    if (String(src).startsWith('file:') && globalThis.process?.versions?.node) {
      const { readFile } = await import('node:fs/promises');
      return new Uint8Array(await readFile(new URL(src)));
    }
    const res = await fetch(src);
    if (!res.ok) throw new Error(`${what} ${src}: ${res.status} ${res.statusText}`);
    return new Uint8Array(await res.arrayBuffer());
  }
  throw new TypeError(`${what}: not bytes, a Blob, a stream or a URL`);
}
