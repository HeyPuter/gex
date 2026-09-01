//! The command stream between a guest's GL driver and the host's.
//!
//! The split is at the API, not at a virtual GPU. virtio-gpu/VirGL would run
//! Mesa's state tracker as translated aarch64 (tens of thousands of
//! instructions per draw) and compile each shader twice. Instead the guest's
//! `libGLESv2.so.2` is a pure marshaller: a call becomes a few dozen bytes
//! appended to a buffer, and the host's native driver runs them. This is what
//! Android's emulator does.
//!
//! The buffer is guest memory the host already has mapped, so a call is a memcpy
//! with no syscall or host call out of wasm (the expensive thing). One kick per
//! flush hands over the whole batch, forced early only when a call needs an
//! answer. Names are guest-allocated: GL accepts an unseen name, and for shaders
//! and programs, where it will not, the host keeps a guest-to-host map.

/// Marks the head of a ring. The bytes "GEXG", little-endian.
pub const MAGIC: u32 = 0x4758_4547;
/// Version of the command ABI. A guest and host must agree before any opcode
/// is decoded; generated opcode policy changes require incrementing this.
pub const PROTOCOL_VERSION: u32 = 8;

/// The fixed header at the start of the shared buffer. `repr(C)` for the C
/// guest half; every field 32 bits, 32 bytes total.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub magic: u32,
    /// Bytes of command area, which begins immediately after this header.
    pub capacity: u32,
    /// Bytes of command the guest has written. The host consumes all of them
    /// and sets it back to zero.
    pub tail: u32,
    /// Bytes of reply area, which begins at `HEADER + capacity`.
    pub reply_capacity: u32,
    /// Bytes of reply the host has written.
    pub reply_len: u32,
    /// Non-zero when the host gave up on the batch: see [`error`].
    pub status: u32,
    /// Which command failed, counting from zero.
    pub failed_at: u32,
    pub protocol_version: u32,
}

/// Size of [`Header`] in bytes.
pub const HEADER: u32 = 32;

/// Which client API a context is for, as `eglBindAPI` selects it.
pub mod client_api {
    /// OpenGL ES, EGL's default when an application never calls `eglBindAPI`.
    pub const ES: u32 = 0;
    /// Desktop OpenGL, which `eglBindAPI(EGL_OPENGL_API)` selects.
    pub const DESKTOP: u32 = 1;
}

pub mod error {
    pub const NONE: u32 = 0;
    /// The batch ended in the middle of a command.
    pub const TRUNCATED: u32 = 1;
    /// An opcode the host does not know.
    pub const BAD_OPCODE: u32 = 2;
    /// An argument that cannot be right: a length past the end of the buffer,
    /// a name that was never created.
    pub const BAD_ARGUMENT: u32 = 3;
    /// There is no GL context to run this against.
    pub const NO_CONTEXT: u32 = 4;
    /// The command layouts/opcode assignments are from a different ABI.
    pub const VERSION_MISMATCH: u32 = 5;
}

/// Opcodes, numbered in blocks so adding to one group does not renumber
/// another and break a guest built against an older header.
/// What a [`op::SURFACE_ATTACH`] names.
pub mod surface {
    /// A `wl_surface`, by its object id on the guest's own compositor
    /// connection.
    pub const WAYLAND: u32 = 0;
    /// An X drawable, by XID. The window manager resolves it to the surface
    /// the X server publishes for that window.
    pub const X: u32 = 1;
}

