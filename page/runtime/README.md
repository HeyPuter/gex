# @gexemu/runtime

Linux programs in a web page: a kernel, the programs it runs, and their files.
A page builds a `System`, gives it a filesystem, boots it, and runs programs in
it with a terminal or with pipes. Its filesystem may keep the root across
loads. Windows, network, sound and GL are parts of their own, each a class that takes the system as its first
argument.

```js
import { System } from '@gexemu/runtime';
import { MemoryFilesystem } from '@gexemu/runtime/fs';

const sys = new System();
const fs = new MemoryFilesystem(sys);
fs.addTar('/trees/base.tar.zst');
await sys.boot();

const { code, stdout } = await sys.run(['uname', '-a']);

const sh = sys.spawn(['bash', '-l'], { pty: { cols: 80, rows: 24 }, env: { TERM: 'xterm-256color' } });
sh.pty.addEventListener('data', (e) => term.write(e.data));
term.onData((d) => sh.pty.write(d));
```

## In a web desktop

How a web desktop (AnuraOS, say) runs Linux and Windows programs through
the runtime. Its windows go to the desktop's own window manager, and its root
stays across loads. Each step is one of the parts below; the reference sections
say the rest.

**Serve it.** Import the package from the desktop's code, through its bundler
or served as it is, from the desktop's own origin. Every page that makes a system must be served with
`Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp`. Anything the page loads from
another origin (an iframe, an image) then needs CORP or CORS. Keep one
`System` for the desktop's life: a page has one.

**Give it a tree.** A root is one or more tars laid at `/`. A large tree
should be lazy: a list of files fetched from the desktop's origin when a program
first reads them, so boot fetches only what boot reads. A tree with an
aarch64 or x86-64 program gets that architecture beside wasm32 (see The
page). A Debian tree of wasm32 packages with arm64 beside them (as
debdemo builds one) runs apt as it comes, over the network below.

```js
import { System } from '@gexemu/runtime';
import { OPFSFilesystem } from '@gexemu/runtime/fs';
import { Wisp } from '@gexemu/runtime/net/wisp';
import { HostWindows } from '@gexemu/runtime/windows/host';

const sys = new System({ xcache: true });              // translated code kept across loads
const fs = new OPFSFilesystem(sys, { name: 'debian' }); // the root, kept in the page's OPFS
fs.addTar('/gex-trees/debian.tar.zst', { lazy: '/gex-trees/debian-lazy.json' });
sys.network = new Wisp('wss://wisp.example/');        // connections out (see Network)
const hw = new HostWindows(sys, { width: innerWidth, height: innerHeight });
await sys.boot();
```

**Keep the root.** An `OPFSFilesystem` keeps what programs change
(installed packages, a home directory) in its store and lays it back over
the same tars at the next load. The tars must be the same at every load, so
give a new tree a new `name`. What is written reaches the store within the
filesystem's `writeback` (5 s), at a program's `fsync` or `sync`, and when
the tab is hidden or left: a desktop need not call `sys.sync()`, though it
may before it unloads. `/tmp`, `/run` and mounts are never kept.

**Terminals.** A terminal is a `spawn` with a `pty`; its `data` events go to the
desktop's terminal widget and its keys come back with `pty.write`. Resize
with `pty.resize(cols, rows)`. Nothing is inherited, so give every program its
`env`.

```js
const env = { PATH: '/usr/local/bin:/usr/bin:/bin', HOME: '/root', TERM: 'xterm-256color', LANG: 'C.UTF-8', ...hw.env };
const sh = sys.spawn(['bash', '-l'], { pty: { cols: 80, rows: 24 }, env, cwd: '/root' });
sh.pty.addEventListener('data', (e) => term.write(e.data));
term.onData((d) => sh.pty.write(d));
sh.exited.then(() => closeTerminalWindow());
```

**Windows.** With `HostWindows`, each guest window, Wayland or X11, arrives
as a `window` event carrying a `GuestWindow`. Its `canvas` goes into one of
the desktop's frames:
- `decorations: 'host'`: the desktop draws the title bar (`title`, and the
  `title` event).
- `'client'`: the window draws its own (a Win32 window under Wine). Place it
  bare, where its `x` and `y` say when it is `placed`.
- Popups and subsurfaces are never framed: put them at `x`, `y` over their
  `parent`.

Then wire it to the desktop's window manager:
- **Moves:** after each move the frame makes, call `place(x, y)`.
- **Resizes:** forward them with `resize(w, h)` or `setStates(...)`, and size
  the frame on `resized`.
- **Close:** the frame's close button calls `close()`. Drop the frame on
  `closed`.
- **Client-side frames:** `request` events (`move`, `resize`, `maximize`,
  `minimize`, ...) are the window asking the desktop's manager to act.
- **Minimize:** hiding a frame is the desktop's own; the program is not
  told, and its window keeps its size and contents for when it comes back.
- **Taskbar:** `appId` names the application (`appid` when it changes) and
  `pid` its program (`process.windows()` goes the other way), so a taskbar
  groups windows and matches them to the launcher's entries.

An X11 program needs nothing more: `DISPLAY` is in `hw.env`, and the
runtime starts its X server for the first client. When the desktop's work
area changes, call `hw.setOutputSize(width, height)`.

**The desktop's files.** `fs.mount('/mnt/desktop', provider)` shows a
directory of the desktop's own filesystem to every program, while it runs
(Mounts says what a provider implements). A mount is the desktop's to keep:
the filesystem's store leaves it out.

