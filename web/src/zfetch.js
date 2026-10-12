// The demo build's runtime .wasm files, fetched as .wasm.zst.
//
// `web/build-demo.sh` ships every runtime module (shim, kernel, seed, Xwayland,
// its compositor bridge, Mesa) only as `NAME.wasm.zst`: about a tenth of the
// stripped size for the shim. Rather than teach each loader about it, the
// demo build wraps `fetch` once per realm -- the page and each Worker that
// fetches a module -- so a same-origin request for `X.wasm` asks for
// `X.wasm.zst` and answers with the decoded bytes, typed application/wasm so
// `instantiateStreaming` (emscripten's Xwayland glue) takes them as well.
//
// Off in every other build (`VITE_GEX_ZWASM` is set by build-demo.sh only), and
// under Node, where there is no `import.meta.env`.

const ON = import.meta.env?.VITE_GEX_ZWASM === '1';

let decoder = null;
async function zstd() {
  if (!decoder) {
    decoder = (async () => {
      const { ZSTDDecoder } = await import('../eject-site/vendor/zstddec-stream.mjs');
      const d = new ZSTDDecoder();
      await d.init();
      return d;
    })();
  }
  return decoder;
}

/** zstd's frame magic, little-endian 0xFD2FB528. */
export function isZstd(b) {
  return b.length > 4 && b[0] === 0x28 && b[1] === 0xb5 && b[2] === 0x2f && b[3] === 0xfd;
}

/** Decode zstd bytes (the shared decoder, initialised once per realm). */
export async function unzstd(bytes) {
  return (await zstd()).decode(bytes);
}

export function installZFetch() {
  if (!ON || globalThis.__gexZFetch || typeof fetch !== 'function') return;
  const plain = globalThis.fetch.bind(globalThis);
  globalThis.__gexZFetch = plain;
  globalThis.fetch = async (input, init) => {
    const href = typeof input === 'string' ? input : input instanceof URL ? input.href : input?.url;
    let u = null;
    try {
      u = new URL(href, globalThis.location?.href);
    } catch {}
    if (u && u.origin === globalThis.location?.origin && u.pathname.endsWith('.wasm')) {
      u.pathname += '.zst';
      const res = await plain(u.href, init);
      if (res.ok) {
        const z = new Uint8Array(await res.arrayBuffer());
        // A missing file answers index.html with a 200 (the static server's
        // fallback): only a zstd frame is taken as the module.
        if (isZstd(z)) {
          const body = await unzstd(z);
          return new Response(body, {
            status: 200,
            headers: { 'content-type': 'application/wasm', 'content-length': String(body.length) },
          });
        }
      }
    }
    return plain(input, init);
  };
}

installZFetch();
