// A test in a Node process of its own (run.mjs `--host node`), forked with
// an IPC channel: node test/lib/node.mjs FILE (the rest from the message the
// runner sends first: `{ arch, phase, state, res, work }`). It sends
// `{ line }` as it goes, `{ reload: state }` to be run again as the next
// phase, and `{ done, result, skip, res }` at the end.

import { pathToFileURL } from 'node:url';
import { join, relative } from 'node:path';
import { readFileSync, rmSync } from 'node:fs';

let logsAsked = 0;
import { makeT, runTest } from './t.js';

const file = process.argv[2];
const PUBLIC = pathToFileURL(join(import.meta.dirname, '../../page/public/')).href;

// A lazy file's URL is a host path here (hosts/browser/js/lazy-fetch.mjs).
process.env.GEX_NODE_LAZY = '1';

process.once('message', async ({ arch, phase, state, res, work, gpu, timed }) => {
  // The translation cache across loads (`new System({ xcache })`): one of
  // the job's own, empty at its first phase, as a fresh page's storage is.
  process.env.GEX_XCACHE_DIR = join(work, 'xcache', `${relative(join(import.meta.dirname, '..'), file).replace(/\.test\.js$/, '').replaceAll('/', '-')}-${arch}`);
  if (phase === 1) rmSync(process.env.GEX_XCACHE_DIR, { recursive: true, force: true });
  const t = makeT({ host: 'node', arch, gpu, timed, phase, state, res, emit: (line) => process.send({ line }) });
  const browserOnly = (what) => () => { throw new Error(`${what}: browser host only`); };
  Object.assign(t, {
    baseUrl: PUBLIC,
    debug: (more = {}) => ({ ...(arch ? { arch } : {}), ...more }),
    work: (name) => pathToFileURL(join(work, name)).href,
    /** The work directory itself (lib/guest.mjs keeps its builds there). */
    workDir: work,
    hits: browserOnly('t.hits'),
    reload(next = null) {
      process.send({ reload: next, res });
      return new Promise(() => {});
    },
    /** The lines the runtime logged this phase that match `re` (the runner
     *  keeps this process's output). Everything written before the call:
     *  a mark goes down stdout and stderr first, and the runner answers once
     *  it has read both (its pipes are not ordered with this message). */
    logs: (re = /./) => new Promise((resolve) => {
      const mark = `\u0001gex-logs ${++logsAsked}`;
      const on = (m) => {
        if (!m?.logs) return;
        process.off('message', on);
        resolve(m.logs);
      };
      process.on('message', on);
      process.stdout.write(`${mark}\n`);
      process.stderr.write(`${mark}\n`);
      process.send({ logs: { re: re.source, flags: re.flags.replace('g', ''), mark } });
    }),
    /** This phase's process's peak RSS in KiB (its Workers are its threads). */
    peakRss: async () => process.resourceUsage().maxRSS,
    /** This process's memory from /proc, KiB: `{ data, rss, hwm }`. */
    procStatus: async () => {
      const st = readFileSync('/proc/self/status', 'utf8');
      const kb = (k) => Number(new RegExp(`${k}:\\s+(\\d+) kB`).exec(st)?.[1] ?? 0);
      return { data: kb('VmData'), rss: kb('VmRSS'), hwm: kb('VmHWM') };
    },
    input: browserOnly('t.input'),
    click: browserOnly('t.click'),
  });
  let r;
  try {
    const mod = await import(pathToFileURL(file).href);
    r = await runTest(mod.default, t, res);
  } catch (e) {
    res.fail.push('load');
    process.send({ line: `FAIL ${e?.stack ?? e}` });
    r = { result: 'FAIL', skip: null };
  }
  process.send({ done: true, ...r, res }, () => process.exit(0));
});
