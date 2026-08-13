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
    pub reserved: u32,
}

/// Size of [`Header`] in bytes.
pub const HEADER: u32 = 32;

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
}

/// Opcodes, numbered in blocks so adding to one group does not renumber
/// another and break a guest built against an older header.
pub mod op {
    // -- the device itself ------------------------------------------------
    /// `{}` -- make a context and make it current. Replies: vendor, renderer,
    /// version.
    pub const CONTEXT_CREATE: u32 = 1;
    /// `{ width, height }` -- the offscreen surface everything draws to.
    pub const SURFACE_RESIZE: u32 = 2;
    /// `{}` -- finish everything and reply when it is done.
    pub const FINISH: u32 = 3;

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

    // -- buffers ----------------------------------------------------------
    pub const GEN_BUFFERS: u32 = 32; // { count, names... }
    pub const DELETE_BUFFERS: u32 = 33; // { count, names... }
    pub const BIND_BUFFER: u32 = 34; // { target, name }
    pub const BUFFER_DATA: u32 = 35; // { target, usage, len, bytes... }
    pub const BUFFER_SUB_DATA: u32 = 36; // { target, offset, len, bytes... }

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

    // -- vertex attributes -------------------------------------------------
    pub const ENABLE_VERTEX_ATTRIB_ARRAY: u32 = 80; // { index }
    pub const DISABLE_VERTEX_ATTRIB_ARRAY: u32 = 81; // { index }
    /// `{ index, size, type, normalized, stride, offset }` -- offset into the
    /// bound buffer. Client-side arrays are uploaded in the guest first, since a
    /// guest pointer means nothing to the host's GL.
    pub const VERTEX_ATTRIB_POINTER: u32 = 82;

    // -- textures ----------------------------------------------------------
    pub const GEN_TEXTURES: u32 = 96; // { count, names... }
    pub const DELETE_TEXTURES: u32 = 97;
    pub const BIND_TEXTURE: u32 = 98; // { target, name }
    pub const ACTIVE_TEXTURE: u32 = 99; // { unit }
    pub const TEX_PARAMETER_I: u32 = 100; // { target, pname, param }
    /// `{ target, level, internal, w, h, border, format, type, len, bytes }`
    pub const TEX_IMAGE_2D: u32 = 101;
    pub const TEX_SUB_IMAGE_2D: u32 = 102;

    // -- drawing -----------------------------------------------------------
    pub const DRAW_ARRAYS: u32 = 112; // { mode, first, count }
    pub const DRAW_ELEMENTS: u32 = 113; // { mode, count, type, offset }

    // -- reading back --------------------------------------------------------
    /// `{ x, y, w, h, format, type }` -- replies with the pixels. The only
    /// command that moves a lot of data back, and slow if called every frame.
    pub const READ_PIXELS: u32 = 128;
    /// `{ name }` -- one of GL's strings, replied as bytes.
    pub const GET_STRING: u32 = 129;
    /// `{ pname }` -- replies with up to four integers.
    pub const GET_INTEGERV: u32 = 130;
    /// `{}` -- replies with the error code, one round trip per call.
    pub const GET_ERROR: u32 = 131;
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
}