pub mod op {
    // -- the device itself ------------------------------------------------
    /// `{ client_api, major, minor }` -- make a context of the API the
    /// application asked for and make it current. Replies: vendor, renderer,
    /// version.
    ///
    /// The API has to be on the wire because only the guest knows it: it comes
    /// from `eglBindAPI`, and a host that guesses will hand a desktop program
    /// an ES context, or the reverse. The guest therefore defers this command
    /// until `eglCreateContext`, which is where a real EGL implementation
    /// makes the context and the same information is available.
    ///
    /// `client_api` is [`client_api::ES`] or [`client_api::DESKTOP`]. A zero
    /// major asks the host for the highest version it can honour, which is
    /// what an application that never states one should get.
    pub const CONTEXT_CREATE: u32 = 1;
    /// `{ width, height }` -- the offscreen surface everything draws to.
    pub const SURFACE_RESIZE: u32 = 2;
    /// `{}` -- finish everything and reply when it is done.
    pub const FINISH: u32 = 3;
    /// `{}` -- submit the completed default framebuffer to the display.
    pub const PRESENT: u32 = 4;
    /// `{ kind, id, token }` -- what the frames after this one belong to.
    ///
    /// The host draws for a guest that has a window somewhere else: a
    /// `wl_surface` the compositor is already showing, or an X drawable
    /// belonging to a window an X server owns. Naming it is what lets the
    /// frame be *composited* rather than blitted over the whole output --
    /// without it there is nowhere to put the pixels, since with Mesa on the
    /// host nothing comes back to the guest to draw with.
    ///
    /// `kind` is [`surface::WAYLAND`] or [`surface::X`]; `id` is the
    /// `wl_surface` object id or the XID; `token` is the guest's pid, which is
    /// how the compositor names the process that published the surface (see
    /// `gex_wl_token`). A zero `id` detaches, which is what a guest drawing to
    /// nothing but the output does (the gears demo), and is the default.
    ///
    /// An X drawable needs no token: the surface belongs to the X server's
    /// process, not the client's, and the window manager is what resolves one
    /// to the other.
    pub const SURFACE_ATTACH: u32 = 5;

    // -- state ------------------------------------------------------------
    pub const CLEAR_COLOR: u32 = 16; // { r, g, b, a } as floats
    pub const CLEAR: u32 = 17; // { mask }
    pub const VIEWPORT: u32 = 18; // { x, y, w, h }
    pub const ENABLE: u32 = 19; // { cap }
    pub const DISABLE: u32 = 20; // { cap }
    pub const BLEND_FUNC: u32 = 21; // { src, dst }
    pub const DEPTH_FUNC: u32 = 22; // { func }
    pub const CULL_FACE: u32 = 23; // { mode }
    pub const FRONT_FACE: u32 = 24; // { mode }
    pub const SCISSOR: u32 = 25; // { x, y, w, h }
    pub const PIXEL_STOREI: u32 = 26; // { pname, param }
    pub const CLEAR_DEPTH: u32 = 27; // { depth as float }
    pub const DEPTH_MASK: u32 = 28; // { enabled }
    pub const HINT: u32 = 29;
    pub const COLOR_MASK: u32 = 30;
    pub const BLEND_EQUATION: u32 = 31;

    // -- buffers ----------------------------------------------------------
    pub const GEN_BUFFERS: u32 = 32; // { count, names... }
    pub const DELETE_BUFFERS: u32 = 33; // { count, names... }
    pub const BIND_BUFFER: u32 = 34; // { target, name }
    pub const BUFFER_DATA: u32 = 35; // { target, usage, size:u64, has_data, blob }
    pub const BUFFER_SUB_DATA: u32 = 36; // { target, offset:u64, size:u64, blob }
    pub const BLEND_FUNC_SEPARATE: u32 = 37;
    pub const POLYGON_OFFSET: u32 = 38;
    pub const LINE_WIDTH: u32 = 39;
    pub const CLEAR_STENCIL: u32 = 40;
    pub const DEPTH_RANGE: u32 = 41;
    pub const BIND_BUFFER_BASE: u32 = 42;
    pub const GEN_SAMPLERS: u32 = 43;
    pub const DELETE_SAMPLERS: u32 = 44;
    pub const BIND_SAMPLER: u32 = 45;
    pub const SAMPLER_PARAMETER_I: u32 = 46;
    pub const SAMPLER_PARAMETER_F: u32 = 47;

