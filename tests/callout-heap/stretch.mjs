// Preloaded into every thread (node --import): the most promises a thread
// made in one stretch with no async callback run between -- a nested guest
// loop (a call-out) that never yields. Each one's reactions wait in the
// microtask queue until the loop ends, so they are what such a loop keeps.
import { createHook } from 'node:async_hooks';
import { threadId } from 'node:worker_threads';
let cur = 0, max = 0;
createHook({
  init(id, type) {
    if (type !== 'PROMISE' || ++cur <= max) return;
    max = cur;
    if ((max & (max - 1)) === 0 && max >= 16) process._rawDebug(`[stretch] thread ${threadId}: ${max} promises`);
  },
  before() { cur = 0; },
}).enable();
