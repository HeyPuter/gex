// Units served compressed (`?cgenzst=1`: KEY/c.wasm.zst): each fetched,
// decoded and compiled here, off the page's main thread, and the Module
// handed back (a Module crosses postMessage by reference).
//
// In: { id, url, zstd: the decoder module's URL }
// Out: { id, mod } | { id, error }

let decoder = null;

self.onmessage = async ({ data: { id, url, zstd } }) => {
  try {
    decoder ??= import(/* @vite-ignore */ zstd).then(async ({ ZSTDDecoder }) => {
      const d = new ZSTDDecoder();
      await d.init();
      return d;
    });
    const d = await decoder;
    let r = await fetch(url);
    // A unit with no compressed copy: its plain one.
    const plain = r.status === 404 && url.endsWith('.zst');
    if (plain) r = await fetch(url.slice(0, -4));
    if (!r.ok) throw new Error(`${url}: ${r.status}`);
    const got = new Uint8Array(await r.arrayBuffer());
    const bytes = plain ? got : d.decode(got);
    self.postMessage({ id, mod: await WebAssembly.compile(bytes) });
  } catch (e) {
    self.postMessage({ id, error: String(e?.message ?? e) });
  }
};