    // -- shaders and programs ---------------------------------------------
    pub const CREATE_SHADER: u32 = 48; // { name, type }
    pub const SHADER_SOURCE: u32 = 49; // { name, len, glsl... }
    pub const COMPILE_SHADER: u32 = 50; // { name }
    pub const CREATE_PROGRAM: u32 = 51; // { name }
    pub const ATTACH_SHADER: u32 = 52; // { program, shader }
    pub const BIND_ATTRIB_LOCATION: u32 = 53; // { program, index, len, name }
    pub const LINK_PROGRAM: u32 = 54; // { program }
    pub const USE_PROGRAM: u32 = 55; // { program }
    pub const DELETE_SHADER: u32 = 56; // { name }
    pub const DELETE_PROGRAM: u32 = 57; // { name }
    /// `{ program, len, name }` -- replies with the location.
    pub const GET_UNIFORM_LOCATION: u32 = 58;
    /// `{ program, len, name }` -- replies with the location.
    pub const GET_ATTRIB_LOCATION: u32 = 59;
    /// `{ name, pname }` -- replies with the value (compile/link status).
    pub const GET_SHADER_IV: u32 = 60;
    pub const GET_PROGRAM_IV: u32 = 61;
    /// `{ name }` -- replies with the info log.
    pub const GET_SHADER_INFO_LOG: u32 = 62;
    pub const GET_PROGRAM_INFO_LOG: u32 = 63;

    // -- uniforms ---------------------------------------------------------
    pub const UNIFORM_1I: u32 = 64; // { location, value }
    pub const UNIFORM_1F: u32 = 65; // { location, value }
    pub const UNIFORM_2F: u32 = 66;
    pub const UNIFORM_3F: u32 = 67;
    pub const UNIFORM_4F: u32 = 68;
    pub const UNIFORM_MATRIX4FV: u32 = 69; // { location, count, transpose, floats... }
    pub const GET_ACTIVE_UNIFORM: u32 = 70; // { program, index } -> size, type, name
    pub const GET_ACTIVE_ATTRIB: u32 = 71; // { program, index } -> size, type, name
    pub const UNIFORM_1IV: u32 = 72;
    pub const UNIFORM_1FV: u32 = 73;
    pub const UNIFORM_2FV: u32 = 74;
    pub const UNIFORM_3FV: u32 = 75;
    pub const UNIFORM_4FV: u32 = 76;
    pub const UNIFORM_2IV: u32 = 77;
    pub const UNIFORM_3IV: u32 = 78;
    pub const UNIFORM_4IV: u32 = 79;

    // -- vertex attributes -------------------------------------------------
    pub const ENABLE_VERTEX_ATTRIB_ARRAY: u32 = 80; // { index }
    pub const DISABLE_VERTEX_ATTRIB_ARRAY: u32 = 81; // { index }
    /// `{ index, size, type, normalized, stride, offset }` -- offset into the
    /// bound buffer. Client-side arrays are uploaded in the guest first, since a
    /// guest pointer means nothing to the host's GL.
    pub const VERTEX_ATTRIB_POINTER: u32 = 82;
    pub const GEN_VERTEX_ARRAYS: u32 = 83;
    pub const DELETE_VERTEX_ARRAYS: u32 = 84;
    pub const BIND_VERTEX_ARRAY: u32 = 85;
    pub const UNIFORM_MATRIX2FV: u32 = 86;
    pub const UNIFORM_MATRIX3FV: u32 = 87;
    pub const DETACH_SHADER: u32 = 88;
    pub const GET_ATTACHED_SHADERS: u32 = 89;
    pub const GET_UNIFORM_BLOCK_INDEX: u32 = 90;
    pub const UNIFORM_BLOCK_BINDING: u32 = 91;
    pub const UNIFORM_2I: u32 = 92;
    pub const UNIFORM_3I: u32 = 93;
    pub const UNIFORM_4I: u32 = 94;

