// Running a guest's OpenGL ES on the page's WebGL 2.
//
// This is the browser's half of the graphics device. The guest's
// libGLESv2.so.2 marshals each call into a buffer inside the guest's own
// memory; the shim hands this the address; and every command becomes a real
// WebGL call on a real canvas.
//
// WebGL 2 *is* OpenGL ES 3.0, so almost nothing here is a translation. The
// exceptions are the two places the web deliberately differs from native GL,
// and both are about the same thing -- a page may not be handed a raw
// pointer:
//
//   * Object names. Native GL uses integers the guest chose; WebGL uses
//     opaque objects. So there is a table per kind, indexed by the guest's
//     number.
//   * Buffer data. Native GL takes a pointer; WebGL takes a typed array. The
//     bytes are already in the guest's memory, so a subarray view is passed
//     and nothing is copied.
//
// The command encoding is generated from the Rust definition, so the guest,
// the native host and this cannot drift apart.

import { op, MAGIC, HEADER_BYTES, error } from './gl-proto.js';

// Header word offsets, in 32-bit units.
const H_MAGIC = 0;
const H_CAPACITY = 1;
const H_TAIL = 2;
const H_REPLY_CAP = 3;
const H_REPLY_LEN = 4;
const H_STATUS = 5;
const H_FAILED_AT = 6;

/// GL constants this file needs by name. The guest sends numbers, so these
/// are only for the few places a decision is made.
const GL_VENDOR = 0x1f00;
const GL_RENDERER = 0x1f01;
const GL_VERSION = 0x1f02;
const GL_SHADING_LANGUAGE_VERSION = 0x8b8c;
const GL_COMPILE_STATUS = 0x8b81;
const GL_LINK_STATUS = 0x8b82;

/// Reads a command's arguments.
class Args {
  constructor(view, start, end) {
    this.view = view;
    this.at = start;
    this.end = end;
  }
  u32() {
    if (this.at + 4 > this.end) return 0;
    const v = this.view.getUint32(this.at, true);
    this.at += 4;
    return v;
  }
  i32() {
    return this.u32() | 0;
  }
  f32() {
    if (this.at + 4 > this.end) return 0;
    const v = this.view.getFloat32(this.at, true);
    this.at += 4;
    return v;
  }
  /// A length word then that many bytes, as a view into guest memory rather
  /// than a copy: uploading a texture should not allocate.
  blob(bytes) {
    const len = this.u32();
    const start = this.at;
    if (start + len > this.end) return new Uint8Array(0);
    this.at += len;
    return bytes.subarray(start, start + len);
  }
  string(bytes) {
    return new TextDecoder().decode(this.blob(bytes));
  }
}

/// Builds the reply the guest reads back.
class Reply {
  constructor() {
    this.parts = [];
    this.length = 0;
  }
  u32(v) {
    const b = new Uint8Array(4);
    new DataView(b.buffer).setUint32(0, v >>> 0, true);
    this.parts.push(b);
    this.length += 4;
  }
  blob(bytes) {
    this.u32(bytes.length);
    this.parts.push(bytes);
    this.length += bytes.length;
    const pad = (4 - (bytes.length % 4)) % 4;
    if (pad) {
      this.parts.push(new Uint8Array(pad));
      this.length += pad;
    }
  }
  string(s) {
    this.blob(new TextEncoder().encode(s));
  }
  into(dst) {
    let at = 0;
    for (const p of this.parts) {
      if (at + p.length > dst.length) break;
      dst.set(p, at);
      at += p.length;
    }
    return at;
  }
}

export class GlDevice {
  /// `canvas` is where the guest's default framebuffer goes. A guest drawing
  /// to what it thinks is an offscreen pbuffer is drawing here, which is what
  /// makes the picture appear without anything having to copy it.
  constructor(canvas) {
    this.canvas = canvas;
    this.gl = canvas.getContext('webgl2', {
      alpha: false,
      antialias: false,
      depth: true,
      // The guest may read pixels back long after drawing them, and a page
      // that discards the buffer after compositing would hand back nothing.
      preserveDrawingBuffer: true,
    });
    this.buffers = new Map();
    this.textures = new Map();
    this.shaders = new Map();
    this.programs = new Map();
    this.framebuffers = new Map();
    this.ok = !!this.gl;
  }

  describe() {
    if (!this.ok) return 'no WebGL 2';
    const gl = this.gl;
    return `${gl.getParameter(gl.VERSION)} / ${gl.getParameter(gl.RENDERER)}`;
  }

