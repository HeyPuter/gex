# @gex/runtime

Linux programs in a web page: a kernel, the programs it runs, and their files.
A page builds a `System`, gives it a filesystem, boots it, and runs programs in
it with a terminal or with pipes. Windows, persistence, network, sound and GL
are parts of their own, each a class that takes the system as its first
argument.

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
- One system per page: a second `new System()` throws.
- A system runs wasm32 programs (built with gexcc) and the programs of one
  translated architecture, x86-64 or aarch64, which it takes from the tree's
  programs at boot (`/bin/sh` first); aarch64 when the tree has none. The
  kernel is built for one, so this is fixed for the system's life: a program
  of the other one fails to start (`ENOEXEC`).
- At boot, before the first program, pid 1 runs the tree's `/etc/gex/boot`
  when it has one (as root; it must end, and what it starts goes on): where
  a tree starts its daemons.

## System

| Member | Does |
| --- | --- |
| `new System({ baseUrl, debug })` | A system, not started; a page makes one. `debug` holds the runtime's switches for debugging (`{ strace: 1 }`, `{ console: true }` to log pid 1's console); they are not a stable API. |
| `boot()` | Starts the kernel and pid 1. Parts that work before boot (the filesystem) must be made before it. |
| `spawn(argv, opts)` | Starts a program and returns its `Process` at once. `opts`: `pty: { cols, rows, encoding }` for a terminal, else three pipes; `env` (`{K: V}` or `['K=V']`; nothing is inherited), `cwd`, `uid`, `gid`, `detached` (no terminal or pipes: output goes nowhere). The program is found on `env.PATH`, or in `/usr/local/bin:/usr/bin:/bin` and the `sbin`s. |
| `run(argv, opts)` | `spawn`, then `{ code, signal, stdout, stderr }` once it ends. `input` is written to its stdin; `encoding` (default `'utf-8'`, `null` for bytes). |
| `state` | `{ phase, error, console }`; phase is `new`, `booting`, `running`, `stopped` or `failed`. A `running` event when it boots, `console` events for what pid 1 and the runtime say. |
| `describe()` | `{ phase, arch, processes, pids, threads, memory }`. |
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
| `addFiles({ path: data }, { mode })` | Single files: bytes, a `Blob`, a URL, or a string (the file's text); `mode` default 0644. |
| `readFile(path, { encoding })` | Bytes, or text with an `encoding`. |
| `writeFile(path, data, { mode, append, exclusive })` | Writes the file, made if missing. |
| `stat(path, { follow })` | `{ type, mode, size, ino, mtime, nlink }`; `follow: false` does not follow a last symlink. |
| `readdir(path)` | `[{ name, type }]`. |
| `mkdir(path, { recursive, mode })`, `rm(path, { recursive, force })`, `rename(from, to)`, `symlink(target, path)`, `readlink(path)`, `chmod(path, mode)` | As their commands. |
| `watch(path, fn)` | `fn({ type, path })` for each change to a directory's entries or to a file, by a program or the page: `create`, `change` (a write finished), `attrib`, `delete`, `move-out` and `move-in` (one rename's halves share a `cookie`), and `gone` when `path` itself went. Returns a function that stops it. |

Errors carry `code` (`'ENOENT'`, ...) and `errno`.

## Windows

One windows part per system, made before boot. A program finds the
compositor through its environment: add the part's `env` to `spawn`'s.

X11 programs' windows come as any other: with windows, display `:0` is the
runtime's own X server (Xwayland, rootless, gex's window manager framing
nothing), started when the first X client connects, which waits for it. The
tree needs only the clients' libraries.

### HostWindows (`@gex/runtime/windows/host`)

Each guest window is handed to the page to frame and place, as a desktop's
window manager does.

```js
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
| `parent`, `pid`, `title`, `width`, `height` | |
| `decorations` | `'host'` (frame it), `'client'` (it draws its own: a Win32 window under Wine) or `'none'`. |
| `placed`, `x`, `y` | A popup's or subsurface's place relative to its parent's canvas; a placed window's (`placed`, Wine's) on the output, where its client moves and stacks it (`restack`). |
| `place(x, y)` | Where the page put it on the output; call after each move (X windows' menus read it). |
| `resize(w, h)`, `setStates({ maximized, fullscreen, resizing }, w, h)` | Ask for a size, with states. |
| `focus()`, `close()` | The keyboard; a polite close. |
| `appId` | `null` for now. |
| Events | `title`, `resized`, `position`, `restack` (`below`), `request` (`kind`: `'move'`, `'resize'` with `edges`, `'maximize'`, `'unmaximize'`, `'fullscreen'`, `'unfullscreen'`: from the client's own frame), `closed`. |

### SingleCanvas (`@gex/runtime/windows/single`)

`new SingleCanvas(sys, { container })`: the windows drawn straight into one
positioned element, unframed, the first at its corner, popups over their
parents, a window its client places (Wine's) where it says. The element's size
is the output's; call `resized()` after changing it. For a page that is one
program (a game, a Win32 program that draws its own frames). `env`,
`describe()` and a `change` event as windows come and go.

### CompositorWindows (`@gex/runtime/windows/compositor`)

`new CompositorWindows(sys, { canvas, scale })`: the whole desktop in one
canvas, drawn by gex's scene compositor on WebGPU in a Worker: frames, title
bars, stacking, the cursor. `env`, `resize(width, height)` (device pixels),
`describe()`.

`process.windows()` is for HostWindows; the other two keep their windows to
themselves.

## Building

`web/runtime/build.sh DIR` builds the package into `DIR`: its modules, and
under `gex/` the shim, kernel and seed of each guest architecture the tree has
built (`shims/browser/build.sh`, `GEX_ARCH=x86 shims/browser/build.sh`) and pid
1, `gex-init` (`web/runtime/init`, built with `gexcc`).

## Tests

`node web/runtime/test/run.mjs` runs each test page here in headless Chromium
against a vite dev server. `BUSYBOX` names a wasm32 busybox, which with its
applets and `test/probe` is the test tree.
