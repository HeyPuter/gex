//! The vtest protocol, as a stream state machine.
//!
//! vtest is the transport Mesa's virgl driver uses when there is no DRM
//! device: the driver connects to a UNIX socket and speaks a small
//! request/response protocol. gex serves that socket, so the guest runs stock
//! Mesa -- no gex-specific driver, no patched loader -- and its Gallium
//! command stream arrives here to be executed by virglrenderer on the host's
//! real OpenGL.
//!
//! That is the whole reason for choosing this transport first: the guest side
//! is code nobody has to write. The DRM render node is faster and is the next
//! step, but it needs `DRM_IOCTL_VIRTGPU_*` and a shared-memory path before a
//! single triangle appears; this needs nothing.
//!
//! # Protocol version
//!
//! Version 0 is negotiated deliberately. From version 2 the driver expects
//! `RESOURCE_CREATE2` to hand back a shared-memory descriptor over
//! `SCM_RIGHTS`, and every transfer then becomes a memcpy into that mapping.
//! Version 0 puts transfer payloads in the stream instead. It copies more, and
//! it is the reason this path will never be the fast one -- but it needs no
//! descriptor passing at all, and correctness comes first.
//!
//! # Framing
//!
//! Every command is a two-word header -- a length and a command id -- then a
//! payload. The length is in dwords for `SUBMIT_CMD` and in bytes for
//! `CREATE_RENDERER`, and most other commands ignore it entirely and read a
//! fixed-size struct. That is not a description of a good protocol, it is a
//! description of this one, and the sizes below follow the reference server
//! rather than the header file's constants.

use crate::Virgl;
use std::sync::OnceLock;

/// Whether to narrate the command stream.
///
/// A protocol this stateful fails by crashing somewhere inside the renderer
/// several commands after the mistake, so knowing which command was last
/// accepted is most of the diagnosis.
fn tracing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GEX_VIRGL_TRACE").is_some())
}

fn name(cmd: u32) -> &'static str {
    match cmd {
        VCMD_GET_CAPS => "GET_CAPS",
        VCMD_RESOURCE_CREATE => "RESOURCE_CREATE",
        VCMD_RESOURCE_UNREF => "RESOURCE_UNREF",
        VCMD_TRANSFER_GET => "TRANSFER_GET",
        VCMD_TRANSFER_PUT => "TRANSFER_PUT",
        VCMD_SUBMIT_CMD => "SUBMIT_CMD",
        VCMD_RESOURCE_BUSY_WAIT => "RESOURCE_BUSY_WAIT",
        VCMD_CREATE_RENDERER => "CREATE_RENDERER",
        VCMD_GET_CAPS2 => "GET_CAPS2",
        VCMD_PING_PROTOCOL_VERSION => "PING_PROTOCOL_VERSION",
        VCMD_PROTOCOL_VERSION => "PROTOCOL_VERSION",
        VCMD_RESOURCE_CREATE2 => "RESOURCE_CREATE2",
        VCMD_TRANSFER_GET2 => "TRANSFER_GET2",
        VCMD_TRANSFER_PUT2 => "TRANSFER_PUT2",
        _ => "?",
    }
}

pub const VCMD_GET_CAPS: u32 = 1;
pub const VCMD_RESOURCE_CREATE: u32 = 2;
pub const VCMD_RESOURCE_UNREF: u32 = 3;
pub const VCMD_TRANSFER_GET: u32 = 4;
pub const VCMD_TRANSFER_PUT: u32 = 5;
pub const VCMD_SUBMIT_CMD: u32 = 6;
pub const VCMD_RESOURCE_BUSY_WAIT: u32 = 7;
pub const VCMD_CREATE_RENDERER: u32 = 8;
pub const VCMD_GET_CAPS2: u32 = 9;
pub const VCMD_PING_PROTOCOL_VERSION: u32 = 10;
pub const VCMD_PROTOCOL_VERSION: u32 = 11;
pub const VCMD_RESOURCE_CREATE2: u32 = 12;
pub const VCMD_TRANSFER_GET2: u32 = 13;
pub const VCMD_TRANSFER_PUT2: u32 = 14;

/// The version this server speaks. See the note above on why it is not 2.
const SERVED_PROTOCOL_VERSION: u32 = 0;

const HDR: usize = 8;

fn dw(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]])
}

/// One connected guest client.
pub struct Session {
    /// Bytes read from the guest that do not yet form a whole command.
    input: Vec<u8>,
    /// The renderer context this client owns, once it has asked for one.
    ctx_id: Option<u32>,
    /// Set when the client has said something this server cannot answer. The
    /// connection is then dead: a stream protocol has no way to resynchronise
    /// after a command of unknown length, and pretending otherwise turns one
    /// bad command into a stream of garbage.
    pub failed: Option<String>,
}

