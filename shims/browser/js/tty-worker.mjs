// tty.mjs's Worker: the page's terminal on the global kernel, off the page's
// thread. The kernel's locks wait (`memory.atomic.wait32`), which a Worker
// may and the page may not. Messages in: { id, op: 'open', args: [spec] }
// once, then { id, op, args } for each `PageTty` method; out: { id, value }
// or { id, error }, one per message, in order.
import { PageTty } from './tty.mjs';

let tty = null;
const OPS = new Set(['key', 'input', 'drainEcho', 'ready', 'eof', 'lflag', 'winsize', 'resize', 'fg', 'signal']);
let queue = Promise.resolve();
self.onmessage = (e) => {
  const { id, op, args } = e.data;
  queue = queue.then(async () => {
    try {
      if (op === 'open') {
        tty = await PageTty.openHere(args[0]);
        self.postMessage({ id, value: !!tty });
      } else if (tty && OPS.has(op)) {
        self.postMessage({ id, value: tty[op](...args) });
      } else {
        self.postMessage({ id, error: `the terminal Worker has no ${op}${tty ? '' : ' (not open)'}` });
      }
    } catch (err) {
      self.postMessage({ id, error: String(err?.stack ?? err) });
    }
  });
};