  /// Run one batch. `memory` is the guest's WebAssembly.Memory and `ring` the
  /// address of the header inside it.
  ///
  /// Everything is read through fresh views because the guest's memory can
  /// grow between calls, which detaches any buffer held from before.
  submit(memory, ring) {
    const bytes = new Uint8Array(memory.buffer);
    const words = new Uint32Array(memory.buffer, ring, HEADER_BYTES / 4);
    if (words[H_MAGIC] !== MAGIC) return -1;

    const capacity = words[H_CAPACITY];
    const tail = words[H_TAIL];
    if (tail > capacity) return -1;

    const view = new DataView(memory.buffer);
    const base = ring + HEADER_BYTES;
    const reply = new Reply();
    let status = error.NONE;
    let index = 0;
    let at = 0;

    while (at < tail) {
      if (at + 8 > tail) {
        status = error.TRUNCATED;
        break;
      }
      const opcode = view.getUint32(base + at, true);
      const len = view.getUint32(base + at + 4, true);
      if (len < 8 || at + len > tail) {
        status = error.TRUNCATED;
        break;
      }
      const args = new Args(view, base + at + 8, base + at + len);
      if (!this.one(opcode, args, bytes, reply)) {
        status = error.BAD_OPCODE;
        break;
      }
      at += len;
      index++;
    }

    // Fresh views again: a command may have grown the guest's memory.
    const out = new Uint32Array(memory.buffer, ring, HEADER_BYTES / 4);
    const replyArea = new Uint8Array(
      memory.buffer,
      ring + HEADER_BYTES + capacity,
      out[H_REPLY_CAP],
    );
    const written = reply.into(replyArea);
    out[H_TAIL] = 0;
    out[H_REPLY_LEN] = written;
    out[H_STATUS] = status;
    out[H_FAILED_AT] = index;
    return 0;
  }