impl Default for Session {
    fn default() -> Session {
        Session::new()
    }
}

impl Session {
    pub fn new() -> Session {
        Session {
            input: Vec::new(),
            ctx_id: None,
            failed: None,
        }
    }

    /// Bytes have arrived from the guest.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.input.extend_from_slice(bytes);
    }

    /// Run every complete command in the buffer; return what to send back.
    pub fn run(&mut self, virgl: &mut Virgl) -> Vec<u8> {
        let mut out = Vec::new();
        while self.failed.is_none() {
            let Some(len) = self.command_len() else { break };
            if self.input.len() < HDR + len {
                break;
            }
            // Taken out of the buffer before dispatch, so an early return
            // cannot leave a half-consumed command behind.
            let body: Vec<u8> = self.input[HDR..HDR + len].to_vec();
            let cmd = dw(&self.input, 1);
            let length = dw(&self.input, 0);
            self.input.drain(..HDR + len);
            if tracing() {
                eprintln!("[virgl] {} len={length} payload={}", name(cmd), body.len());
            }
            if let Err(e) = self.dispatch(virgl, cmd, length, &body, &mut out) {
                self.failed = Some(e);
            }
        }
        out
    }

    /// How many payload bytes the command at the head of the buffer has, or
    /// `None` if the header is not all here yet.
    fn command_len(&mut self) -> Option<usize> {
        if self.input.len() < HDR {
            return None;
        }
        let length = dw(&self.input, 0) as usize;
        let cmd = dw(&self.input, 1);
        Some(match cmd {
            VCMD_PING_PROTOCOL_VERSION | VCMD_GET_CAPS | VCMD_GET_CAPS2 => 0,
            VCMD_PROTOCOL_VERSION | VCMD_RESOURCE_UNREF => 4,
            VCMD_RESOURCE_BUSY_WAIT => 8,
            VCMD_RESOURCE_CREATE => 40,
            VCMD_RESOURCE_CREATE2 => 44,
            VCMD_TRANSFER_GET => 44,
            VCMD_TRANSFER_GET2 => 40,
            // The payload carries its own size, in the last word of the
            // header, so the whole thing has to be here before the size is
            // known -- hence the second length check after this returns.
            VCMD_TRANSFER_PUT => {
                if self.input.len() < HDR + 44 {
                    return None;
                }
                44 + dw(&self.input[HDR..], 10) as usize
            }
            VCMD_TRANSFER_PUT2 => {
                if self.input.len() < HDR + 40 {
                    return None;
                }
                40 + dw(&self.input[HDR..], 8) as usize
            }
            VCMD_SUBMIT_CMD => length * 4,
            // The only command whose length is in bytes.
            VCMD_CREATE_RENDERER => length,
            _ => {
                self.failed = Some(format!("unknown vtest command {cmd}"));
                return None;
            }
        })
    }

    fn dispatch(
        &mut self,
        virgl: &mut Virgl,
        cmd: u32,
        _length: u32,
        body: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        match cmd {
            VCMD_CREATE_RENDERER => {
                let name = String::from_utf8_lossy(body).trim_end_matches('\0').to_string();
                self.ctx_id = Some(virgl.context_create(&name)?);
            }

            VCMD_PING_PROTOCOL_VERSION => {
                // A bare header. The client uses whether this is answered at
                // all to decide the server is new enough to ask about
                // versions, so the reply carries no payload.
                push_hdr(out, 0, VCMD_PING_PROTOCOL_VERSION);
            }

            VCMD_PROTOCOL_VERSION => {
                let asked = dw(body, 0);
                let version = asked.min(SERVED_PROTOCOL_VERSION);
                push_hdr(out, 1, VCMD_PROTOCOL_VERSION);
                out.extend_from_slice(&version.to_le_bytes());
            }

            VCMD_GET_CAPS | VCMD_GET_CAPS2 => {
                let set = if cmd == VCMD_GET_CAPS { 1 } else { 2 };
                let caps = virgl.caps(set);
                // The reference sends `size + 1` as the length. It is not a
                // count of anything -- the client reads `size` bytes -- but it
                // is what the client expects to see.
                push_hdr(out, caps.len() as u32 + 1, set);
                out.extend_from_slice(&caps);
            }

            VCMD_RESOURCE_CREATE | VCMD_RESOURCE_CREATE2 => {
                let ctx = self.ctx()?;
                // create2 adds a shared-memory size at the end. At protocol
                // version 0 the client never sends it, and if it somehow did,
                // ignoring it means the transfers still work -- just through
                // the stream, which is where they already are.
                virgl.resource_create(ctx, body)?;
            }

            VCMD_RESOURCE_UNREF => {
                let ctx = self.ctx()?;
                virgl.resource_unref(ctx, dw(body, 0));
            }

            VCMD_SUBMIT_CMD => {
                let ctx = self.ctx()?;
                virgl.submit_cmd(ctx, body)?;
            }

            VCMD_TRANSFER_GET | VCMD_TRANSFER_GET2 => {
                let ctx = self.ctx()?;
                let (args, size) = transfer_args(cmd, body);
                let data = virgl.transfer_get(ctx, &args, size)?;
                // No header: the client reads exactly the bytes it asked for.
                out.extend_from_slice(&data);
            }

            VCMD_TRANSFER_PUT | VCMD_TRANSFER_PUT2 => {
                let ctx = self.ctx()?;
                let (args, size) = transfer_args(cmd, body);
                let head = if cmd == VCMD_TRANSFER_PUT { 44 } else { 40 };
                virgl.transfer_put(ctx, &args, &body[head..head + size])?;
            }

            VCMD_RESOURCE_BUSY_WAIT => {
                // Everything this server does is synchronous -- a command has
                // finished by the time its reply is written -- so nothing is
                // ever busy. The client still has to be told so.
                virgl.poll();
                push_hdr(out, 1, VCMD_RESOURCE_BUSY_WAIT);
                out.extend_from_slice(&0u32.to_le_bytes());
            }

            other => return Err(format!("unhandled vtest command {other}")),
        }
        Ok(())
    }

    fn ctx(&self) -> Result<u32, String> {
        self.ctx_id
            .ok_or_else(|| "the client sent a command before creating a renderer".to_string())
    }
}

