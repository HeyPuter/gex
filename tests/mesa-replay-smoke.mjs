import { pathToFileURL } from "node:url";

const modulePath = process.argv[2];
if (!modulePath) throw new Error("usage: mesa-replay-smoke.mjs MODULE.js");
const { default: createMesa } = await import(pathToFileURL(modulePath));
const mesa = await createMesa();

const init = mesa._gex_gl_init(64, 64);
if (init !== 0) throw new Error(`Mesa replay initialization failed: ${init}`);

function submitPacket(packet) {
  const input = mesa._malloc(packet.length || 1);
  if (packet.length) mesa.HEAPU8.set(packet, input);
  const result = mesa._gex_gl_submit(input, packet.length);
  mesa._free(input);
  return result;
}

class Words {
  constructor() { this.bytes = []; }
  u32(value) {
    value >>>= 0;
    this.bytes.push(value & 255, (value >>> 8) & 255,
                    (value >>> 16) & 255, (value >>> 24) & 255);
    return this;
  }
  i32(value) { return this.u32(value); }
  f32(value) {
    const data = new ArrayBuffer(4);
    new DataView(data).setFloat32(0, value, true);
    this.bytes.push(...new Uint8Array(data));
    return this;
  }
  u64(value) { return this.u32(Number(value & 0xffffffffn)).u32(Number(value >> 32n)); }
  blob(bytes = new Uint8Array()) {
    this.u32(bytes.length);
    this.bytes.push(...bytes);
    while (this.bytes.length & 3) this.bytes.push(0);
    return this;
  }
}

function command(op, payload = new Words()) {
  const result = new Words().u32(op).u32(8 + payload.bytes.length);
  result.bytes.push(...payload.bytes);
  return new Uint8Array(result.bytes);
}

function join(parts) {
  const size = parts.reduce((sum, part) => sum + part.length, 0);
  const result = new Uint8Array(size);
  let offset = 0;
  for (const part of parts) { result.set(part, offset); offset += part.length; }
  return result;
}

const malformed = [
  ["truncated header", new Uint8Array([1]), -1],
  ["undersized command", new Uint8Array([1, 0, 0, 0, 7, 0, 0, 0]), -1],
  ["command exceeds packet", new Uint8Array([1, 0, 0, 0, 12, 0, 0, 0]), -1],
  ["unknown opcode", command(0xffffffff), -2],
  ["context create without its payload", command(1), -2],
  ["short fixed payload", command(16), -2],
  ["trailing fixed payload", command(17, new Words().u32(0).u32(0)), -2],
  ["short object array", command(96, new Words().u32(2).u32(101)), -2],
];

const GL_TEXTURE_2D = 0x0de1;
const GL_RGBA = 0x1908;
const GL_UNSIGNED_BYTE = 0x1401;
const GL_RGBA8 = 0x8058;
const GL_FRAMEBUFFER = 0x8d40;
const GL_COLOR_ATTACHMENT0 = 0x8ce0;
const GL_FRAMEBUFFER_COMPLETE = 0x8cd5;
const GL_COLOR_BUFFER_BIT = 0x00004000;

const packet = join([
  // CONTEXT_CREATE { client_api, major, minor }: OpenGL ES, and a zero version
  // meaning "the best the host can give".
  command(1, new Words().u32(0).u32(0).u32(0)),
  command(96, new Words().u32(1).u32(101)),
  command(98, new Words().u32(GL_TEXTURE_2D).u32(101)),
  command(101, new Words()
    .u32(GL_TEXTURE_2D).i32(0).i32(GL_RGBA8).i32(8).i32(8).i32(0)
    .u32(GL_RGBA).u32(GL_UNSIGNED_BYTE).u32(0).u64(0n).blob()),
  command(103, new Words().u32(1).u32(201)),
  command(105, new Words().u32(GL_FRAMEBUFFER).u32(201)),
  command(107, new Words().u32(GL_FRAMEBUFFER).u32(GL_COLOR_ATTACHMENT0)
    .u32(GL_TEXTURE_2D).u32(101).i32(0)),
  command(106, new Words().u32(GL_FRAMEBUFFER)),
  command(16, new Words().f32(0.25).f32(0.5).f32(0.75).f32(1)),
  command(17, new Words().u32(GL_COLOR_BUFFER_BIT)),
  command(128, new Words().i32(3).i32(4).i32(1).i32(1)
    .u32(GL_RGBA).u32(GL_UNSIGNED_BYTE).u32(0).u64(0n)),
  command(131),
]);

const submit = submitPacket(packet);
if (submit !== 0) throw new Error(`packet replay failed: ${submit}`);

const replyLength = mesa._gex_gl_reply_len();
const replyPointer = mesa._gex_gl_reply_data();
const reply = mesa.HEAPU8.slice(replyPointer, replyPointer + replyLength);
const view = new DataView(reply.buffer, reply.byteOffset, reply.byteLength);
let cursor = 0;
function readU32() { const value = view.getUint32(cursor, true); cursor += 4; return value; }
function readBlob() {
  const length = readU32();
  const value = reply.slice(cursor, cursor + length);
  cursor += (length + 3) & ~3;
  return value;
}
const decoder = new TextDecoder();
const vendor = decoder.decode(readBlob());
const renderer = decoder.decode(readBlob());
const version = decoder.decode(readBlob());
const framebufferStatus = readU32();
const pixelOffset = readU32();
const pixel = readBlob();
const error = readU32();

