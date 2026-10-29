# The app library

An ejected program's web build is a library, `app.mjs`, beside its files.
`app.js` here builds into it for a 32-bit Wine build, and
`crates/gex-eject/src/app.mjs` is the one for a Linux program. The page
that imports it is yours:

```html
<script type="module">
  import { start } from './app.mjs';
  const app = await start({ container: document.body });
  const status = await app.exited;
</script>
```

The page must be cross-origin isolated (`Cross-Origin-Opener-Policy:
same-origin`, `Cross-Origin-Embedder-Policy: require-corp`). The build's
`serve.mjs` and `_headers` send both. The library never reads the page's
URL. Its files come from `baseUrl`, so the site can live under any path.

## `start(options)`

`start()` resolves once the program runs. It rejects in these cases, with
a message that names the cause:

- an option it does not know (with the nearest name for a typo);
- a value of the wrong kind;
- a piece the build left out (`gl: true` on a build without OpenGL names
  the `user/eject.json` setting);
- no cross-origin isolation;
- no WebGPU, for a build that draws with it;
- a download that failed.

Every option is optional. A default comes from the build
(`user/eject.json` `"start"`, over what the recording saw).

| Option | What |
| --- | --- |
| `container` | the element the desktop fills; default `document.body` |
| `canvas` | or the canvas to draw into |
| `size` | `'fit'` (the container's size), `{w, h}` or `'WxH'` |
| `args` | the program's arguments, an array of strings |
| `env` | more environment, `['K=V', ...]` or `{K: V}`; a name given replaces the build's |
| `cwd`, `exe` | the working directory; the program, as a path in the bundle |
| `uid` | the guest's user and group id (default the build's, else 1000) |
| `mtimes` | `true`: the guest sees the bundle's file times |
| `persist` | `false`, or `{dir, db}`: a guest directory kept in IndexedDB |
| `wisp` | a Wisp proxy's URL, `ws://` or `wss://`: the guest's TCP and DNS go over it; offline without it |
| `pageHosts` | `{host: path}`: hosts the page answers itself over plain HTTP, from files under `path` |
| `strace` | `true` (every process) or a pid: narrate system calls |
| `trapstack` | `true`: a trap's wasm frames |
| `debug` | other runtime switches, `{name: value}`, as gex's test page's `?name=value` |
| `compositor`, `gl`, `translation`, `hostx`, `mem64` | `true` asks for a runtime piece (an error if the build left it out). `compositor: false` draws windows straight into the page. `hostx: 'rootful'` is one fullscreen X screen |
| `glLazy` | start Mesa at the first GL request, not at boot |
| `ready` | `'frame'` (default): the start-up is over at the first frame. `'manual'`: when the page calls `ready()` |
| `onLog(text)` | the program's terminal output |
| `onExit(status)` | called when it exits |
| `onProgress(p)` | the start-up's progress, for a loading screen (`crates/gex-eject/README.md`, "The loading screen") |
| `baseUrl` | where `app.mjs`'s files are; default beside `app.mjs` |
| `launch` | the build's launch file, relative to `baseUrl` |

The handle `start()` resolves with:

| | What |
| --- | --- |
| `exited` | a promise of the exit status; it rejects if the run fails |
| `kill()` | stop now; `exited` resolves 137 |
| `resize(size)` | the desktop's new size, as `size` above; returns `{w, h}` |
| `ready()` | with `ready: 'manual'`: the start-up is over |
| `describe()` | the desktop's windows, their titles and pixels |
| `state` | `phase`, `out` (the output so far), `status`, `error`, `progress` |
| `drop(data, opts)` | files and folders dropped on the page, copied into the guest (below) |
| `dragOver(x, y)` | the window at a point of the page, `{title, ...}` or `null` |
| `dragLeave()` | the drag left the desktop |

### Dropped files

```js
desk.addEventListener('dragover', (e) => e.preventDefault());
desk.addEventListener('drop', (e) => {
  e.preventDefault();
  // In the handler, before any await: the browser empties
  // e.dataTransfer once the event is over.
  app.drop(e.dataTransfer, {
    x: e.clientX, y: e.clientY,
    fallback: '/home/user/.wine/drive_c/users/user/Desktop',
    onProgress: ({ done, total, filesDone, files }) => { /* ... */ },
  }).then((r) => console.log(`copied to ${r.paths.join(', ')}`));
});
```

`drop` copies the tree, with its folders, into a guest directory (`dir`,
or a new `/tmp/drops/N`). A name the drop has twice is numbered too. The copy runs in a Worker, so the
page does not stop while it runs. `data` is a DataTransfer, its `items`, a
FileList or an array of Files.

| Option | What |
| --- | --- |
| `x`, `y` | where it was dropped, the event's `clientX` and `clientY` |
| `dir` | the guest directory to copy into: absolute, without `..`, not `/`, made if missing. Nothing there is written over: a name it has already gets ` (2)`, ` (3)`, ... |
| `fallback` | a guest directory the copy moves into when no window takes the drop. A name already there gets ` (2)`, ` (3)`, ... |
| `signal` | an AbortSignal: stops the copy (or the wait for a kernel stack, at most 30 s) and removes what it wrote |
| `onProgress(p)` | `{done, total}` bytes and `{filesDone, files}` so far, and the file `name` being copied |

It resolves with:

- `path`: the directory it made;
- `files` and `bytes`: how many files it copied, and their total size;
- `ms`: how long the copy took;
- `window`: the window at `x, y`, or `null`;
- `accepted`: whether that window took the drop;
- `paths`: where the top-level files and folders are now.

`accepted` is always `false` for now, because the drop is not yet handed to
the window. So with `fallback`, the files always end up in that directory.

Files are 0644, or 0755 when one starts like a program (`\x7fELF` or
`#!`); folders are 0755; the owner is the guest's user. The single-thread
build has no `drop`. A drop with no files or folders in it rejects.

## A Linux program's build

Its `start()` takes `container` (output goes to a `<pre>` there unless
`onLog`), `args`, `env`, `cwd`, `strace`, `translation`, `onLog(text,
stream)`, `onExit`, `onProgress` and `baseUrl`. It accepts the Wine
build's other options and ignores them, so one page can start either kind.
It has no desktop: `resize()` and `describe()` return `null`. The run is on
the page's thread, so `kill()` stops waiting for it and hearing it.

## Not the API

The following can change without notice:

- the `build` option and `fromLaunch`, for gex's test page;
- `state`'s other fields;
- the `globalThis` names the library sets for its runtime and test drivers
  (`__nt`, `__gex*`, `GEX_*`);
- everything under `shims/browser/js`, the `Gex` class included. `start()`
  is the way in.