fn push_hdr(out: &mut Vec<u8>, len: u32, cmd: u32) {
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&cmd.to_le_bytes());
}

/// What a transfer is asking for. The two shapes differ only in whether the
/// strides are given or left for virglrenderer to work out.
pub struct Transfer {
    pub handle: u32,
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
    pub offset: u64,
    pub b: crate::ffi::VirglBox,
}

fn transfer_args(cmd: u32, body: &[u8]) -> (Transfer, usize) {
    if cmd == VCMD_TRANSFER_GET || cmd == VCMD_TRANSFER_PUT {
        (
            Transfer {
                handle: dw(body, 0),
                level: dw(body, 1),
                stride: dw(body, 2),
                layer_stride: dw(body, 3),
                offset: 0,
                b: crate::ffi::VirglBox {
                    x: dw(body, 4),
                    y: dw(body, 5),
                    z: dw(body, 6),
                    w: dw(body, 7),
                    h: dw(body, 8),
                    d: dw(body, 9),
                },
            },
            dw(body, 10) as usize,
        )
    } else {
        (
            Transfer {
                handle: dw(body, 0),
                level: dw(body, 1),
                stride: 0,
                layer_stride: 0,
                offset: dw(body, 9) as u64,
                b: crate::ffi::VirglBox {
                    x: dw(body, 2),
                    y: dw(body, 3),
                    z: dw(body, 4),
                    w: dw(body, 5),
                    h: dw(body, 6),
                    d: dw(body, 7),
                },
            },
            dw(body, 8) as usize,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The framing is the part that has to be right before anything else can
    /// be, and it can be checked without a renderer: a command split across
    /// two reads must not be seen until all of it has arrived.
    #[test]
    fn a_command_is_not_seen_until_it_is_whole() {
        let mut s = Session::new();
        let mut msg = Vec::new();
        push_hdr(&mut msg, 1, VCMD_PROTOCOL_VERSION);
        msg.extend_from_slice(&2u32.to_le_bytes());

        s.feed(&msg[..6]);
        assert_eq!(s.command_len(), None, "half a header is not a command");
        s.feed(&msg[6..8]);
        assert_eq!(s.command_len(), Some(4));
        assert!(s.input.len() < HDR + 4);
    }

    #[test]
    fn a_transfer_put_carries_its_own_length() {
        let mut s = Session::new();
        let mut msg = Vec::new();
        push_hdr(&mut msg, 11, VCMD_TRANSFER_PUT);
        for i in 0..11u32 {
            // The last word of the header is the payload size.
            msg.extend_from_slice(&(if i == 10 { 16u32 } else { 0 }).to_le_bytes());
        }
        s.feed(&msg);
        assert_eq!(s.command_len(), Some(44 + 16));
    }

    #[test]
    fn an_unknown_command_stops_the_session() {
        let mut s = Session::new();
        let mut msg = Vec::new();
        push_hdr(&mut msg, 0, 999);
        s.feed(&msg);
        assert_eq!(s.command_len(), None);
        assert!(s.failed.is_some());
    }
}
