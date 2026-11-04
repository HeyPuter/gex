// zstd bytes decoded in the page or a Worker, with zstddec (the shared
// decoder, initialised once per realm).

let decoder = null;
async function zstd() {
  if (!decoder) {
    decoder = (async () => {
      const { ZSTDDecoder } = await import('../vendor/zstddec/zstddec-stream.mjs');
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