    // -- textures ----------------------------------------------------------
    pub const GEN_TEXTURES: u32 = 96; // { count, names... }
    pub const DELETE_TEXTURES: u32 = 97;
    pub const BIND_TEXTURE: u32 = 98; // { target, name }
    pub const ACTIVE_TEXTURE: u32 = 99; // { unit }
    pub const TEX_PARAMETER_I: u32 = 100; // { target, pname, param }
    /// `{ target, level, internal, w, h, border, format, type, pbo, pointer64,
    ///    len, bytes }`. `pointer64` is only dereferenced as an offset when a
    /// pixel-unpack buffer is bound; otherwise the self-contained blob is used.
    pub const TEX_IMAGE_2D: u32 = 101;
    pub const TEX_SUB_IMAGE_2D: u32 = 102;
    pub const GEN_FRAMEBUFFERS: u32 = 103;
    pub const DELETE_FRAMEBUFFERS: u32 = 104;
    pub const BIND_FRAMEBUFFER: u32 = 105;
    pub const CHECK_FRAMEBUFFER_STATUS: u32 = 106;
    pub const FRAMEBUFFER_TEXTURE_2D: u32 = 107;
    pub const GEN_RENDERBUFFERS: u32 = 108;
    pub const DELETE_RENDERBUFFERS: u32 = 109;
    pub const BIND_RENDERBUFFER: u32 = 110;
    pub const RENDERBUFFER_STORAGE: u32 = 111;

    // -- drawing -----------------------------------------------------------
    pub const DRAW_ARRAYS: u32 = 112; // { mode, first, count }
    // { mode, count, type, offset, client } and, when `client` is set, a blob
    // of `count` index elements. glDrawElements may name a client array
    // rather than an offset into GL_ELEMENT_ARRAY_BUFFER, and the pointer is
    // meaningless on the host: sending it as an offset had the host read its
    // own address space, which is how KHR-GL32.transform_feedback's indexed
    // draws became four copies of vertex zero.
    pub const DRAW_ELEMENTS: u32 = 113;
    pub const FRAMEBUFFER_RENDERBUFFER: u32 = 114;
    pub const GENERATE_MIPMAP: u32 = 115;
    pub const TEX_STORAGE_2D: u32 = 116;
    pub const TEX_STORAGE_3D: u32 = 117;
    pub const TEX_IMAGE_3D: u32 = 118;
    pub const FRAMEBUFFER_TEXTURE_LAYER: u32 = 119;
    pub const DRAW_BUFFERS: u32 = 120;
    pub const VERTEX_ATTRIB_I_POINTER: u32 = 121;
    pub const VERTEX_ATTRIB_DIVISOR: u32 = 122;
    pub const DRAW_ARRAYS_INSTANCED: u32 = 123;
    pub const DRAW_ELEMENTS_INSTANCED: u32 = 124;
    pub const BLIT_FRAMEBUFFER: u32 = 125;
    pub const VERTEX_ATTRIB_4F: u32 = 126;
    pub const VERTEX_ATTRIB_I4I: u32 = 127;

