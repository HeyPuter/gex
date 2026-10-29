# @gex/runtime

Linux programs in a web page: a kernel, the programs it runs, and their files.
A page builds a `System`, gives it a filesystem, boots it, and runs programs in
it with a terminal or with pipes. Windows, persistence, network, sound and GL
are parts of their own, each a class that takes the system as its first
argument. This first version has the system and its filesystem.

```js
import { System } from '@gex/runtime';
import { Filesystem } from '@gex/runtime/fs';

const sys = new System();
const fs = new Filesystem(sys);
fs.addTar('/trees/base.tar.zst');
await sys.boot();

const { code, stdout } = await sys.run(['uname', '-a']);

const sh = sys.spawn(['bash', '-l'], { pty: { cols: 80, rows: 24 }, env: { TERM: 'xterm-256color' } });
sh.pty.addEventListener('data', (e) => term.write(e.data));
term.onData((d) => sh.pty.write(d));
```

## The page

- Programs run in Workers on a shared memory, so the page must be
  cross-origin isolated: `Cross-Origin-Opener-Policy: same-origin` and
  `Cross-Origin-Embedder-Policy: require-corp`.
- The runtime fetches its files from `gex/`, beside its modules. A bundler
  copies the package as it is. `new System({ baseUrl })` names another place,
  the directory that holds `gex/`.
- One system per page.

## System

| Member | Does |
| --- | --- |
| `new System({ baseUrl, debug })` | A system, not started. `debug` holds the runtime's debug switches (`{ strace: 1 }`, `{ console: true }` to log pid 1's console, `{ arch: 'x86' }` for an x86-64 guest; aarch64 is the default). |
| `boot()` | Starts the kernel and pid 1. Parts that work before boot (the filesystem) must be made before it. |
| `spawn(argv, opts)` | Starts a program and returns its `Process` at once. `opts`: `pty: { cols, rows, encoding }` for a terminal, else three pipes; `env` (`{K: V}` or `['K=V']`; nothing is inherited), `cwd`, `uid`, `gid`, `detached` (no terminal or pipes: output goes nowhere). The program is found on `env.PATH`, or in `/usr/local/bin:/usr/bin:/bin` and the `sbin`s. |
| `run(argv, opts)` | `spawn`, then `{ code, signal, stdout, stderr }` once it ends. `input` is written to its stdin; `encoding` (default `'utf-8'`, `null` for bytes). |
| `state` | `{ phase, error, console }`; phase is `new`, `booting`, `running`, `stopped` or `failed`. A `running` event when it boots, `console` events for what pid 1 and the runtime say. |
| `describe()` | `{ phase, processes, pids, threads, memory }`. |
| `shutdown()` | Ends every program and the kernel. |

## Process

| Member | Does |
| --- | --- |
| `pid` | Its pid once it runs. |
| `started` | Resolves with the pid; rejects when the program could not start (`no such program`, with `errno`). |
| `exited` | Resolves with `{ code, signal }`: a program killed by a signal has `signal` set (`'SIGTERM'`) and `code` 128 plus its number. An `exit` event too. |
| `pty` | With a terminal: `write(data)` (a string or bytes, any size), `resize(cols, rows)` (the program hears `SIGWINCH`), and `data` events (bytes, or strings with an `encoding`). |
| `stdin`, `stdout`, `stderr` | Without a terminal: a `WritableStream` and two `ReadableStream`s of bytes. |
| `kill(signal)` | `'SIGTERM'` by default; a name or a number. |

## Filesystem

One per system. Before boot, it is the tars and files a page adds, laid one
over another in the order added (a later one wins). After boot, a tar or
files are written into the running tree.

| Member | Does |
| --- | --- |
| `addTar(src, { at, lazy })` | A tar (zstd or not) as bytes, a `Blob`, a `Response`, a stream or a URL, laid at `at` (default `/`). `lazy`: files fetched when first read, `[[path, size, url], ...]` or the URL of such a list (JSON); their URLs must be on the page's own origin and answer range requests. |
| `addFiles({ path: data })` | Single files: bytes, a `Blob`, a URL, or a string (the file's text). |
| `readFile(path, { encoding })` | Bytes, or text with an `encoding`. |
| `writeFile(path, data, { mode, append, exclusive })` | Writes the file, made if missing. |
| `stat(path, { follow })` | `{ type, mode, size, ino, mtime, nlink }`; `follow: false` does not follow a last symlink. |
| `readdir(path)` | `[{ name, type }]`. |
| `mkdir(path, { recursive, mode })`, `rm(path, { recursive, force })`, `rename(from, to)`, `symlink(target, path)`, `readlink(path)`, `chmod(path, mode)` | As their commands. |
| `watch(path, fn)` | `fn({ type, path })` for each change to a directory's entries or to a file, by a program or the page: `create`, `change` (a write finished), `attrib`, `delete`, `move-out` and `move-in` (one rename's halves share a `cookie`), and `gone` when `path` itself went. Returns a function that stops it. |

Errors carry `code` (`'ENOENT'`, ...) and `errno`.

## Building

`web/runtime/build.sh DIR` builds the package into `DIR`: its modules, and
under `gex/` the shim, kernel and seed of each guest architecture the tree has
built (`shims/browser/build.sh`, `GEX_ARCH=x86 shims/browser/build.sh`) and pid
1, `gex-init` (`web/runtime/init`, built with `gexcc`).

## Tests

`node web/runtime/test/run.mjs` runs each test page here in headless Chromium
against a vite dev server. `BUSYBOX` names a wasm32 busybox, which with its
applets and `test/probe` is the test tree.
