// Reading and rewriting a recorded guest GL command stream.
//
// The format is defined by `crates/gex-gl/src/trace.rs`, which is what writes
// the files; this is the reader the replay and reduction tools share. Keeping
// one copy of the parsing here means a tool that mis-frames a record fails in
// one place rather than three.

export const MAGIC = "GEXGLTR\0";
export const FORMAT_VERSION = 1;
export const TRACE_HEADER_BYTES = 24;

const decoder = new TextDecoder("latin1");

function padding(length) {
  return (4 - (length & 3)) & 3;
}

/** Parse a trace file. Throws on anything it was not written to read. */
export function parseTrace(bytes) {
  if (bytes.length < TRACE_HEADER_BYTES) throw new Error("trace ends inside its header");
  if (decoder.decode(bytes.subarray(0, 8)) !== MAGIC) throw new Error("not a gex GL trace");
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const formatVersion = view.getUint32(8, true);
  if (formatVersion !== FORMAT_VERSION)
    throw new Error(`trace format ${formatVersion} is from a different build`);
  const info = {
    formatVersion,
    protocolVersion: view.getUint32(12, true),
    width: view.getUint32(16, true),
    height: view.getUint32(20, true),
  };

  const batches = [];
  let at = TRACE_HEADER_BYTES;
  while (at < bytes.length) {
    if (at + 8 > bytes.length) throw new Error("trace ends inside a record header");
    const length = view.getUint32(at, true);
    const replyCapacity = view.getUint32(at + 4, true);
    const start = at + 8;
    const end = start + length;
    if (end + padding(length) > bytes.length) throw new Error("trace ends inside a record");
    batches.push({ bytes: bytes.slice(start, end), replyCapacity });
    at = end + padding(length);
  }
  return { info, batches };
}

/** Serialize a trace, byte for byte what a recording of it would have been. */
export function writeTrace(trace) {
  let size = TRACE_HEADER_BYTES;
  for (const batch of trace.batches) size += 8 + batch.bytes.length + padding(batch.bytes.length);
  const out = new Uint8Array(size);
  const view = new DataView(out.buffer);
  for (let i = 0; i < 8; i++) out[i] = MAGIC.charCodeAt(i);
  view.setUint32(8, FORMAT_VERSION, true);
  view.setUint32(12, trace.info.protocolVersion, true);
  view.setUint32(16, trace.info.width, true);
  view.setUint32(20, trace.info.height, true);
  let at = TRACE_HEADER_BYTES;
  for (const batch of trace.batches) {
    view.setUint32(at, batch.bytes.length, true);
    view.setUint32(at + 4, batch.replyCapacity, true);
    out.set(batch.bytes, at + 8);
    at += 8 + batch.bytes.length + padding(batch.bytes.length);
  }
  return out;
}

/**
 * Split one batch into its commands. Each carries the whole `{op, size, body}`
 * encoding, so a reduced stream is rebuilt by concatenation and never has to
 * re-encode a payload it does not understand.
 */
export function splitCommands(batchBytes) {
  const view = new DataView(batchBytes.buffer, batchBytes.byteOffset, batchBytes.byteLength);
  const commands = [];
  let at = 0;
  while (at < batchBytes.length) {
    if (at + 8 > batchBytes.length) throw new Error(`batch ends inside a command header at ${at}`);
    const op = view.getUint32(at, true);
    const size = view.getUint32(at + 4, true);
    if (size < 8 || at + size > batchBytes.length)
      throw new Error(`command at ${at} claims ${size} bytes`);
    commands.push({ op, bytes: batchBytes.slice(at, at + size) });
    at += size;
  }
  return commands;
}

export function joinCommands(commands) {
  let size = 0;
  for (const command of commands) size += command.bytes.length;
  const out = new Uint8Array(size);
  let at = 0;
  for (const command of commands) {
    out.set(command.bytes, at);
    at += command.bytes.length;
  }
  return out;
}

/**
 * Every command in the trace, in order, each tagged with the batch it came
 * from. Reduction works on this flat list; `rebuild` puts the survivors back
 * into their original batches so submission boundaries -- which are where the
 * guest asked for an answer -- are preserved.
 */
export function flatten(trace) {
  const commands = [];
  trace.batches.forEach((batch, index) => {
    for (const command of splitCommands(batch.bytes)) commands.push({ ...command, batch: index });
  });
  return commands;
}

export function rebuild(trace, commands) {
  const perBatch = trace.batches.map(() => []);
  for (const command of commands) perBatch[command.batch].push(command);
  return {
    info: trace.info,
    // A batch every one of whose commands was removed is dropped rather than
    // submitted empty: an empty submission is a different thing to replay.
    batches: trace.batches
      .map((batch, index) => ({ bytes: joinCommands(perBatch[index]), replyCapacity: batch.replyCapacity }))
      .filter((batch) => batch.bytes.length > 0),
  };
}

/** A command builder for the synthetic queries a replay appends. */
export class Words {
  constructor() {
    this.bytes = [];
  }
  u32(value) {
    value >>>= 0;
    this.bytes.push(value & 255, (value >>> 8) & 255, (value >>> 16) & 255, (value >>> 24) & 255);
    return this;
  }
  i32(value) {
    return this.u32(value);
  }
  u64(value) {
    return this.u32(Number(BigInt(value) & 0xffffffffn)).u32(Number(BigInt(value) >> 32n));
  }
}

export function command(op, payload = new Words()) {
  const out = new Uint8Array(8 + payload.bytes.length);
  const view = new DataView(out.buffer);
  view.setUint32(0, op, true);
  view.setUint32(4, out.length, true);
  out.set(payload.bytes, 8);
  return out;
}

export const op = {
  CONTEXT_CREATE: 1,
  SURFACE_RESIZE: 2,
  FINISH: 3,
  PRESENT: 4,
  READ_PIXELS: 128,
  GET_ERROR: 131,
};
