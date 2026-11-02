// PointerLock through the scene compositor (pointerlock.test.js).
import { pointerLock } from './pointerlock.test.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 120, html: `<style>html, body { margin: 0; }</style>
<canvas id="out" width="800" height="600"></canvas>` };

export default (t) => pointerLock(t, true);
