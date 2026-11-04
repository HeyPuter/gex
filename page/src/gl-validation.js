// WebGPU validation errors from the GL device: kept and logged, never fatal.
//
// A validation error refuses one call -- a writeTexture, a submit -- and the
// device goes on working. Treating the first as the device's death would
// turn one bad upload into a dead session: `ok` false for good, GL windows
// given no more frames and every later batch refused. wined3d's 1x1
// multisample dummy texture is enough to trigger one. Device loss is still fatal; that is
// mesa-gl.js's business, not this.
//
// A game that raises one error a frame must not flood the console either: the
// first few are logged, then one line every `every` ms says how many more.

export class ValidationLog {
  constructor({ first = 5, every = 10000, now = () => performance.now(), log = console.error } = {}) {
    this.count = 0;
    /** The first error's message, or null: the one that explains the rest. */
    this.first = null;
    this.last = null;
    this.#firstN = first;
    this.#every = every;
    this.#now = now;
    this.#log = log;
  }

  #firstN;
  #every;
  #now;
  #log;
  #lastLogAt = -Infinity;
  #quiet = 0;

  /** One error; returns whether it was logged. */
  note(message) {
    this.count++;
    this.first ??= message;
    this.last = message;
    if (this.count <= this.#firstN) {
      this.#log('[mesa-gl] WebGPU validation:', message);
      if (this.count === this.#firstN) this.#lastLogAt = this.#now();
      return true;
    }
    this.#quiet++;
    const t = this.#now();
    if (t - this.#lastLogAt < this.#every) return false;
    this.#log(`[mesa-gl] WebGPU validation: ${this.#quiet} more (${this.count} in all), the last:`, message);
    this.#quiet = 0;
    this.#lastLogAt = t;
    return true;
  }

  /** For glState(): empty when there were none. */
  summary() {
    return this.count ? `validation=${this.count} (first: ${this.first}) ` : '';
  }
}
