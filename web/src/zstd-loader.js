// The page's zstd decoder (zstddec's streaming build), there only while
// something decodes.
//
// Its wasm memory grows to hold every stream open at once -- the boot's
// pieces have windows of up to 23 MiB and four decode together -- and a wasm
// memory never shrinks, so a decoder kept for the page's life kept 45 to 92
// MiB of RSS after boot for nothing. Each user takes it (`get`) and gives it
// back (`release`); once none has for `idleMs`, the instance and its memory
// are let go, and the next `get` makes a new one.
//
// Imported under a URL of its own (a fragment): the module keeps its
// instance in module state, and another user of the same URL in this realm
// would share it -- and lose it at the dispose.

export function zstdLoader(src, { idleMs = 2000 } = {}) {
  let p = null;
  let dec = null;
  let users = 0;
  let timer = null;
  const url = `${src}${src.includes('#') ? '' : '#page-decoder'}`;
  const get = async () => {
    users++;
    clearTimeout(timer);
    timer = null;
    try {
      return await (p ??= (async () => {
        const { ZSTDDecoder } = await import(/* @vite-ignore */ url);
        const d = new ZSTDDecoder();
        await d.init();
        return (dec = d);
      })());
    } catch (e) {
      p = null;
      users--;
      throw e;
    }
  };
  get.release = () => {
    if (--users > 0) return;
    users = 0;
    clearTimeout(timer);
    timer = setTimeout(() => {
      timer = null;
      if (users || !dec) return;
      // An older zstddec has no dispose: kept, as before.
      dec.dispose?.();
      dec = null;
      p = null;
    }, idleMs);
  };
  /** Whether a decoder is held now (for a test). */
  get.held = () => !!dec;
  return get;
}
