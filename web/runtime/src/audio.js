// `WebAudio`: what the guest's programs play on `/dev/dsp` (and ALSA, Pulse
// or Wine over it), out of the page's speakers through an AudioWorklet, the
// page runtime's player (web/src/audio.js). One per system, made any time;
// browsers start audio only after a user gesture, so a page makes it in
// one's handler, or the first pointer or key event after resumes it. Until
// there is one, what programs write is played to nobody, at the wall clock's
// pace.

import { internals } from './internal.js';

export class WebAudio extends EventTarget {
  #player = null;
  #closed = false;
  #ready;

  /** `latency`: the AudioContext's latency hint ('interactive', the
   *  default, 'playback', or seconds). Events: `running` once it plays. */
  constructor(sys, { latency = 'interactive' } = {}) {
    super();
    const i = internals(sys, 'WebAudio');
    i.attach('audio', this);
    // Made in the gesture's handler: the context too, before any await, so
    // the browser lets it start.
    const ctx = typeof AudioContext === 'function' ? tryContext(latency) : null;
    this.#ready = new Promise((resolve, reject) => {
      i.afterBoot(async (s) => {
        try {
          const at = (await i.kernel().r('k_audio_ring')) >>> 0;
          if (this.#closed) return resolve(this);
          const { playAudio } = await import('../../src/audio.js');
          this.#player = playAudio({ memory: s.kernel.memory, at, latency, context: ctx });
          if (!this.#player) throw new Error('WebAudio: this browser has no AudioWorklet over shared memory');
          await this.#player.ready;
          const c = this.#player.ctx;
          const said = () => { if (c.state === 'running') this.dispatchEvent(new Event('running')); };
          c.addEventListener('statechange', said);
          said();
          resolve(this);
        } catch (e) {
          ctx?.close().catch(() => {});
          reject(e);
        }
      });
    });
    this.#ready.catch(() => {});
  }

  /** Resolves once the player is set up (it may still wait for a gesture). */
  get ready() {
    return this.#ready;
  }

  /** The AudioContext, once `ready`. */
  get context() {
    return this.#player?.ctx ?? null;
  }

  /** The player's last report: `{ backlog, base, output, ... }` (ms). */
  get stats() {
    return this.#player?.last ?? null;
  }

  /** Stop playing; programs' audio goes back to nobody. */
  async close() {
    this.#closed = true;
    await this.#player?.close();
    this.#player = null;
  }
}

function tryContext(latency) {
  try {
    return new AudioContext({ latencyHint: latency });
  } catch {
    try { return new AudioContext({ latencyHint: 'interactive' }); } catch { return null; }
  }
}
