# gex's tests

One runner for every test, on two hosts: a headless Chromium page (`browser`)
and a Node process (`node`), both through the runtime API (web/runtime).

```sh
node test/run.mjs                          # every test, every host it allows, a64
node test/run.mjs --group core --host both --arch a64,x86
node test/run.mjs basic windows/x11        # by name, or group/name
node test/run.mjs --list                   # the jobs, without running them
```

| Option | |
|---|---|
| `--group G,...` | Only these groups (the directories here). |
| `--host browser\|node\|both` | Where to run (default `both`): each test runs on the hosts its `meta.hosts` allows. |
| `--arch a64,x86` | The translated architectures (default `a64`); each test runs on each. |
| `--jobs N` | Tests at once (default 4): pages of one Chromium, Node processes. |
| `--allow-skip` | A skipped test does not fail the run. |
| `--verbose` | Every check line, the guest's console and Chromium's. |
| `--work DIR` | The work directory (default a fresh temporary one, deleted after). Its images are kept and reused while their inputs are unchanged. |
| `--port N` | vite's port (default 5411); Chromium's debugging port is N+1. |

Env: `BUSYBOX` (a wasm32 busybox, for the `busybox` image), `GEXCC` (gexcc,
default on PATH), `GEX_TEST_SLIM` (a directory with a Debian image,
`debian-slim.tar.zst`, and its apt repository, `repo/`), `CHROME` (default
/usr/bin/chromium), `GEX_TEST_WORK` and `GEX_TEST_PORT` (the defaults of
`--work` and `--port`).

The browser host is vite on web/ (cross-origin isolated, the runtime from
source) and one headless Chromium on WebGPU. Each test gets a page in a
browser context of its own. The node host forks a Node process per test (and
per phase).

## Output

One JSON line per test, host and arch on stdout, as each ends:

```json
{"test":"core/basic","host":"browser","arch":"a64","result":"ok","ms":4900,"checks":23,"fail":[],"skip":null,"times":{}}
```

| Field | |
|---|---|
| `test` | `group/name`. |
| `host` | `browser` or `node`. |
| `arch` | `a64` or `x86`. |
| `result` | `ok`, `FAIL` or `skip`. |
| `ms` | Wall time, every phase. |
| `checks` | Checks made. |
| `fail` | What each failed check said; `threw` when the test threw, `load` when it would not load, or the runner's reason (a timeout, an exit). |
| `skip` | Why it was skipped (an image missing, no WebGPU adapter), else null. |
| `times` | `{name: ms}` from `t.time`. |

What a person reads goes to stderr: one line per test, and each FAIL line.
The exit status is 0 only when every test is ok; a skip counts as not ok
unless `--allow-skip` is given.

## A test

`test/GROUP/NAME.test.js`, a module with no HTML:

```js
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 120 };

export default async function (t) {
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  new Filesystem(sys).addTar(t.work('bb.tar.zst'));
  await sys.boot();
  t.eq('echo', (await sys.run(['sh', '-c', 'echo hi'], { env: { PATH: '/bin' } })).stdout, 'hi\n');
  await sys.shutdown();
}
```

`meta` is an object literal (the runner reads it without running the test):

| Field | |
|---|---|
| `hosts` | Where it can run (default `['browser']`). |
| `images` | The images it needs (fixtures/images.mjs): made in the work directory before any test runs. Without one, the test is skipped. |
| `timeout` | Seconds for the whole test, every phase (default 120). |
| `html` | The page's elements (browser), added to its body before the test runs. |

A page or process runs one test, so each makes its own System.

### `t`

| | |
|---|---|
| `t.check(what, cond, detail)`, `t.eq(what, got, want)` | A check: a line, `ok` or `FAIL`. A failed check does not stop the test. |
| `t.say(text)` | A line of output. |
| `t.until(fn, what, ms)`, `t.timeout(promise, ms, what)` | Wait for a truthy `fn()`, or for a promise, else throw. |
| `t.lines(process, stream)` | A stream's text as lines, as they come (a process's stdout by default). |
| `t.time(name, fn)` | `fn`'s ms, in the result's `times`. |
| `t.skip(why)` | End the test as skipped. |
| `t.host`, `t.arch` | The host, and the translated arch (`aarch64`, `x86`). |
| `t.baseUrl`, `t.debug(more)` | A System's `baseUrl` and `debug` on this host. |
| `t.work(name)` | A work directory file as a URL (http on the browser host, file on node). |
| `t.reload(state)` | Run the test again in a fresh page or process, as phase `t.phase + 1` with `t.state = state`; the checks so far are kept. |
| `t.phase`, `t.state` | This run's phase (1 first), and what `reload` was given. |
| `t.input(...events)`, `t.click(x, y)` | Real input over CDP: `{ method: 'Input.dispatchMouseEvent', ... }` and the like (browser). |
| `t.hits(name)` | How many times the page asked for work file `name` (browser). |

## Images

fixtures/images.mjs:

- `busybox`: `bb.tar.zst`. This is BUSYBOX and its applets, the probe programs (fixtures/probe/*.c, built with gexcc), and an `/etc/xattr` with an extended attribute. It also holds `lazy.bin`, a lazy file's bytes.
- `slim`: GEX_TEST_SLIM's image and repository, with `slim-repo.json` listing the repository.
- `package`: `pkg/`, the runtime package (web/runtime/build.sh).