    // -- reading back --------------------------------------------------------
    /// `{ x, y, w, h, format, type, pbo, pointer64 }` -- for client memory,
    /// replies with the first written byte offset and the pixels. The only
    /// command that moves a lot of data back, and slow if called every frame.
    pub const READ_PIXELS: u32 = 128;
    /// `{ name }` -- one of GL's strings, replied as bytes.
    pub const GET_STRING: u32 = 129;
    /// `{ pname }` -- replies with up to four integers.
    pub const GET_INTEGERV: u32 = 130;
    /// `{}` -- replies with the error code, one round trip per call.
    pub const GET_ERROR: u32 = 131;
    pub const GET_STRING_I: u32 = 132;
    pub const BLEND_COLOR: u32 = 133;
    pub const CLEAR_BUFFER_FV: u32 = 134;
    pub const CLEAR_BUFFER_IV: u32 = 135;
    pub const CLEAR_BUFFER_UIV: u32 = 136;
    pub const CLEAR_BUFFER_FI: u32 = 137;
    pub const STENCIL_FUNC: u32 = 138;
    pub const STENCIL_FUNC_SEPARATE: u32 = 139;
    pub const STENCIL_MASK: u32 = 140;
    pub const STENCIL_MASK_SEPARATE: u32 = 141;
    pub const STENCIL_OP: u32 = 142;
    pub const STENCIL_OP_SEPARATE: u32 = 143;
    pub const BLEND_EQUATION_SEPARATE: u32 = 144;
    pub const GET_UNIFORM_FV: u32 = 145;
    pub const GET_UNIFORM_IV: u32 = 146;
    pub const TEX_PARAMETER_F: u32 = 147;
    pub const TEX_PARAMETER_FV: u32 = 148;
    pub const TEX_PARAMETER_IV: u32 = 149;
    pub const GEN_QUERIES: u32 = 150;
    pub const DELETE_QUERIES: u32 = 151;
    pub const IS_QUERY: u32 = 152;
    pub const GEN_TRANSFORM_FEEDBACKS: u32 = 153;
    pub const DELETE_TRANSFORM_FEEDBACKS: u32 = 154;
    pub const BIND_TRANSFORM_FEEDBACK: u32 = 155;
    pub const IS_TRANSFORM_FEEDBACK: u32 = 156;
    pub const PAUSE_TRANSFORM_FEEDBACK: u32 = 157;
    pub const RESUME_TRANSFORM_FEEDBACK: u32 = 158;
    /// `{ program, index }` -> size, type, name -- the transform feedback
    /// varying's description. Shaped exactly like `GET_ACTIVE_UNIFORM`.
    pub const GET_TRANSFORM_FEEDBACK_VARYING: u32 = 159;
    /// `{ shader }` -> the source string the guest last handed over.
    pub const GET_SHADER_SOURCE: u32 = 160;
    /// `{ program, index }` -> name, for one active uniform.
    pub const GET_ACTIVE_UNIFORM_NAME: u32 = 161;
    /// `{ program, index }` -> name, for one active uniform block.
    pub const GET_ACTIVE_UNIFORM_BLOCK_NAME: u32 = 162;
    /// `{ program, index, pname }` -> the integers that query yields.
    pub const GET_ACTIVE_UNIFORM_BLOCKIV: u32 = 163;
    /// `{ program, pname, indices }` -> one integer per index.
    pub const GET_ACTIVE_UNIFORMSIV: u32 = 164;
    /// `{ program, count, names }` -> one index per name. The names arrive as
    /// one blob of NUL-terminated strings, which is how the guest already
    /// holds them.
    pub const GET_UNIFORM_INDICES: u32 = 165;

    /* Fixed function, which a compatibility profile has and the generated
     * GL 4.4 core set does not. These are hand-written rather than generated
     * because tools/gen_gl44_manifest.py builds its list from glcorearb.h --
     * the core-profile header, which by definition omits every one of them.
     *
     * The whole set exists to run programs written before shaders: glxgears
     * needs exactly these twenty and nothing else. Immediate mode is as
     * chatty as it looks, but glxgears compiles its gears into display lists,
     * so glBegin and glVertex3f run once at startup and each frame is a
     * handful of matrix calls and three glCallList. */
    pub const MATRIX_MODE: u32 = 166;
    pub const LOAD_IDENTITY: u32 = 167;
    pub const PUSH_MATRIX: u32 = 168;
    pub const POP_MATRIX: u32 = 169;
    pub const TRANSLATEF: u32 = 170;
    pub const ROTATEF: u32 = 171;
    pub const FRUSTUM: u32 = 172;
    pub const BEGIN: u32 = 173;
    pub const END: u32 = 174;
    pub const VERTEX3F: u32 = 175;
    pub const NORMAL3F: u32 = 176;
    pub const SHADE_MODEL: u32 = 177;
    pub const LIGHTFV: u32 = 178;
    pub const MATERIALFV: u32 = 179;
    pub const GEN_LISTS: u32 = 180;
    pub const NEW_LIST: u32 = 181;
    pub const END_LIST: u32 = 182;
    pub const CALL_LIST: u32 = 183;
    pub const DELETE_LISTS: u32 = 184;
}

