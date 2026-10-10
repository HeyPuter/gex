// The global kernel's `env.lazy_fetch` (shims/kernel: `TmpFs::add_lazy`):
// a lazy file's bytes, fetched when a guest reads them. A synchronous ranged
// request, made from the Worker whose guest is reading (the read blocks
// there anyway); it fills `len` bytes at `dst` in the kernel's memory.
// Where there is no XMLHttpRequest (Node), lazy files cannot be read.

/** `lazy_fetch(url, urlLen, offset, dst, len)` against the kernel memory `memory`. */
export function lazyFetch(memory) {
  return (url, urlLen, offset, dst, len) => {
    if (typeof XMLHttpRequest !== 'function') return -1;
    url >>>= 0; urlLen >>>= 0; dst >>>= 0; len >>>= 0;
    const from = Number(offset);
    const href = new TextDecoder().decode(new Uint8Array(memory.buffer, url, urlLen).slice());
    let last = 'no response';
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        const x = new XMLHttpRequest();
        x.open('GET', href, false);
        x.responseType = 'arraybuffer';
        x.setRequestHeader('Range', `bytes=${from}-${from + len - 1}`);
        // The HTTP cache can answer a range from a partial entry badly (a
        // later range of a file whose first chunk it holds); the guest keeps
        // its own copy of each chunk, so the cache buys nothing here.
        x.setRequestHeader('Cache-Control', 'no-store');
        x.send();
        const got = new Uint8Array(x.response ?? new ArrayBuffer(0));
        last = `status ${x.status}, ${got.length} bytes`;
        // 206 is the range; a server without ranges sends the whole file (200).
        const at = x.status === 206 ? 0 : x.status === 200 ? from : -1;
        if (at < 0 || got.length < at + len) continue;
        new Uint8Array(memory.buffer, dst, len).set(got.subarray(at, at + len));
        return len;
      } catch (e) {
        last = String(e);
      }
    }
    console.error(`[lazy] ${href} @${from}+${len}: failed (${last})`);
    return -1;
  };
}
