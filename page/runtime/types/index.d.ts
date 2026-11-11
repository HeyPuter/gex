// @gexemu/runtime: the System and the programs it runs. README.md says what
// each does; these are its shapes.

import type { Network } from './net';
export type { Duplex, Network } from './net';

export type Env = Record<string, string> | string[];
export type Bytes = Uint8Array;
/** What the runtime takes as a file's or a tar's contents. */
export type Source = Uint8Array | ArrayBuffer | ArrayBufferView | Blob | Response | ReadableStream<Uint8Array> | URL | string;
export type SignalName = keyof typeof SIGNALS | string;

export declare const SIGNALS: Readonly<{
  SIGHUP: 1; SIGINT: 2; SIGQUIT: 3; SIGILL: 4; SIGTRAP: 5; SIGABRT: 6; SIGBUS: 7; SIGFPE: 8;
  SIGKILL: 9; SIGUSR1: 10; SIGSEGV: 11; SIGUSR2: 12; SIGPIPE: 13; SIGALRM: 14; SIGTERM: 15;
  SIGCHLD: 17; SIGCONT: 18; SIGSTOP: 19; SIGTSTP: 20; SIGTTIN: 21; SIGTTOU: 22; SIGWINCH: 28;
}>;

export interface SystemOptions {
  /** The directory that holds `gex/` (default: beside the modules). */
  baseUrl?: string;
  /** Compiled function-tier units for the tree's images (`gexc cgen-image`'s
   *  cache): a URL, or `{ url, require }`, where `require` fails an image
   *  that has none. */
  units?: string | URL | { url: string | URL; require?: boolean };
  /** Keep translated code across loads (the origin's private file system). */
  /** Keep translated code across loads; a string names the cache, apart from other names'. */
  xcache?: boolean | string;
  /** The translated architecture the tree's programs are built for. Default:
   *  read from the tree at boot. Given, the kernel and runtime load while the
   *  page lays out its files. */
  arch?: 'x86' | 'aarch64';
  /** Switches for debugging (`{ strace: 1 }`, `{ console: true }`, `{ log: '1' }`
   *  for the runtime's default lines); not a stable API. */
  debug?: Record<string, unknown>;
}

export interface SpawnOptions {
  /** A terminal; without one, three pipes. */
  pty?: { cols?: number; rows?: number; encoding?: string };
  /** The whole environment: nothing is inherited. */
  env?: Env;
  cwd?: string;
  uid?: number;
  gid?: number;
  /** No terminal and no pipes: output goes nowhere. */
  detached?: boolean;
}

export interface RunOptions extends Omit<SpawnOptions, 'pty' | 'detached'> {
  /** Written to the program's standard input, which is then closed. */
  input?: string | Uint8Array;
  /** Default 'utf-8'; null for bytes. */
  encoding?: string | null;
}

export interface RunResult<T = string> {
  code: number;
  signal: string | null;
  stdout: T;
  stderr: T;
}

export type Phase = 'new' | 'booting' | 'running' | 'stopped' | 'failed';

export interface SystemState {
  phase: Phase;
  error: string | null;
  /** What the runtime said. */
  console: string;
  /** How the init ended, which stopped the system. */
  init?: { code: number; signal: string | null } | { error: string };
}

/** A program that crashed (System's `crash` event): one a crash signal
 *  ended, or a Windows program Wine ended for an unhandled exception. It
 *  has ended by the time the event comes. */
export interface CrashEvent extends Event {
  readonly pid: number;
  /** Its program's name (a Windows program's `.exe`). */
  readonly name: string;
  /** What happened, in a few words ("page fault on read access to 00000000 at address 004017DD", "SIGSEGV"). */
  readonly reason: string;
  /** Its exit status: a Windows exception's code (0xc0000005), else 128 plus the signal. */
  readonly code: number;
  /** The signal that ended it, 0 for none. */
  readonly signal: number;
}

export interface HostCalls {
  /** `fn(value)` for a guest's call NAME on /dev/gex-host; returns a function that unregisters it. */
  register(name: string, fn: (value: unknown) => unknown): () => void;
  unregister(name: string): void;
  names(): string[];
}

export interface GuestConnection extends EventTarget {
  readable: ReadableStream<Uint8Array>;
  writable: WritableStream<Uint8Array | string>;
  close(): void;
}

/** `sys.listen`'s: the connections programs make to a unix socket name the page listens on. */
export interface UnixListener extends AsyncIterable<GuestConnection> {
  readonly path: string;
  readonly closed: boolean;
  /** Resolves once the name is bound (at boot for one made before it); rejects with its errno's code (EADDRINUSE, ENOENT, ...). */
  readonly ready: Promise<void>;
  /** The next connection; null once closed. */
  accept(): Promise<GuestConnection | null>;
  /** Stop listening: a program's connect is then refused (the socket file stays). Leaving a `for await` loop over it closes it too. */
  close(): Promise<void>;
}

export declare class System extends EventTarget {
  constructor(options?: SystemOptions);
  get state(): SystemState;
  /** With arguments (`spawn`'s), the program that is pid 1; without, pid 1 is the kernel's own. */
  boot(argv?: string[], options?: SpawnOptions): Promise<void>;
  /** Throws when the system has an init. */
  spawn(argv: string[], options?: SpawnOptions): Process;
  /** The init's Process, or null without one. */
  get init(): Process | null;
  run(argv: string[], options?: RunOptions & { encoding?: string }): Promise<RunResult<string>>;
  run(argv: string[], options: RunOptions & { encoding: null }): Promise<RunResult<Uint8Array>>;
  get hostCalls(): HostCalls;
  /** Where programs' connections out go (a Network the page implements), or null: then they fail as unreachable. Set before or after boot; one at a time. */
  get network(): Network | null;
  set network(network: Network | null);
  get net(): { connectGuest(port: number, options?: { host?: string }): Promise<GuestConnection> };
  /** Listen on a unix socket name (an absolute path, or an abstract name with a leading NUL): each program's AF_UNIX stream connect to it is a GuestConnection. Before or after boot. */
  listen(path: string): UnixListener;
  /** Connect to a program listening on a unix socket name; rejects with code ECONNREFUSED or ENOENT as a program's connect would fail. */
  connect(path: string): Promise<GuestConnection>;
  describe(): { phase: Phase; arch: string | null; processes: { pid: number | null; argv: string[] }[]; pids: number[]; threads: number; memory: { kernel: number } };
  /** What the programs wrote made durable in the filesystem's store (sync(2)). */
  sync(): Promise<void>;
  shutdown(): Promise<void>;
}

export declare class DataEvent extends Event {
  readonly data: string | Uint8Array;
}

export declare class Pty extends EventTarget {
  /** A string or bytes, any size. */
  write(data: string | Uint8Array): void;
  /** The program hears SIGWINCH. */
  resize(cols: number, rows: number): void;
}

export declare class Process extends EventTarget {
  readonly argv: string[];
  /** Its pid, once it runs. */
  get pid(): number | null;
  /** Resolves with the pid; rejects when the program could not start (with `errno`). */
  readonly started: Promise<number>;
  readonly exited: Promise<{ code: number; signal: string | null }>;
  /** With a terminal. */
  readonly pty: Pty | null;
  /** Without a terminal. */
  readonly stdin: WritableStream<Uint8Array> | null;
  readonly stdout: ReadableStream<Uint8Array> | null;
  readonly stderr: ReadableStream<Uint8Array> | null;
  kill(signal?: SignalName | number): void;
  /** Its windows, with HostWindows. */
  windows(): import('./windows-host').GuestWindow[];
}
