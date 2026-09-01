import { writeFileSync } from 'node:fs';
import { Cdp, browserWsUrl } from '../web/test/cdp.mjs';

const chromePort = Number(process.env.GEX_CHROME_PORT || 9335);
const servePort = Number(process.env.GEX_SERVE_PORT || 8091);
/* The instrumented build runs the same corpus an order of magnitude slower
 * than the release one, so the gate scripts raise this.
 */
const timeoutMs = Number(process.env.GEX_WGPU_TIMEOUT_MS || 60000);
// Which page to drive. The Gallium corpus is the default; the replay page
// runs a recorded guest stream through the same driver.
const page = process.env.GEX_WGPU_PAGE || 'mesa-wgpu-smoke.html';
const extraQuery = process.env.GEX_WGPU_QUERY ? `&${process.env.GEX_WGPU_QUERY}` : '';
const ws = await browserWsUrl(chromePort, 5000);
if (!ws) throw new Error(`persistent CDP browser unavailable on port ${chromePort}`);

const browser = await Cdp.connect(ws);
let targetId;
try {
  ({ targetId } = await browser.send('Target.createTarget', {
    url: `http://127.0.0.1:${servePort}/${page}?run=${Date.now()}${extraQuery}`,
  }));
  const { sessionId } = await browser.send('Target.attachToTarget', { targetId, flatten: true });
  await browser.send('Runtime.enable', {}, sessionId);
  await browser.send('Page.enable', {}, sessionId);

  const errors = [];
  browser.on('Runtime.exceptionThrown', ({ exceptionDetails }, sid) => {
    if (sid === sessionId) errors.push(exceptionDetails?.text || 'page exception');
  });
  browser.on('Runtime.consoleAPICalled', ({ type, args }, sid) => {
    if (sid !== sessionId) return;
    const line = args.map((arg) => arg.value ?? arg.description ?? '').join(' ');
    /* Debug/sanitizer builds write the per-shader corpus trace to stderr, which
     * Emscripten routes to console.error.  That trace is progress output, not a
     * failure; everything else on the error channel still fails the run.
     */
    // Diagnostics the driver writes to stderr are progress, not failure: the
    // hook audit lists what is unimplemented, and the refusal reporter names
    // the first unsupported operation. The page's outcome object is what
    // decides pass or fail for these runs.
    // `gex-gl: ` belongs here too: that is the protocol decoder's own trace
    // -- buffer uploads and the like -- and it is progress output by exactly
    // the same argument as the driver's. Leaving it out failed a replay whose
    // outcome object said ok, on three glBufferData lines.
    const diagnostic =
      line.startsWith('GEX corpus shader: ') || line.startsWith('gex-wgpu: ') ||
      line.startsWith('gex-gl: ');
    if (type === 'error' && !diagnostic) errors.push(line);
    else if (line) console.log(line);
  });

  const deadline = Date.now() + timeoutMs;
  let outcome;
  while (Date.now() < deadline) {
    const { result } = await browser.send('Runtime.evaluate', {
      expression: 'globalThis.__gexWgpuResult || null',
      returnByValue: true,
    }, sessionId);
    if (result.value) {
      outcome = result.value;
      // The surface, when something asked for it. Fetched separately from the
      // outcome because it is ~a megabyte of base64 and only the differential
      // gate wants it.
      if (process.env.GEX_WGPU_SURFACE_OUT) {
        const surface = await browser.send('Runtime.evaluate', {
          expression: 'globalThis.__gexWgpuSurface || null',
          returnByValue: true,
        }, sessionId);
        if (surface?.result?.value) {
          const pixels = Buffer.from(surface.result.value, 'base64');
          const [w, h] = String(outcome.size || '0x0').split('x').map(Number);
          const header = Buffer.alloc(16);
          header.write('GEXSURF\0', 0, 'latin1');
          header.writeUInt32LE(w || 0, 8);
          header.writeUInt32LE(h || 0, 12);
          writeFileSync(process.env.GEX_WGPU_SURFACE_OUT,
                        Buffer.concat([header, pixels]));
          console.log(`SURFACE wrote ${process.env.GEX_WGPU_SURFACE_OUT} ${w}x${h}`);
        }
      }
      break;
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  if (!outcome) throw new Error('WebGPU Gallium smoke timed out');
  if (!outcome.ok || errors.length)
    throw new Error(`WebGPU Gallium smoke failed: ${JSON.stringify({ outcome, errors })}`);
  // The corpus page reports an exit code; the replay page reports what it
  // replayed and the digest that makes it comparable to softpipe.
  console.log(
    outcome.digest !== undefined && outcome.digest !== null
      ? `PASS gex-wgpu-browser digest=${outcome.digest} pixels=${outcome.pixels} ` +
        `batches=${outcome.batches}/${outcome.total} size=${outcome.size}`
      : `PASS gex-wgpu-browser rc=${outcome.rc}`,
  );
} finally {
  if (targetId) await browser.send('Target.closeTarget', { targetId }).catch(() => {});
  browser.close();
}