**A launcher.** The tree's applications are its desktop entries:
`fs.readdir('/usr/share/applications')`, each `.desktop` file read with
`fs.readFile(path, { encoding: 'utf-8' })` for its `Name`, `Exec` and `Icon`
(an icon theme's file under `/usr/share/icons`). An entry's `Exec` runs as a
`spawn` of its words, its field codes (`%f`, `%U`) left out or filled, and its
windows' `appId` is the entry's name without `.desktop` (a Wayland program)
or its `StartupWMClass` (an X one).

**Calls out of the guest.** To let a program open a URL in the desktop's
browser, or show a notification, register a host call. A guest script then
uses it as `xdg-open` would.

```js
sys.hostCalls.register('open-url', ({ url }) => { openBrowserWindow(url); return true; });
// guest: /usr/local/bin/xdg-open
//   exec 3<>/dev/gex-host; printf 'open-url\0{"url":"%s"}' "$1" >&3; cat <&3 > /dev/null
```

**Sound, GL, input.**
- Sound: make a `WebAudio` in the first click's handler. Browsers hold audio
  until a gesture.
- GL: a `MesaGl` made before boot gives programs OpenGL and Vulkan in their
  windows. Mesa is fetched when the first program opens GL.
- Input: `PointerLock`, `Clipboard` and `Gamepads` give programs a locked
  pointer, the browser's clipboard and controllers.

**Windows programs.** Wine is a tree too: its install (lazy: it is large)
and a prefix, laid beside the Linux tree. A Windows program is then
`wine 'C:\path\prog.exe'`, with `WINEPREFIX` naming the prefix. The
first Wine program starts Wine's server and the services Wine runs; the
next ones reuse them while they last. Its windows come as any other, framed by the program
itself (`decorations: 'client'`) and `placed`. The filesystem's store keeps
the prefix's changes with the rest of the root. Each 64-bit Wine process takes a
memory of about 2 GiB (most of it reserved, not touched), so a desktop
running several Windows programs at once should expect the tab's address
space to reach well past 8 GiB.

**Start time.** `xcache: true` keeps translated code across loads in the
origin's private file system, so a second start of the same programs
translates little. `units` points a system at compiled function-tier units
for a tree's hot images (`gexc cgen-image`), which start at full speed with
no translation.

**What a desktop can see.** `state` and the system's events (`running`,
`console`, `stopped`) for a status area, and `describe()` for a task manager's
processes and memory.

## The page

- Programs run in Workers on a shared memory, so the page must be
  cross-origin isolated: `Cross-Origin-Opener-Policy: same-origin` and
  `Cross-Origin-Embedder-Policy: require-corp`.
- Install the package and import it; nothing else to do. The modules name
  each file the runtime fetches (`new URL('./…', import.meta.url)`) and each
  Worker (`new Worker(new URL('./…', import.meta.url), { type: 'module' })`),
  so the page's bundler takes them into its build as files, with no
  settings (vite and webpack 5 are tested), and vite 8's dev server serves
  them from `node_modules`. vite 7's dev server does not point a
  pre-bundled package's URLs back at it: there, the package fails to start
  unless it is left out of pre-bundling
  (`optimizeDeps: { exclude: ['@gexemu/runtime'] }`); vite 7's build works as
  it is. Every Worker is one file with no imports, so a bundler's default
  classic-script Workers build too. No file becomes a `data:` URL.
- Served without a bundler, from any directory, the modules find their
  files beside themselves. `new System({ baseUrl })` takes them from a
  directory of the page's instead, laid out as gex's own site has them
  (`gex/…`).
- One system per page: a second `new System()` throws.
- A system runs wasm32 programs (built with gexcc) and the programs of one
  translated architecture, x86-64 or aarch64: the one `new System({ arch })`
  names, else the tree's programs' at boot (`/bin/sh` first); aarch64 when
  the tree has none. Naming it lets the kernel and runtime load while the
  page's files are got ready, so a page that knows its programs' says. The
  kernel is built for one, so this is fixed for the system's life: a program
  of the other one fails to start (`ENOEXEC`).
- Pid 1 is the kernel's own unless `boot` is given an init: no
  program runs as it. Each program `spawn` starts is its child, in a
  session of its own; a program whose parent ends is given to it, and its
  exit is collected in the kernel, as an init's would be. `getppid()` of
  either is 1, `/proc/1` names `init`, and a signal sent to pid 1 is
  ignored.
- With an init (`await sys.boot(['/sbin/init'], opts)`, the arguments
  `spawn` takes), that program is pid 1 and owns the processes, as on
  Linux: it collects its children and the orphans given to it, and `spawn`
  throws. Without `env`, its environment is Linux's for an init, `HOME=/`
  and `TERM=linux`. The page reaches programs
  through what the init starts: terminals, windows, `/dev/gex-host`.
  `sys.init` is its `Process`. When it ends, the system stops, and
  `state.init` says how it ended.
- A page makes one `System` (a second throws), for now: the page runtime
  keeps its kernel and switches in page-wide state.
- Nothing runs at boot but the init. Without one, a tree's own start (its
  daemons) is the page's first `spawn`s, made once `boot()` resolves.

## System