/// Writes commands into a batch. For the host's tests: the guest half is C,
/// and this builds the same bytes to check the executor against.
#[derive(Default)]
pub struct Batch {
    bytes: Vec<u8>,
}

impl Batch {
    pub fn new() -> Batch {
        Batch::default()
    }

    pub fn cmd(&mut self, opcode: u32, args: &[u32]) -> &mut Batch {
        self.raw(opcode, args, &[])
    }

    /// Split a 64-bit ABI value into the two little-endian wire words used by
    /// GLintptr, GLsizeiptr, GLint64, GLuint64, and GLdouble payloads.
    pub fn words64(value: u64) -> [u32; 2] {
        [value as u32, (value >> 32) as u32]
    }

    /// A command with trailing bytes (shader source, buffer contents). Padded
    /// to four so the next command stays aligned; the payload's own length word
    /// says how much is real.
    pub fn raw(&mut self, opcode: u32, args: &[u32], tail: &[u8]) -> &mut Batch {
        let padded = tail.len().div_ceil(4) * 4;
        let len = 8 + args.len() * 4 + padded;
        self.bytes.extend_from_slice(&opcode.to_le_bytes());
        self.bytes.extend_from_slice(&(len as u32).to_le_bytes());
        for a in args {
            self.bytes.extend_from_slice(&a.to_le_bytes());
        }
        self.bytes.extend_from_slice(tail);
        self.bytes.resize(self.bytes.len() + padded - tail.len(), 0);
        self
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Reads arguments out of one command's body.
pub struct Args<'a> {
    body: &'a [u8],
    at: usize,
}

impl<'a> Args<'a> {
    pub fn new(body: &'a [u8]) -> Args<'a> {
        Args { body, at: 0 }
    }

    pub fn u32(&mut self) -> Option<u32> {
        let end = self.at + 4;
        if end > self.body.len() {
            return None;
        }
        let v = u32::from_le_bytes(self.body[self.at..end].try_into().ok()?);
        self.at = end;
        Some(v)
    }

    pub fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }

    pub fn f32(&mut self) -> Option<f32> {
        self.u32().map(f32::from_bits)
    }

    pub fn u64(&mut self) -> Option<u64> {
        let low = self.u32()? as u64;
        let high = self.u32()? as u64;
        Some(low | (high << 32))
    }

    pub fn i64(&mut self) -> Option<i64> {
        self.u64().map(|value| value as i64)
    }

    pub fn f64(&mut self) -> Option<f64> {
        self.u64().map(f64::from_bits)
    }

    /// A length word then that many bytes, which is how every payload is
    /// carried.
    pub fn blob(&mut self) -> Option<&'a [u8]> {
        let len = self.u32()? as usize;
        let end = self.at + len;
        if end > self.body.len() {
            return None;
        }
        let out = &self.body[self.at..end];
        self.at = end;
        Some(out)
    }

    pub fn rest(&mut self) -> &'a [u8] {
        let out = &self.body[self.at.min(self.body.len())..];
        self.at = self.body.len();
        out
    }
}