  /// Execute one command. `false` means the opcode is unknown, after which
  /// the stream cannot be read any further.
  one(opcode, a, bytes, reply) {
    const gl = this.gl;
    switch (opcode) {
      case op.CONTEXT_CREATE:
        reply.string(gl.getParameter(GL_VENDOR) ?? '');
        reply.string(gl.getParameter(GL_RENDERER) ?? '');
        reply.string(gl.getParameter(GL_VERSION) ?? '');
        break;
      case op.SURFACE_RESIZE: {
        const w = a.i32(), h = a.i32();
        // The guest asking for a pbuffer of some size is the closest thing
        // this design has to a request for a window, so the canvas takes it.
        if (w > 0 && h > 0) {
          this.canvas.width = w;
          this.canvas.height = h;
        }
        break;
      }
      case op.FINISH:
        gl.finish();
        reply.u32(0);
        break;

      case op.CLEAR_COLOR: gl.clearColor(a.f32(), a.f32(), a.f32(), a.f32()); break;
      case op.CLEAR_DEPTH: gl.clearDepth(a.f32()); break;
      case op.CLEAR: gl.clear(a.u32()); break;
      case op.VIEWPORT: gl.viewport(a.i32(), a.i32(), a.i32(), a.i32()); break;
      case op.SCISSOR: gl.scissor(a.i32(), a.i32(), a.i32(), a.i32()); break;
      case op.ENABLE: gl.enable(a.u32()); break;
      case op.DISABLE: gl.disable(a.u32()); break;
      case op.BLEND_FUNC: gl.blendFunc(a.u32(), a.u32()); break;
      case op.DEPTH_FUNC: gl.depthFunc(a.u32()); break;
      case op.CULL_FACE: gl.cullFace(a.u32()); break;
      case op.FRONT_FACE: gl.frontFace(a.u32()); break;
      case op.PIXEL_STOREI: gl.pixelStorei(a.u32(), a.i32()); break;

      case op.GEN_BUFFERS: {
        const n = a.u32();
        for (let i = 0; i < n; i++) this.buffers.set(a.u32(), gl.createBuffer());
        break;
      }
      case op.DELETE_BUFFERS: {
        const n = a.u32();
        for (let i = 0; i < n; i++) {
          const name = a.u32();
          gl.deleteBuffer(this.buffers.get(name) ?? null);
          this.buffers.delete(name);
        }
        break;
      }
      case op.BIND_BUFFER: {
        const target = a.u32(), name = a.u32();
        gl.bindBuffer(target, name ? (this.buffers.get(name) ?? null) : null);
        break;
      }
      case op.BUFFER_DATA: {
        const target = a.u32(), usage = a.u32(), data = a.blob(bytes);
        // A zero-length payload with a non-zero size means "allocate without
        // initialising", which the guest encodes as an empty blob.
        gl.bufferData(target, data.length ? data : 0, usage);
        break;
      }
      case op.BUFFER_SUB_DATA: {
        const target = a.u32(), offset = a.u32(), data = a.blob(bytes);
        gl.bufferSubData(target, offset, data);
        break;
      }

      case op.CREATE_SHADER: {
        const name = a.u32(), type = a.u32();
        this.shaders.set(name, gl.createShader(type));
        break;
      }
      case op.SHADER_SOURCE: {
        const name = a.u32(), src = a.string(bytes);
        const s = this.shaders.get(name);
        if (s) gl.shaderSource(s, src);
        break;
      }
      case op.COMPILE_SHADER: {
        const s = this.shaders.get(a.u32());
        if (s) gl.compileShader(s);
        break;
      }
      case op.DELETE_SHADER: {
        const name = a.u32();
        gl.deleteShader(this.shaders.get(name) ?? null);
        this.shaders.delete(name);
        break;
      }
      case op.CREATE_PROGRAM:
        this.programs.set(a.u32(), gl.createProgram());
        break;
      case op.ATTACH_SHADER: {
        const p = this.programs.get(a.u32()), s = this.shaders.get(a.u32());
        if (p && s) gl.attachShader(p, s);
        break;
      }
      case op.BIND_ATTRIB_LOCATION: {
        const p = this.programs.get(a.u32());
        const index = a.u32(), name = a.string(bytes);
        if (p) gl.bindAttribLocation(p, index, name);
        break;
      }
      case op.LINK_PROGRAM: {
        const p = this.programs.get(a.u32());
        if (p) gl.linkProgram(p);
        break;
      }
      case op.USE_PROGRAM: {
        const name = a.u32();
        // Remembered because a uniform is set against whatever program is
        // current, and the integer the guest holds only means something
        // relative to that program's list.
        this.current = name ? (this.programs.get(name) ?? null) : null;
        gl.useProgram(this.current);
        break;
      }
      case op.DELETE_PROGRAM: {
        const name = a.u32();
        gl.deleteProgram(this.programs.get(name) ?? null);
        this.programs.delete(name);
        break;
      }
      case op.GET_UNIFORM_LOCATION: {
        const p = this.programs.get(a.u32()), name = a.string(bytes);
        // WebGL hands back an opaque location object, and the guest wants an
        // integer. They are kept in a list per program and the index stands
        // in for the location, which is all the guest ever does with it.
        if (!p) { reply.u32(0xffffffff); break; }
        if (!p._locations) p._locations = [];
        const loc = gl.getUniformLocation(p, name);
        if (loc === null) { reply.u32(0xffffffff); break; }
        p._locations.push(loc);
        reply.u32(p._locations.length - 1);
        break;
      }
      case op.GET_ATTRIB_LOCATION: {
        const p = this.programs.get(a.u32()), name = a.string(bytes);
        reply.u32(p ? gl.getAttribLocation(p, name) : -1);
        break;
      }
      case op.GET_SHADER_IV: {
        const s = this.shaders.get(a.u32()), pname = a.u32();
        const v = s ? gl.getShaderParameter(s, pname) : 0;
        reply.u32(pname === GL_COMPILE_STATUS ? (v ? 1 : 0) : (v | 0));
        break;
      }
      case op.GET_PROGRAM_IV: {
        const p = this.programs.get(a.u32()), pname = a.u32();
        const v = p ? gl.getProgramParameter(p, pname) : 0;
        reply.u32(pname === GL_LINK_STATUS ? (v ? 1 : 0) : (v | 0));
        break;
      }
      case op.GET_SHADER_INFO_LOG: {
        const s = this.shaders.get(a.u32());
        reply.string(s ? (gl.getShaderInfoLog(s) ?? '') : '');
        break;
      }
      case op.GET_PROGRAM_INFO_LOG: {
        const p = this.programs.get(a.u32());
        reply.string(p ? (gl.getProgramInfoLog(p) ?? '') : '');
        break;
      }

      case op.UNIFORM_1I: gl.uniform1i(this.loc(a.i32()), a.i32()); break;
      case op.UNIFORM_1F: gl.uniform1f(this.loc(a.i32()), a.f32()); break;
      case op.UNIFORM_2F: gl.uniform2f(this.loc(a.i32()), a.f32(), a.f32()); break;
      case op.UNIFORM_3F:
        gl.uniform3f(this.loc(a.i32()), a.f32(), a.f32(), a.f32());
        break;
      case op.UNIFORM_4F:
        gl.uniform4f(this.loc(a.i32()), a.f32(), a.f32(), a.f32(), a.f32());
        break;
      case op.UNIFORM_MATRIX4FV: {
        const loc = this.loc(a.i32());
        const count = a.u32(), transpose = a.u32() !== 0;
        const data = a.blob(bytes);
        // A view rather than a copy, but it has to be aligned for Float32Array
        // and the guest's blob is only guaranteed word-aligned.
        const floats = new Float32Array(count * 16);
        new Uint8Array(floats.buffer).set(data.subarray(0, count * 64));
        gl.uniformMatrix4fv(loc, transpose, floats);
        break;
      }

      case op.ENABLE_VERTEX_ATTRIB_ARRAY: gl.enableVertexAttribArray(a.u32()); break;
      case op.DISABLE_VERTEX_ATTRIB_ARRAY: gl.disableVertexAttribArray(a.u32()); break;
      case op.VERTEX_ATTRIB_POINTER: {
        const index = a.u32(), size = a.i32(), type = a.u32();
        const normalized = a.u32() !== 0, stride = a.i32(), offset = a.u32();
        gl.vertexAttribPointer(index, size, type, normalized, stride, offset);
        break;
      }

      case op.GEN_TEXTURES: {
        const n = a.u32();
        for (let i = 0; i < n; i++) this.textures.set(a.u32(), gl.createTexture());
        break;
      }
      case op.DELETE_TEXTURES: {
        const n = a.u32();
        for (let i = 0; i < n; i++) {
          const name = a.u32();
          gl.deleteTexture(this.textures.get(name) ?? null);
          this.textures.delete(name);
        }
        break;
      }
      case op.BIND_TEXTURE: {
        const target = a.u32(), name = a.u32();
        gl.bindTexture(target, name ? (this.textures.get(name) ?? null) : null);
        break;
      }
      case op.ACTIVE_TEXTURE: gl.activeTexture(a.u32()); break;
      case op.TEX_PARAMETER_I: gl.texParameteri(a.u32(), a.u32(), a.i32()); break;
      case op.TEX_IMAGE_2D: {
        const target = a.u32(), level = a.i32(), internal = a.i32();
        const w = a.i32(), h = a.i32(), border = a.i32();
        const format = a.u32(), type = a.u32(), data = a.blob(bytes);
        gl.texImage2D(target, level, internal, w, h, border, format, type,
                      data.length ? data : null);
        break;
      }
      case op.TEX_SUB_IMAGE_2D: {
        const target = a.u32(), level = a.i32();
        const x = a.i32(), y = a.i32(), w = a.i32(), h = a.i32();
        const format = a.u32(), type = a.u32(), data = a.blob(bytes);
        gl.texSubImage2D(target, level, x, y, w, h, format, type, data);
        break;
      }

      case op.DRAW_ARRAYS: gl.drawArrays(a.u32(), a.i32(), a.i32()); break;
      case op.DRAW_ELEMENTS: gl.drawElements(a.u32(), a.i32(), a.u32(), a.u32()); break;

      case op.READ_PIXELS: {
        const x = a.i32(), y = a.i32(), w = a.i32(), h = a.i32();
        const format = a.u32(), type = a.u32();
        const px = new Uint8Array(Math.max(0, w) * Math.max(0, h) * 4);
        gl.readPixels(x, y, w, h, format, type, px);
        reply.blob(px);
        break;
      }
      case op.GET_STRING: {
        const name = a.u32();
        let s = gl.getParameter(name);
        if (name === GL_SHADING_LANGUAGE_VERSION && !s) s = '';
        reply.string(typeof s === 'string' ? s : String(s ?? ''));
        break;
      }
      case op.GET_INTEGERV: {
        const v = gl.getParameter(a.u32());
        const list = typeof v === 'number' ? [v] : Array.from(v ?? [0]);
        for (let i = 0; i < 4; i++) reply.u32(list[i] ?? 0);
        break;
      }
      case op.GET_ERROR:
        reply.u32(gl.getError());
        break;

      default:
        return false;
    }
    return true;
  }

  /// Turn the integer the guest was given back into a WebGL uniform
  /// location, against whichever program is current -- which is the program
  /// the guest was looking at when it asked.
  loc(index) {
    const p = this.current;
    if (!p || !p._locations || index < 0) return null;
    return p._locations[index] ?? null;
  }
}