| Member | Does |
| --- | --- |
| `new System({ baseUrl, units, xcache, arch, debug })` | A system, not started; a page makes one. `arch`: `'x86'` (x86-64) or `'aarch64'`, as `describe()` names them: the translated architecture its programs are built for (by default read from the tree at boot); given, the runtime loads while the page lays out its files. `units`: the URL of a directory of compiled function-tier units (a unit cache, as `gexc cgen-image` writes it); each image mapped at the base its unit was built for runs from it, the rest are translated as they run. `{ url, require: true }` makes a program image without one fail to start. `xcache: true` (or a name, for a cache kept apart from other names' on the same page) keeps the translated code across loads, in the origin's private file system (on the Node test host, the directory GEX_XCACHE_DIR names): the next load of the same runtime with the same switches starts with it, and code whose file changed since is translated again. `debug` holds the runtime's switches for debugging (`{ strace: 1 }`, `{ console: true }` to log the runtime's console); they are not a stable API. The runtime is quiet by default: the browser's console hears its errors and rare events, not a line per program started or library loaded; `debug: { log: '1' }` brings those back, `{ log: 'debug' }` adds the runtime's narration. |
| `boot(argv, opts)` | Starts the kernel and lays the filesystem's sources into its root. Parts that work before boot (the filesystem) must be made before it. With arguments, `spawn`'s, the program that is pid 1 (see above); without, pid 1 is the kernel's. |
| `spawn(argv, opts)` | Starts a program and returns its `Process` at once; throws when the system has an init. `opts`: `pty: { cols, rows, encoding }` for a terminal, else three pipes; `env` (`{K: V}` or `['K=V']`; nothing is inherited), `cwd`, `uid`, `gid`, `detached` (no terminal or pipes: output goes nowhere). The program is found on `env.PATH`, or in `/usr/local/bin:/usr/bin:/bin` and the `sbin`s. |
| `run(argv, opts)` | `spawn`, then `{ code, signal, stdout, stderr }` once it ends. `input` is written to its stdin; `encoding` (default `'utf-8'`, `null` for bytes). It reads both outputs as they come, so a program never waits on it. The output is what was written before the program ended: a daemon it started that still holds its pipes (a persistent server) does not keep `run` waiting. |
| `init` | The init's `Process`, or null without one. |
| `state` | `{ phase, error, console, init }`; phase is `new`, `booting`, `running`, `stopped` or `failed`. |
| Events | On the system itself (`sys.addEventListener`): `running` when it boots, `stopped` when it ends, `console` (`text`) for what the runtime says, `crash` (`pid`, `name`, `reason`, `code`, `signal`) when a program crashes, any program of the system's: one a crash signal ends (`SIGSEGV`, `SIGILL`, `SIGABRT`, ...: `signal` its number, `code` 128 plus it), or a Windows program that Wine ends for an exception nothing handled (`name` its `.exe`, `reason` the exception, `code` its code, such as `0xc0000005`, `signal` 0). The program has ended by then: its windows are gone. |
| `describe()` | `{ phase, arch, processes, pids, threads, memory }`. |
| `sync()` | What the programs wrote made durable in the filesystem's store, as `sync(2)` (nothing to do for a `MemoryFilesystem`). Rejects with the store's error (`code` `EIO`, `ENOSPC`). |
| `shutdown()` | Ends every program and the kernel, after a `sync()`. |

## Process

| Member | Does |
| --- | --- |
| `pid` | Its pid once it runs. |
| `started` | Resolves with the pid; rejects when the program could not start (`no such program`, with `errno`). |
| `exited` | Resolves with `{ code, signal }`: a program killed by a signal has `signal` set (`'SIGTERM'`) and `code` 128 plus its number. An `exit` event too. Rejects as `started` does when the program never ran. |
| `pty` | With a terminal: `write(data)` (a string or bytes, any size), `resize(cols, rows)` (the program hears `SIGWINCH`), and `data` events (bytes, or strings with an `encoding`). |
| `stdin`, `stdout`, `stderr` | Without a terminal: a `WritableStream` and two `ReadableStream`s of bytes. The outputs are read from the program's pipes as the page reads the streams: a program whose output nobody reads waits when its pipe is full (64 KiB), as on Linux and in Node, and may never end. Read both, cancel one you do not want (the program then gets `EPIPE`, or `SIGPIPE`), or spawn it `detached`. A terminal's output is always read, into `data` events. An init (`boot(argv)`) has pipes too, unless `detached`: read them. |
| `kill(signal)` | `'SIGTERM'` by default; a name or a number. |

## Filesystem

One per system. `Filesystem` is abstract: it holds the tree, a cache of
its files' contents in the system's memory, and the kernel's side of both.
Where the contents are kept is the subclass the page makes:

| Class | Keeps file contents |
| --- | --- |
| `new MemoryFilesystem(sys, { max })` | In the system's memory. `max` (bytes, optional): writes past it fail with `ENOSPC`; without it, as much as the memory holds. |
| `new OPFSFilesystem(sys, { name, cache, writeback, exclude })` | In the page's origin private file system (OPFS), in a store named `name` (letters, digits, `.`, `_`, `-`), kept across loads (Persistence below). Memory holds a cache of `cache` MiB (default 256, 16 to 4096) and the files' contents live in the store, so the tree may be far larger than memory. A written block reaches the store within `writeback` ms (default 5000, 100 to 600000) or at a sync. Small files (16 KiB or less) stay in memory, and so do the files `addTar` and `addFiles` lay until a program writes one (boot writes nothing to the store); lazy files are fetched as before. A full store (the origin's quota) fails writes with `ENOSPC`, a failing one with `EIO`. |
| A subclass of the page's own | Wherever its methods keep them (below). |

`new Filesystem(sys)` throws. A `MemoryFilesystem` keeps nothing across
loads; a store does (Persistence below).

With a store, the cache is write-back, as a disk's page cache is: what a
program writes is in memory first. `sync(2)`, `syncfs`, `fsync`,
`fdatasync` and `sync_file_range` write it out and flush the store before
they return (the page's `sys.sync()` too, and `shutdown()`), a block is
written out once it has waited `writeback` ms, and a page being hidden or
left syncs. A write-out the store fails is `EIO` (`ENOSPC` when the store
is full) to the call that needed it (a sync, or a write that needed room in
the cache), and the data stays in memory, tried again. A `MAP_SHARED` mapping's
pages reach the file at `msync` and `munmap`; `sync` does not reach into
other programs' mappings.

A single read or write of 1 MiB or more moves the whole blocks it covers
straight between the store and the program's buffer, without keeping them
in the cache (a write is in the store when it returns, but is durable only
at a sync, as any other), so a big copy runs near the store's own speed and
leaves the cache as it was. Blocks a program uses again are kept over ones
read or written once: a file read through once, however large, does not
push them out.

A store of the page's own is a subclass that implements the store's
methods: one array of bytes, which the kernel reads and writes by offset in
blocks of 64 KiB as its cache fills and empties, one call at a time. Its
constructor takes `cache`, `writeback` and `exclude` as `OPFSFilesystem`'s
does. Its bytes are the root as a load before kept it: a store that is to
start over empties itself in `open()`. It needs the default
build: in the single-thread one (`@gexemu/runtime-single`) the boot rejects it,
and the built-ins are the choice.

| Method | Does |
| --- | --- |
| `open()` | Optional: make the store ready, before boot. |
| `read(offset, length)` | A `Uint8Array` of `length` bytes at `offset`; shorter past the end, which reads as zeros. |
| `write(offset, bytes)` | Writes them, growing the store. |
| `size()`, `truncate(length)` | The store's length; set it (growing with zeros). |
| `flush()` | Makes what was written durable. |

Each is async. An error with `code: 'ENOSPC'` is a full store to the
program that wrote; any other error is `EIO`.

```js
import { System } from '@gexemu/runtime';
import { Filesystem } from '@gexemu/runtime/fs';

const PIECE = 1 << 20;
// The files' contents in 1 MiB pieces the page keeps (in a Map here).
class PieceFilesystem extends Filesystem {
  #pieces = new Map();
  #size = 0;
  #each(offset, length, fn) {
    for (let at = 0; at < length;) {
      const p = offset + at, n = Math.min(length - at, PIECE - (p % PIECE));
      fn(Math.floor(p / PIECE), p % PIECE, at, n);
      at += n;
    }
  }
  async read(offset, length) {
    const out = new Uint8Array(Math.max(0, Math.min(length, this.#size - offset)));
    this.#each(offset, out.length, (i, o, at, n) => {
      const piece = this.#pieces.get(i);
      if (piece) out.set(piece.subarray(o, o + n), at);
    });
    return out;
  }
  async write(offset, bytes) {
    this.#each(offset, bytes.length, (i, o, at, n) => {
      if (!this.#pieces.has(i)) this.#pieces.set(i, new Uint8Array(PIECE));
      this.#pieces.get(i).set(bytes.subarray(at, at + n), o);
    });
    this.#size = Math.max(this.#size, offset + bytes.length);
  }
  async size() { return this.#size; }
  async truncate(length) {
    for (const i of this.#pieces.keys()) if (i * PIECE >= length) this.#pieces.delete(i);
    this.#pieces.get(Math.floor(length / PIECE))?.fill(0, length % PIECE);
    this.#size = length;
  }
  async flush() {}
}

const sys = new System();
const fs = new PieceFilesystem(sys, { cache: 16 });
fs.addTar('/trees/base.tar.zst');
await sys.boot();
```

Before boot, it is the tars and files a page adds, laid one
over another in the order added (a later one wins). After boot, a tar or
files are written into the running tree.

| Member | Does |
| --- | --- |
| `addTar(src, { at, lazy })` | A tar (zstd or not) as bytes, a `Blob`, a `Response`, a stream or a URL, laid at `at` (default `/`). `lazy`: files fetched when first read, `[[path, size, url, mode, mtime], ...]` (mode optional, 0644 without; mtime in seconds, optional) or the URL of such a list as JSON. A list (or the list at a URL) may be an ejected site's `lazy.json` instead: `{ base, files: [[path, size, url, mtime, mode], ...] }`, each url relative to `base`, which resolves against the list's URL (an ejected site's `/project/build/web/` names the list's own directory, wherever the site is served) or, for the object, the page, and a url `BUNDLE#OFFSET` a file in the site's rest bundle (zstd frames, indexed by `BUNDLE.json`), which must be added before boot. Their URLs must be on the page's own origin and answer range requests. What a system reads of them is kept in the page's Cache Storage (`gex-runtime-lazy-v1`), by URL and version (size and mtime), for the next visit, where the page has shared memory and Cache Storage. |
| `addFiles({ path: data }, { mode })` | Single files: bytes, a `Blob`, a URL, or a string (the file's text); `mode` default 0644. |
| `readFile(path, { encoding })` | Bytes, or text with an `encoding`. |
| `writeFile(path, data, { mode, append, exclusive })` | Writes the file, made if missing. |
| `stat(path, { follow })` | `{ type, mode, size, ino, mtime, nlink }`; `follow: false` does not follow a last symlink. |
| `readdir(path)` | `[{ name, type }]`. |
| `mkdir(path, { recursive, mode })`, `rm(path, { recursive, force })`, `rename(from, to)`, `symlink(target, path)`, `readlink(path)`, `chmod(path, mode)` | As their commands. |
| `watch(path, fn)` | `fn({ type, path })` for each change to a directory's entries or to a file, by a program or the page: `create`, `change` (a write finished), `attrib`, `delete`, `move-out` and `move-in` (one rename's halves share a `cookie`), and `gone` when `path` itself went. Returns a function that stops it. |

Errors carry `code` (`'ENOENT'`, ...) and `errno`.

### Mounts

`fs.mount(path, provider)` serves a directory from the page's own filesystem
(a desktop's files, OPFS, IndexedDB, the network), before boot or while the
system runs: every program sees it at once, those already running too, over
whatever the tree has there (back at `unmount`). It returns a `Mount`
(`path`, `ready`, `unmount()`, an `unmounted` event); `fs.unmount(path)` and
`fs.mounts()` too. A program's call on it waits for the provider's promise,
in its own thread only.

The provider's methods take paths inside it (`/`, `/a/b`), are async, and
throw errors with a `code` (`'ENOENT'`, `'EEXIST'`, ...) or an `errno`:

| Method | Does |
| --- | --- |
| `stat(path)` | `{ type: 'file' \| 'directory' \| 'symlink', size, mode, mtime (ms or a Date), ino }` (`mode`, `ino` optional) |
| `readdir(path)` | `[{ name, type }]`, or names (each is then stat'ed) |
| `open(path, { read, write, create, truncate, exclusive, append, mode })` | A handle, any value, for the three below |
| `read(handle, offset, length)`, `write(handle, offset, bytes)`, `close(handle)` | Bytes; a count |
| `mkdir(path, mode)`, `unlink(path)`, `rename(from, to)` | |
| `lstat`, `rmdir`, `truncate(path, length, handle)`, `chmod(path, mode)`, `utimes(path, mtime)`, `readlink(path)`, `symlink(target, path)` | Optional. Without `chmod` and `utimes` those calls succeed and change nothing; without the others they fail |

A mount's own symlinks are not followed on the way through it, and a rename
or a hard link between it and the rest of the tree is `EXDEV` (`mv` copies).

## Persistence

A store (an `OPFSFilesystem`, or a subclass of the page's own) keeps the
root across page loads. At boot, what it kept is laid over the sources before
the first program. From then on it keeps what the programs change: files,
directories, symlinks, modes, extended attributes, hard links, and
deletions of the sources' paths. It never keeps `/tmp`, `/proc`, `/sys`,
`/dev`, `/run`, a mount (its provider keeps its own) or the filesystem's
`exclude`. A lazy file nobody wrote is never fetched for it.

What is kept is written as a generation, beside the files' blocks in the
store, when one of these happens:
- a program calls `sync`, `syncfs`, `fsync` or `fdatasync`;
- the page calls `sys.sync()` or `shutdown()`;
- the page is hidden or left;
- a change has waited the filesystem's `writeback` ms.

Each generation is checksummed, and the one before stays whole until the
next is written. If a boot finds the newest torn or failing its checks (a
tab killed in the middle of one, a damaged store), it boots the one before
and says so on the console. A file's bytes are not journalled: after such
a boot, a file reads what was last written into the blocks it had.

One page holds an `OPFSFilesystem`'s store at a time: a Web Lock named for
it, taken at boot and let go at `shutdown()` or when the page is left. A
system booted on a store another tab of the site holds fails at once with
`EBUSY`. (The eject library's `start()` asks first, and lets the page
offer to run without keeping files: its `onStoreBusy` option.)

```js
import { System } from '@gexemu/runtime';
import { OPFSFilesystem } from '@gexemu/runtime/fs';

const sys = new System();
const fs = new OPFSFilesystem(sys, { name: 'my-tree' });  // kept as 'my-tree'
fs.addTar('/trees/my-tree.tar.zst');                      // the same at every load
await sys.boot();
```

| Member | Does |
| --- | --- |
| `exclude` (constructor) | More absolute paths never kept, beside the ones above. |
| `sys.sync()` | Writes a generation now. |
| `OPFSFilesystem.erase(name)`, `fs.erase()` | Forget what the store keeps: the next boot is the sources alone. Before boot, or once the system has stopped. |

A tree's sources must be the same at every load: what is kept is laid over
them, path by path. A new tree gets a new store `name`. Nothing kept by the
`IdbPersist` of earlier versions is read: those IndexedDB databases are left
as they were, and a page may delete them.

## Network

The guest's connections out go to the page, which carries them over whatever
transport it has. A network is any object with a `connect(host, port)`
method, for TCP, and optionally `datagram(host, port)`, for UDP; the page
gives it to the system as `sys.network`, before or after boot. There is one
at a time: setting another sends the next connections there (open ones stay
where they are), and `null` takes it away. With none, a program's
connection fails as unreachable, and so does UDP with a network that has no
`datagram`.

```js
// One WebSocket per connection, through the page's own relay.
class RelayNetwork {
  connect(host, port) {
    const ws = new WebSocket(`wss://relay.example/tcp?host=${encodeURIComponent(host)}&port=${port}`);
    ws.binaryType = 'arraybuffer';
    return new Promise((resolve, reject) => {
      ws.onerror = () => reject(new Error(`${host}:${port} refused`));
      ws.onopen = () => resolve({
        readable: new ReadableStream({
          start(c) {
            ws.onmessage = (e) => c.enqueue(new Uint8Array(e.data));
            ws.onclose = () => c.close();
          },
          cancel: () => ws.close(),
        }),
        writable: new WritableStream({
          write: (bytes) => ws.send(bytes),
          close: () => ws.close(),
        }),
      });
    });
  }
}
sys.network = new RelayNetwork();
```

| Member | Does |
| --- | --- |
| `connect(host, port)` | A program's TCP connection out. `host` is what the program connects to: an address, or a name, which is not looked up first (a program's lookup of a name succeeds, and its connection brings the name here). Returns or resolves a `Duplex`; a throw or a rejection refuses the connection. |
| `datagram(host, port)` | Optional. A program's UDP to `host:port`, a flow: the first datagram a socket sends there asks for it, and the socket's later ones there go down it. Returns or resolves a `Duplex` whose chunks are datagrams, each written one the program sent and each read one back, which the program receives as from `host:port`. A throw or a rejection drops what was sent. Sending with `sendto` and with `connect` and `send` both work. |
| `Duplex` | `{ readable, writable }`: a ReadableStream of the far end's bytes and a WritableStream of the program's. The page ending `readable` is the far end closing, which the program reads as the end. The program closing its socket closes `writable` and cancels `readable`. |
| `sys.network` | The network, or `null`. |

DNS is not the network's: the guest's lookups are answered by the system
itself, and a name the program connects to (or sends a datagram to) reaches
`connect` or `datagram` as that name. Not in it: a program that listens is
reached from the page with `connectGuest` (below), not through the network;
a program's `shutdown(SHUT_WR)` does not reach the page; UDP from a far
address the program did not send to first.

**Wisp.** `@gexemu/runtime/net/wisp` is a network over a
[Wisp](https://github.com/MercuryWorkshop/wisp-protocol) proxy, with
MercuryWorkshop's wisp-js client: every connection on one WebSocket, dialled
again when it drops. It is an implementation of the interface above, and
only a page that imports it carries wisp-js.

```js
import { Wisp } from '@gexemu/runtime/net/wisp';
sys.network = new Wisp('wss://wisp.example/');
```

| Member | Does |
| --- | --- |
| `new Wisp(url)` | The proxy at `url` (`ws:` or `wss:`), dialled at the first connection. |
| `connect(host, port)` | As above. |
| `datagram(host, port)` | As above, a Wisp UDP stream (a proxy without Wisp's UDP extension refuses it). |
| `close()` | Closes the WebSocket; the next connection dials again. |
| Events | `open` and `closed` as the WebSocket comes and goes. |

Into the guest, `sys.net.connectGuest(port)` connects to what a program
listens on at `127.0.0.1:port`. It resolves `{ readable, writable, close() }`
and a `close` event; closing the writable is a half close, which the
program reads as the end. It rejects with `ECONNREFUSED` when nothing
listens.

## Unix sockets

The page can be either end of a program's AF_UNIX stream socket.
`sys.listen(path)` listens on a name, as a program's `bind` and `listen`
would: each program's `connect` to it is a connection the page takes.
`sys.connect(path)` connects to a name a program listens on. Each
connection is the `{ readable, writable, close() }` that `connectGuest`
gives.

```js
const srv = sys.listen('/run/app.sock');       // before or after boot
(async () => {
  for await (const conn of srv) conn.readable.pipeTo(conn.writable); // an echo server
})();
// guest: perl -MIO::Socket::UNIX -e '$s = IO::Socket::UNIX->new(Peer => "/run/app.sock") or die $!; print $s "hello\n"; print scalar <$s>'
//        -> "hello"
```

```js
// guest, a server that upper-cases what it reads:
//   perl -MIO::Socket::UNIX -e '$l = IO::Socket::UNIX->new(Local => "/run/myd.sock", Listen => 1) or die $!; $c = $l->accept; print $c uc while <$c>'
const conn = await sys.connect('/run/myd.sock');
const w = conn.writable.getWriter();
await w.write('hello\n');
await w.close();                                 // the program reads the end
const reply = await new Response(conn.readable).text(); // "HELLO\n", then the program closes
```

| Member | Does |
| --- | --- |
| `sys.listen(path)` | A listener on `path`: an absolute path, or an abstract name with a leading `\0`. The socket file appears at `path` (`ls -l` shows `s`) once it is bound: at once when the system runs, else as it boots, before the init or any program starts. A socket file there that nothing listens on is replaced; any other file there, or a name something listens on, is `EADDRINUSE`, and a missing directory `ENOENT`. |
| `listener.ready` | Resolves once the name is bound; rejects (the error's `code`) when it could not be. |
| `listener.accept()` | The next connection, when a program makes one; `null` once closed. A connection the page has not taken waits in the kernel, as on Linux: the program's `connect` has returned. |
| `for await (const conn of listener)` | Each connection as it comes. Leaving the loop closes the listener. |
| `listener.close()` | Stops listening: the name is let go, and a program's `connect` is then refused (`ECONNREFUSED`; the socket file stays, as on Linux, until something unlinks it). Connections not taken yet are closed; ones taken stay open. |
| `sys.connect(path)` | A connection to what a program listens on at `path`. Rejects with `ECONNREFUSED` when nothing listens there and `ENOENT` when there is no file there either, as a program's `connect` fails. The system must be running. |
| `conn.readable` | What the program sends. Read only as the page reads it: a page that stops reading leaves the program's writes blocked once the socket's buffer is full (256 KiB), as a slow reader on Linux does. It ends when the program closes its socket or shuts down its writing (`shutdown(SHUT_WR)`), or exits. |
| `conn.writable` | To the program. Each write resolves when the program's socket has taken it, so the page's writes wait on a program that does not read. `close()` is a half close (the program's `read` returns 0; it can still write back); `abort()` closes both ways. A write after the program has closed rejects with `EPIPE`. |
| `conn.close()` | Both ways at once: the program reads the end, and its writes fail with `EPIPE`. |

The page's ends are the kernel's own sockets, the ones programs' sockets
are, so a program waits on them with `poll`, `select` or `epoll` as on any
other (async runtimes, libuv, Python's asyncio), and many connections may be
open at once. The connecting side's credentials (`SO_PEERCRED`) for a page
end are pid 0, uid 0, gid 0. Not in it: datagram sockets (a program's
`SOCK_DGRAM` connect or send to a name the page listens on fails), and
`SOCK_SEQPACKET` keeps no message boundaries (a program's seqpacket
connect works, as a stream); a program's death is the end of the page's
`readable`, not an error, as for a Linux unix socket.

## Host calls

The page's functions a guest program calls. A program opens
`/dev/gex-host`, writes `NAME`, a NUL and the JSON value (in one write or
several), then reads from the same descriptor to its end: a status byte, 0
when the function returned and 1 when it threw, then what it returned, as
JSON, or the error's text. The first read makes the call; each open is one
call, and many may be open at once.

```js
const off = sys.hostCalls.register('notify', ({ title }) => { showToast(title); return true; });
// guest: exec 3<>/dev/gex-host; printf 'notify\0{"title":"built"}' >&3; cat <&3
//        -> "\0true"
```

| Member | Does |
| --- | --- |
| `register(name, fn)` | `fn(value)` may return a promise. Returns a function that unregisters it. |
| `unregister(name)`, `names()` | |

## Audio

What programs play on `/dev/dsp` (and ALSA, Pulse or Wine over it) comes
out of the page through an AudioWorklet once there is a `WebAudio`; until
then it is played to nobody at the wall clock's pace. Browsers start audio
only after a user gesture, so make it in a gesture's handler (or the first
pointer or key event after it resumes the context). One per system, made
any time.

```js
import { WebAudio } from '@gexemu/runtime/audio';
button.onclick = () => new WebAudio(sys);
```

| Member | Does |
| --- | --- |
| `new WebAudio(sys, { latency })` | `latency`: the AudioContext's hint ('interactive', 'playback', or seconds). |
| `ready`, `context` | Resolves once the player is set up; its AudioContext. |
| `stats` | The player's last report (about twice a second): `backlog` (ms queued, `avg`, `min`, `max`), `base` and `output` (the context's latencies, ms), `rms`, `underruns`. |
| `close()` | Stop playing. |
| Events | `running`, once the context plays. |

## GL

The guest's OpenGL, OpenGL ES, Vulkan and (with `d3d`) Direct3D run on the
host's Mesa over WebGPU and draw into the windows' own surfaces. A
`MesaGl` is made before boot, beside a windows class. Mesa is about 20 MB
before compression. It is fetched when a program first opens its GL, or
at `start()`, not at boot.

```js
import { MesaGl } from '@gexemu/runtime/gl';
import { HostWindows } from '@gexemu/runtime/windows/host';
const hw = new HostWindows(sys);  // the windows GL draws into
const gl = new MesaGl(sys);       // before boot
```

| Member | Does |
| --- | --- |
| `new MesaGl(sys, { eager, d3d, source })` | `eager`: start Mesa at boot. `d3d`: the Direct3D device too (a Mesa built with it). `source(name)`: Mesa's files from the page (`gex-wgpu-replay.js`, `.wasm`, `.deferred.wasm`) as bytes, or null to fetch the package's own (or `baseUrl`'s). |
| `start()` | Start Mesa now; resolves its description. A program's first GL call shares the same start. |
| Events | `started` (`description`), `failed` (`error`), for starts through `start()`. |

## Input

The input a browser holds back until the page asks for it, from
`@gexemu/runtime/input`. Each is made any time, one per system.

`new PointerLock(sys)`: a click on a window whose program asks for a locked
pointer takes the browser's pointer lock. The program asks through Wayland's
pointer constraints; Xwayland asks for an X program that grabs the pointer
and hides it, as games do. The mouse's movement then reaches the program as
relative motion, until Escape, `release()`, or the program letting go.
`locked` says whether it holds; the events are `lock` and `unlock`. A program
that asks for nothing never locks the pointer. A program that asks just
after the click that started it (an X game grabbing on a click) gets the
lock while that click still counts as the user's gesture. Xwayland locks
the pointer for an X program only when the program hides the cursor with
XFixes (`XFixesHideCursor`). A grab with a blank cursor image only confines
the pointer, so it gets no relative motion.

`new Clipboard(sys, { sync })`: text copied and pasted between the page and
the guest's programs. A Wayland program's selection, or an X program's
CLIPBOARD (the X window manager owns and converts it for them), comes out as
a `copy` event with `text`; `setText(text)` offers text to every program,
and an X program that pastes gets it as UTF8_STRING or STRING. One program's
copy reaches another as well. `text` is the last text either side set. With
`sync` (the default) a copy also goes onto the browser's clipboard, and the
browser's text comes in on a paste into the page and, where the browser
allows reading it, when the page gets the focus. Text only, up to 1 MiB
(256 KiB to and from X programs).

`new Gamepads(sys, { source })`: the browser's game controllers as the
guest's `/dev/input/event0` to `event3`, Linux evdev joysticks. SDL reads
them, and Wine reads them through SDL. A controller the browser maps to the
standard layout appears as an Xbox 360 pad, with the ids, name, buttons and
axes Linux's xpad driver gives one, so SDL's mapping database knows it. Any
other controller appears as a joystick under its own name and USB ids. A
node exists while its controller is connected. A program without udev finds
controllers by scanning `/dev/input`; SDL scans again when the directory's
mtime moves. Add `env` (`SDL_JOYSTICK_DISABLE_UDEV=1`) to `spawn`'s
environment so SDL scans even where libudev is installed. `pads` lists
`{ slot, id, mapping }`; the events are `connect` and `disconnect`, with
`slot`. A browser shows a page a controller only after one of its buttons
is pressed. `source` replaces `navigator.getGamepads` (a test's made-up
controllers). Rumble is accepted and dropped.

`new WineControllers(sys, gamepads, { wine, env })`: controllers for
Windows programs. A 64-bit Wine install with controllers (SDL beside its
loader) does not start Wine's controller bus, winebus, at boot, so a session
with no controller pays nothing for it. This starts it in the Wine session
(`wine sc start winebus`, with `wine` the loader's path and `env` the
session's environment) the first time `gamepads` has a controller, or at
once if it has one, once per system. Games then see the controller arrive
through XInput, DirectInput and winmm. `state` is `idle`,
`starting` or `started`; the `start` event has `code`, 0 when the driver
started. As with SDL on Linux, a program that opens the controller sees
its state from the next change (a press, a stick moved).

## Windows

One windows part per system, made before boot. A program finds the
compositor through its environment: add the part's `env` to `spawn`'s.

X11 programs' windows come as any other: with windows, display `:0` is the
runtime's own X server (Xwayland, rootless, gex's window manager framing
nothing), started when the first X client connects, which waits for it. The
tree needs only the clients' libraries.

### HostWindows (`@gexemu/runtime/windows/host`)

Each guest window is handed to the page to frame and place, as a desktop's
window manager does.

```js
import { HostWindows } from '@gexemu/runtime/windows/host';
const hw = new HostWindows(sys, { width: 1280, height: 720 });
hw.addEventListener('window', ({ window: w }) => {
  if (w.kind === 'toplevel' && w.decorations === 'host') frame(w);   // a title bar, w.canvas inside
  else desktop.append(w.canvas);                                      // popups: at w.x, w.y over w.parent
});
await sys.boot();
sys.spawn(['foot'], { env: { PATH: '/usr/bin:/bin', ...hw.env } });
```

| Member | Does |
| --- | --- |
| `new HostWindows(sys, { width, height, scale })` | The output programs see as their screen, in CSS pixels (default the viewport) and device pixels per CSS pixel (default `devicePixelRatio`, rounded). |
| `setOutputSize(width, height, scale)` | When the page's desktop changes size. |
| `env` | `{ XDG_RUNTIME_DIR, WAYLAND_DISPLAY, DISPLAY }` for `spawn`. |
| `windows()`, `focused` | Every window; the one with the keyboard. |
| `keyEvent(e, pressed)` | A `keydown`/`keyup` heard elsewhere (the frame), for the window with the keyboard. |
| `window` event | `{ window }`: a new `GuestWindow`. A `cursor` event (`css`) when the client's cursor changes; it is also set on every window's canvas. |

`process.windows()` lists a process's windows.

**GuestWindow**

| Member | Does |
| --- | --- |
| `canvas` | Its pixels, CSS-sized to the window. Focusable: keys go to the window whose canvas has the page's focus; pointer and wheel input are its own. |
| `kind` | `'toplevel'`, `'popup'` (a menu, a tooltip: never framed, drawn above its parent and outside its frame) or `'subsurface'`. |
| `parent`, `title`, `width`, `height` | `title` whole (up to 256 bytes of UTF-8). |
| `pid` | The program's pid: an X window's as the program says (`_NET_WM_PID`, which Qt, GTK and most toolkits set), else its X server's. `process.windows()` uses it. |
| `decorations` | `'host'` (frame it), `'client'` (it draws its own: a Win32 window under Wine) or `'none'`. |
| `placed`, `x`, `y` | A popup's or subsurface's place relative to its parent's canvas; a placed window's (`placed`, Wine's) on the output, where its client moves and stacks it (`restack`). |
| `place(x, y)` | Where the page put it on the output; call after each move (X windows' menus read it). |
| `resize(w, h)`, `setStates({ maximized, fullscreen, resizing }, w, h)` | Ask for a size, with states. |
| `focus()`, `close()` | The keyboard; a polite close. |
| `appId` | What application it is, or `null` when the program does not say: a Wayland window's app id (by convention its desktop entry's name, `org.kde.kate`), an X window's `WM_CLASS` class (what a desktop entry's `StartupWMClass` names, `XTerm`). |
| Events | `title`, `appid`, `resized`, `position`, `restack` (`below`), `request` (`kind`: `'move'`, `'resize'` with `edges`, `'maximize'`, `'unmaximize'`, `'minimize'`, `'fullscreen'`, `'unfullscreen'`: from the client's own frame), `closed`. |

### SingleCanvas (`@gexemu/runtime/windows/single`)

`new SingleCanvas(sys, { container })`: the windows drawn straight into one
positioned element, unframed, the first at its corner, popups over their
parents, a window its client places (Wine's) where it says. The element's size
is the output's; call `resized()` after changing it. For a page that is one
program (a game, a Win32 program that draws its own frames). `env`,
`describe()` and a `change` event as windows come and go.

### CompositorWindows (`@gexemu/runtime/windows/compositor`)

`new CompositorWindows(sys, { canvas, scale, x11 })`: the whole desktop in one
canvas, drawn by gex's scene compositor on WebGPU in a Worker: frames, title
bars, stacking, the cursor. `x11` is `'rootless'` (the default: each X window
is a desktop window, placed by the runtime's window manager) or `'rootful'`
(one X screen fills the canvas and the session runs its own window manager,
as a KDE or Xfce session does). `env`, `resize(width, height)` (device
pixels), `describe()`.

`process.windows()` is for HostWindows; the other two keep their windows to
themselves.

## Building

In gex's tree, `page/runtime/build.sh DIR` builds the package into `DIR`: its modules, their
Workers, and beside them, each named by its content, the shim, kernel and
seed of each guest architecture the tree has built (`hosts/browser/build.sh`, `GEX_ARCH=x86 hosts/browser/build.sh`),
with x86's the memory64 shim and seed its processes with `GEX_MEM64` run on
(`GEX_ARCH=x86 GEX_MEM64=1 hosts/browser/build.sh`, needed when x86 is built), the
GL's WebGPU replay and the X server (`ports/xwayland/build.sh`), stripped
of names and DWARF (`GEX_KEEP_NAMES=1` keeps them). The
package is for browser pages only. Node runs the runtime from its sources
as a test host (`test/run.mjs --host node`), not as a host the API
supports.

## Tests

The runtime's tests, in gex's tree, are its test runner's `core`, `windows`, `gl`,
`image` and `wine` groups (test/README.md): `node test/run.mjs --group core,windows,gl,image,wine`.
`--package DIR` runs the browser's against a built package.