/// Walks a batch.
pub struct Commands<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Commands<'a> {
    pub fn new(bytes: &'a [u8]) -> Commands<'a> {
        Commands { bytes, at: 0 }
    }

    #[allow(clippy::type_complexity)]
    pub fn next(&mut self) -> Option<Result<(u32, &'a [u8]), ()>> {
        if self.at >= self.bytes.len() {
            return None;
        }
        if self.at + 8 > self.bytes.len() {
            return Some(Err(()));
        }
        let opcode = u32::from_le_bytes(self.bytes[self.at..self.at + 4].try_into().ok()?);
        let len =
            u32::from_le_bytes(self.bytes[self.at + 4..self.at + 8].try_into().ok()?) as usize;
        if len < 8 || self.at + len > self.bytes.len() {
            return Some(Err(()));
        }
        let body = &self.bytes[self.at + 8..self.at + len];
        self.at += len;
        Some(Ok((opcode, body)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_round_trips() {
        let mut b = Batch::new();
        b.cmd(op::CLEAR, &[0x4000]);
        b.cmd(op::DRAW_ARRAYS, &[4, 0, 3]);
        let mut c = Commands::new(b.bytes());
        let (o, body) = c.next().unwrap().unwrap();
        assert_eq!(o, op::CLEAR);
        assert_eq!(Args::new(body).u32(), Some(0x4000));
        let (o, body) = c.next().unwrap().unwrap();
        assert_eq!(o, op::DRAW_ARRAYS);
        let mut a = Args::new(body);
        assert_eq!((a.u32(), a.u32(), a.u32()), (Some(4), Some(0), Some(3)));
        assert!(c.next().is_none());
    }

    #[test]
    fn a_payload_is_padded_so_the_next_command_stays_aligned() {
        let mut b = Batch::new();
        // Five bytes of source pads to eight.
        b.raw(op::SHADER_SOURCE, &[1, 5], b"hello");
        b.cmd(op::COMPILE_SHADER, &[1]);
        assert_eq!(b.bytes().len() % 4, 0);
        let mut c = Commands::new(b.bytes());
        let (_, body) = c.next().unwrap().unwrap();
        let mut a = Args::new(body);
        assert_eq!(a.u32(), Some(1));
        assert_eq!(a.blob(), Some(&b"hello"[..]));
        let (o, _) = c.next().unwrap().unwrap();
        assert_eq!(o, op::COMPILE_SHADER, "the next command is found");
    }

    #[test]
    fn a_truncated_batch_is_an_error_rather_than_a_guess() {
        let mut b = Batch::new();
        b.cmd(op::DRAW_ARRAYS, &[4, 0, 3]);
        let full = b.bytes().to_vec();
        for cut in 1..full.len() {
            let mut c = Commands::new(&full[cut..]);
            // Cutting from the front leaves a bad header or a bad length.
            assert!(
                !matches!(c.next(), Some(Ok(_))) || cut % 4 != 0,
                "cut {cut} was accepted"
            );
        }
    }

    #[test]
    fn a_blob_longer_than_its_command_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&op::BUFFER_DATA.to_le_bytes());
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // target
        bytes.extend_from_slice(&999u32.to_le_bytes()); // a length that lies
        let (_, body) = Commands::new(&bytes).next().unwrap().unwrap();
        let mut a = Args::new(body);
        a.u32();
        assert_eq!(a.blob(), None);
    }

    #[test]
    fn the_header_is_thirty_two_bytes() {
        assert_eq!(std::mem::size_of::<Header>(), HEADER as usize);
    }

    #[test]
    fn sixty_four_bit_wire_values_are_little_endian_and_exact() {
        let bits = 0xfedc_ba98_7654_3210;
        assert_eq!(Batch::words64(bits), [0x7654_3210, 0xfedc_ba98]);
        let words = Batch::words64(bits);
        let bytes: Vec<u8> = words.into_iter().flat_map(u32::to_le_bytes).collect();
        let mut args = Args::new(&bytes);
        assert_eq!(args.u64(), Some(bits));

        let number = -1234.5_f64;
        let words = Batch::words64(number.to_bits());
        let bytes: Vec<u8> = words.into_iter().flat_map(u32::to_le_bytes).collect();
        assert_eq!(Args::new(&bytes).f64(), Some(number));
    }
}