// The backend's default client API is GLES, because that is the transport's
// default: a guest that never calls eglBindAPI is a GLES client, and its
// shader source will not compile in a core profile. A desktop-GL trace asks
// for core with gex_gl_set_api. Either identity is a modern context; what
// this rejects is a fallback to something older than the corpus assumes.
if (!/^(OpenGL ES 3\.[0-9]|3\.[3-9]|4\.[0-9]) /.test(version))
  throw new Error(`transport oracle requires GL 3.3+ or GLES 3.0+: ${version}`);
if (framebufferStatus !== GL_FRAMEBUFFER_COMPLETE)
  throw new Error(`incomplete framebuffer: 0x${framebufferStatus.toString(16)}`);
if (pixelOffset !== 0 || pixel.join(",") !== "64,128,191,255")
  throw new Error(`wrong readback offset=${pixelOffset} pixel=${pixel.join(",")}`);
if (error !== 0) throw new Error(`GL error after replay: 0x${error.toString(16)}`);
if (cursor !== reply.length) throw new Error(`unparsed reply bytes: ${reply.length - cursor}`);

// A legacy context, then a 3.2 core one, as a WGL program makes them
// (wglCreateContext to load wglCreateContextAttribsARB, then the context it
// wants). A client has one backend context, so the second request gets the
// first back: it must already report 3.2 or more, as a Windows driver's
// legacy context does, not st/mesa's default compatibility cap of 3.1.
function contextVersions(...requests) {
  const parts = [command(8)];
  for (const [major, minor, profile] of requests)
    parts.push(command(1, new Words().u32(1).u32(major).u32(minor).u32(profile)));
  const status = submitPacket(join(parts));
  if (status !== 0) throw new Error(`desktop context replay failed: ${status}`);
  const length = mesa._gex_gl_reply_len();
  const at = mesa._gex_gl_reply_data();
  const bytes = mesa.HEAPU8.slice(at, at + length);
  const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const versions = [];
  let pos = 0;
  for (let i = 0; i < requests.length; i++) {
    for (let field = 0; field < 3; field++) {
      const n = dv.getUint32(pos, true);
      if (field === 2) versions.push(decoder.decode(bytes.slice(pos + 4, pos + 4 + n)));
      pos += 4 + ((n + 3) & ~3);
    }
  }
  return versions;
}
const glVersion = (s) => { const m = /^(\d+)\.(\d+)/.exec(s); return m ? +m[1] * 10 + +m[2] : 0; };
const [legacyVersion, coreVersion] = contextVersions([1, 0, 1], [3, 2, 0]);
if (glVersion(coreVersion) < 32)
  throw new Error(`a 3.2 core request after a legacy context got ${coreVersion} (legacy: ${legacyVersion})`);

// A shader deleted while attached stays usable until it is detached: GL
// programs may attach, delete, and only then compile and link. The decoder
// must keep the guest name's mapping across DELETE_SHADER for that link to
// succeed.
function deletedWhileAttachedLinks() {
  const GL_VERTEX_SHADER = 0x8b31, GL_FRAGMENT_SHADER = 0x8b30;
  const GL_LINK_STATUS = 0x8b82;
  const text = (s) => new TextEncoder().encode(s);
  const vs = "#version 150\nin vec4 pos;\nvoid main() { gl_Position = pos; }\n";
  const fs = "#version 150\nout vec4 c;\nvoid main() { c = vec4(1.0); }\n";
  const status = submitPacket(join([
    command(51, new Words().u32(301)),
    command(48, new Words().u32(302).u32(GL_VERTEX_SHADER)),
    command(49, new Words().u32(302).blob(text(vs))),
    command(52, new Words().u32(301).u32(302)),
    command(56, new Words().u32(302)),
    command(48, new Words().u32(303).u32(GL_FRAGMENT_SHADER)),
    command(49, new Words().u32(303).blob(text(fs))),
    command(52, new Words().u32(301).u32(303)),
    command(56, new Words().u32(303)),
    command(50, new Words().u32(302)),
    command(50, new Words().u32(303)),
    command(54, new Words().u32(301)),
    command(61, new Words().u32(301).u32(GL_LINK_STATUS)),
    command(131),
  ]));
  if (status !== 0) throw new Error(`deleted-while-attached replay failed: ${status}`);
  const at = mesa._gex_gl_reply_data();
  const dv = new DataView(mesa.HEAPU8.buffer, at, mesa._gex_gl_reply_len());
  return { linked: dv.getUint32(0, true), error: dv.getUint32(4, true) };
}
const attached = deletedWhileAttachedLinks();
if (attached.linked !== 1 || attached.error !== 0)
  throw new Error(`a shader deleted while attached: link status ${attached.linked}, ` +
                  `GL error 0x${attached.error.toString(16)}`);

// Decoder failure poisons the graphics connection: execution and decoding are
// intentionally not resumed afterward.  A caller must discard the worker GL
// context on either negative status, because an operation may have consumed a
// valid prefix before discovering its truncated tail.
for (const [label, bytes, expected] of malformed) {
  const actual = submitPacket(bytes);
  if (actual !== expected)
    throw new Error(`${label}: expected decoder status ${expected}, got ${actual}`);
}

console.log(`PASS gex-mesa-replay vendor=${vendor} renderer=${renderer}`);
console.log(`PASS gex-mesa-replay malformed=${malformed.length}`);
console.log(`PASS gex-mesa-replay version=${version} pixel=${pixel.join(",")}`);
console.log(`PASS gex-mesa-replay legacy-then-core=${coreVersion}`);
console.log(`PASS gex-mesa-replay deleted-while-attached links`);
