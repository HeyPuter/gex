// A test in a Node process of its own (run.mjs `--host node`), forked with
// an IPC channel: node test/lib/node.mjs FILE (the rest from the message the
// runner sends first: `{ arch, phase, state, res, work }`). It sends
// `{ line }` as it goes, `{ reload: state }` to be run again as the next
// phase, and `{ done, result, skip, res }` at the end.

import { pathToFileURL } from 'node:url';
import { join } from 'node:path';
import { makeT, runTest } from './t.js';

const file = process.argv[2];
const PUBLIC = pathToFileURL(join(import.meta.dirname, '../../web/public/')).href;

// A lazy file's URL is a host path here (shims/browser/js/lazy-fetch.mjs).
process.env.GEX_NODE_LAZY = '1';

process.once('message', async ({ arch, phase, state, res, work, gpu, timed }) => {
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
