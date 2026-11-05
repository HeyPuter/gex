// Mesa's softpipe replay (gex-mesa-replay.js, ports/mesa/build.sh's
// non-page targets) under Node, fed transport packets directly: a GLES
// context renders and reads back a cleared texture, a legacy context then a
// 3.2 core one reports 3.2 or more (as a WGL program asks for them), a shader
// deleted while attached still links, and every malformed packet gets its
// decoder status.
//
// Env: GEX_TEST_MESA_BUILD (as the mesa-cold image: a directory with
// mesa-mt/build; default target/web-build), whose
// src/gallium/targets/gex-wasm/gex-mesa-replay.js must be built (ninja), a
// loud skip when it is not.
import { existsSync } from 'node:fs';
import { join as path } from 'node:path';
import { pathToFileURL } from 'node:url';
import { ROOT } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 300 };

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

const PACKET = () => join([
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


const decoder = new TextDecoder();

export default async function (t) {
  const top = process.env.GEX_TEST_MESA_BUILD || path(ROOT, 'target/web-build');
  const module = path(top, 'mesa-mt/build/src/gallium/targets/gex-wasm/gex-mesa-replay.js');
  if (!existsSync(module)) t.skip(`no ${module} (ninja -C ${path(top, 'mesa-mt/build')} src/gallium/targets/gex-wasm/gex-mesa-replay.js; GEX_TEST_MESA_BUILD)`);
  const { default: createMesa } = await import(pathToFileURL(module));
  const mesa = await createMesa();
  t.eq('init', mesa._gex_gl_init(64, 64), 0);

  const submitPacket = (packet) => {
    const input = mesa._malloc(packet.length || 1);
    if (packet.length) mesa.HEAPU8.set(packet, input);
    const result = mesa._gex_gl_submit(input, packet.length);
    mesa._free(input);
    return result;
  };
  const replyBytes = () => {
    const at = mesa._gex_gl_reply_data();
    return mesa.HEAPU8.slice(at, at + mesa._gex_gl_reply_len());
  };

  // A GLES context (a zero version: the best the host can give), an 8x8
  // RGBA8 texture as a framebuffer's colour, cleared and one pixel read back.
  t.eq('packet replay', submitPacket(PACKET()), 0);
  const reply = replyBytes();
  const view = new DataView(reply.buffer, reply.byteOffset, reply.byteLength);
  let cursor = 0;
  const readU32 = () => { const v = view.getUint32(cursor, true); cursor += 4; return v; };
  const readBlob = () => { const n = readU32(); const v = reply.slice(cursor, cursor + n); cursor += (n + 3) & ~3; return v; };
  const vendor = decoder.decode(readBlob());
  const renderer = decoder.decode(readBlob());
  const version = decoder.decode(readBlob());
  const status = readU32();
  const offset = readU32();
  const pixel = readBlob();
  const error = readU32();
  // The default client API is GLES, the transport's: a guest that never
  // calls eglBindAPI is a GLES client. Either identity is a modern context;
  // what this rejects is a fallback to something older than the corpus assumes.
  t.check('GL 3.3+ or GLES 3.0+', /^(OpenGL ES 3\.[0-9]|3\.[3-9]|4\.[0-9]) /.test(version), `${version} (${vendor}, ${renderer})`);
  t.eq('framebuffer status', status, GL_FRAMEBUFFER_COMPLETE);
  t.eq('read-back', `${offset} ${pixel.join(',')}`, '0 64,128,191,255');
  t.eq('GL error after replay', error, 0);
  t.eq('unparsed reply bytes', reply.length - cursor, 0);

  // A legacy context, then a 3.2 core one, as a WGL program makes them
  // (wglCreateContext to load wglCreateContextAttribsARB, then the context it
  // wants). A client has one backend context, so the second request gets the
  // first back: it must already report 3.2 or more, as a Windows driver's
  // legacy context does, not st/mesa's default compatibility cap of 3.1.
  const requests = [[1, 0, 1], [3, 2, 0]];
  const parts = [command(8)];
  for (const [major, minor, profile] of requests) parts.push(command(1, new Words().u32(1).u32(major).u32(minor).u32(profile)));
  t.eq('desktop context replay', submitPacket(join(parts)), 0);
  const bytes = replyBytes();
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
  const glVersion = (s) => { const m = /^(\d+)\.(\d+)/.exec(s); return m ? +m[1] * 10 + +m[2] : 0; };
  t.check('a 3.2 core request after a legacy context gets 3.2+', glVersion(versions[1]) >= 32, `core ${versions[1]}, legacy ${versions[0]}`);

  // A shader deleted while attached stays usable until it is detached: GL
  // programs may attach, delete, and only then compile and link. The decoder
  // must keep the guest name's mapping across DELETE_SHADER for that link.
  const GL_VERTEX_SHADER = 0x8b31, GL_FRAGMENT_SHADER = 0x8b30, GL_LINK_STATUS = 0x8b82;
  const text = (s) => new TextEncoder().encode(s);
  const vs = '#version 150\nin vec4 pos;\nvoid main() { gl_Position = pos; }\n';
  const fs = '#version 150\nout vec4 c;\nvoid main() { c = vec4(1.0); }\n';
  t.eq('deleted-while-attached replay', submitPacket(join([
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
  ])), 0);
  const linked = new DataView(replyBytes().buffer);
  t.eq('a shader deleted while attached: link status, GL error', `${linked.getUint32(0, true)} ${linked.getUint32(4, true)}`, '1 0');

  // A decoder failure poisons the connection: nothing resumes after it, so
  // each malformed packet is checked last, and only its status matters.
  for (const [label, packet, expected] of malformed) t.eq(`malformed: ${label}`, submitPacket(packet), expected);
}
