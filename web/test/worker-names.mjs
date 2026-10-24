// Every Worker the page starts is named.
//
//   node web/test/worker-names.mjs
//
// A debugger lists each Worker by the name it was given (the CDP target's
// title, DevTools' thread list), so a per-Worker profile says which Worker
// it is; unnamed, all of them read alike. Checked: each `new Worker(` the
// page's sources make passes the name below (a guest Worker its serial,
// `gex-w<N>`), every site is in the table, and the host records each
// Worker it hands a process or a thread (`workerRan`), the map from a guest
// Worker's name to the pids and tids it ran.
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const WANT = {
  'web/src/compositor.js': ["'compositor'"],
  'web/src/app.js': ["'fill'", "'lazy-cache'"],
  'web/src/fs-opened.js': ["'fs-opened'"],
  'web/src/persist.js': ["'persist'"],
  'web/src/desktop.js': ["'overlay'"],
  'web/src/runtime.js': ['name', 'name', "'system'", "'xwayland'"],
  'shims/browser/js/tty.mjs': ["'tty'"],
};
// Node's worker_threads (the terminal's and the shim's own tests): no page.
const NODE = new Set(['run-node.mjs', 'check-wait-queues.mjs', 'check-wl-input-wake.mjs']);

const sources = [
  ...readdirSync(join(ROOT, 'web/src')).filter((f) => f.endsWith('.js')).map((f) => `web/src/${f}`),
  ...readdirSync(join(ROOT, 'shims/browser/js')).filter((f) => f.endsWith('.mjs') && !NODE.has(f)).map((f) => `shims/browser/js/${f}`),
];
const seen = {};
for (const f of sources) {
  const s = readFileSync(join(ROOT, f), 'utf8');
  for (const m of s.matchAll(/new Worker\(/g)) {
    // The call's own text, to its closing parenthesis.
    let depth = 0;
    let i = m.index + 'new Worker'.length;
    const from = i;
    for (; i < s.length; i++) {
      if (s[i] === '(') depth++;
      else if (s[i] === ')' && --depth === 0) break;
    }
    const call = s.slice(from, i + 1);
    // One a comment names is not a call.
    const line = s.slice(s.lastIndexOf('\n', m.index) + 1, m.index);
    if (/^\s*(\/\/|\/?\*)/.test(line)) continue;
    const name = call.match(/\bname(?::\s*('[^']+'))?\s*[,}]/);
    assert.ok(name, `${f}: a Worker without a name: new Worker${call.slice(0, 80)}`);
    (seen[f] ??= []).push(name[1] ?? 'name');
  }
}
assert.deepEqual(seen, WANT);

const rt = readFileSync(join(ROOT, 'web/src/runtime.js'), 'utf8');
assert.equal((rt.match(/`gex-w\$\{\+\+serial\}`/g) ?? []).length, 2, 'both guest Worker paths name by the serial');
const host = readFileSync(join(ROOT, 'shims/browser/js/host.mjs'), 'utf8');
assert.ok(/this\.#workers\.set\(pid, w\);\s*workerRan\(w, pid\);/.test(host), 'a process Worker is recorded');
assert.ok(/this\.#workers\.set\(key, w2\);\s*workerRan\(w2, key\);/.test(host), 'a thread Worker is recorded');
console.log('ok worker-names');
