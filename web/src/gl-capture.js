// Recording the guest's GL command stream in the browser.
//
// The same `.gexgltr` file the native device writes (the format is defined by
// `crates/gex-gl/src/trace.rs`), produced from the page instead. That matters
// because the browser is where the WebGPU backend runs: a defect that only
// appears there has to be recorded there before it can be replayed anywhere
// else, and a recording taken from the native device would be a different
// program's stream.
//
// Recording is off unless asked for, and it never changes what is executed:
// the bytes are copied before the batch runs, so a batch that kills the
// device is still in the file.

import { pageSearch } from './page-env.js';

const MAGIC = "GEXGLTR\0";
const FORMAT_VERSION = 1;
const HEADER_BYTES = 24;

/** Default ceiling, so a long-running page cannot exhaust its own memory. */
const DEFAULT_MAX_BYTES = 64 * 1024 * 1024;

function padding(length) {
  return (4 - (length & 3)) & 3;
}

export class GlCapture {
  constructor(protocolVersion, width, height, maxBytes = DEFAULT_MAX_BYTES) {
    const header = new Uint8Array(HEADER_BYTES);
    const view = new DataView(header.buffer);
    for (let i = 0; i < 8; i++) header[i] = MAGIC.charCodeAt(i);
    view.setUint32(8, FORMAT_VERSION, true);
    view.setUint32(12, protocolVersion, true);
    view.setUint32(16, width, true);
    view.setUint32(20, height, true);

    this.parts = [header];
    this.bytes = HEADER_BYTES;
    this.batches = 0;
    this.maxBytes = maxBytes;
    /** Set once the ceiling stopped a batch from being recorded. */
    this.truncated = false;
  }

  /**
   * Append one batch. `bytes` must be a copy or a view that is read here and
   * now; the caller's buffer may be reused or detached immediately after.
   */
  record(bytes, replyCapacity) {
    const size = 8 + bytes.length + padding(bytes.length);
    if (this.bytes + size > this.maxBytes) {
      this.truncated = true;
      return false;
    }
    const record = new Uint8Array(size);
    const view = new DataView(record.buffer);
    view.setUint32(0, bytes.length, true);
    view.setUint32(4, replyCapacity, true);
    record.set(bytes, 8);
    this.parts.push(record);
    this.bytes += size;
    this.batches++;
    return true;
  }

  toBytes() {
    const out = new Uint8Array(this.bytes);
    let at = 0;
    for (const part of this.parts) {
      out.set(part, at);
      at += part.length;
    }
    return out;
  }

  /** Base64, because that is what survives a CDP `Runtime.evaluate`. */
  toBase64() {
    const bytes = this.toBytes();
    let binary = "";
    for (let at = 0; at < bytes.length; at += 0x8000) {
      binary += String.fromCharCode.apply(null, bytes.subarray(at, at + 0x8000));
    }
    return btoa(binary);
  }
}

/**
 * The capture a device should use, or null.
 *
 * Reads `?glcapture=1` in a page and `GEX_GLCAPTURE` under Node, matching how
 * every other switch in `web/src` is spelled. The result is published on
 * `globalThis.__gexGlCapture` so a CDP driver can collect it without the page
 * having to offer a download.
 */
export function captureIfRequested(protocolVersion, width, height, given = null) {
  const search = pageSearch();
  const value = given ?? (search
    ? new URLSearchParams(search).get("glcapture")
    : (globalThis.process?.env?.GEX_GLCAPTURE ?? null));
  if (!value || value === "0") return null;

  const limit = Number(value);
  const capture = new GlCapture(
    protocolVersion,
    width,
    height,
    Number.isFinite(limit) && limit > 1 ? limit : DEFAULT_MAX_BYTES,
  );
  globalThis.__gexGlCapture = capture;
  return capture;
}
