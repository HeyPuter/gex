//! opengl32's Unix side (docs/nt-layer.md §6a.2): Wine's PE opengl32 makes a
//! unix call for every GL and WGL function; each is encoded here onto gex's
//! GL wire protocol (`crates/gex-gl`) and run by the host's GL device,
//! `/dev/gex-gl`, the one Linux guests' GL uses.
//!
//! The command ring is guest memory the NT layer allocates; a submit is the
//! device's ioctl, made directly. Object names are chosen here, as the wire
//! protocol lets a guest do. Client-side vertex arrays are copied into
//! buffers of the layer's own just before each draw, as guest/gl/gexgl.c
//! does for Linux guests.
//!
//! Frames: SwapBuffers reads the default framebuffer back into the window's
//! surface (§6a.1), so GL and GDI output end up in the same place, and
//! publishes it to the display (natively, `gexc run --wayland-out PREFIX`
//! writes composited PPMs).
//!
//! Scope is what Noita and SDL use (its census is in the S12 notes); any
//! other GL function fails loudly by name. All of the process's contexts
//! share the one host context, as if created with wglShareLists.

pub mod generated;
pub mod table;

use super::gdi::{self, Obj};
use super::user::window::Rect;
use super::{st, Abi, Call, Status, R};
use crate::vfs::FileHandle;
use gex_gl::proto::{self, op};
use std::collections::HashMap;
use std::sync::Arc;

const CMD_CAPACITY: u32 = 8 << 20;
const REPLY_CAPACITY: u32 = 16 << 20;
const HEADER: u64 = 32;
const GEX_GL_SUBMIT: u64 = 0xc008_6701;
/// [`GEX_GL_SUBMIT`] without waiting for the batch to run: the device takes
/// a copy of the ring, so it may be refilled at once, and the next submit of
/// any kind waits for this one first -- one batch in flight at most. For a
/// batch with nothing to answer (a frame's last, ending in `PRESENT`). A
/// device without it says so (no answer), and the batch is submitted the
/// waiting way.
const GEX_GL_SUBMIT_ASYNC: u64 = 0xc008_6704;

/// A params struct: where each field of a unix call's arguments is, for the
/// process's ABI.
struct Params {
    at: u64,
    offs: Vec<u64>,
    /// The field kinds (`table::FUNCS`).
    sig: &'static str,
}

fn layout(sig: &str, abi: Abi) -> Vec<u64> {
    let ps = abi.ptr_size();
    let mut off = 0u64;
    sig.bytes()
        .map(|k| {
            let s = match k {
                b'b' => 1,
                b'h' => 2,
                b'i' | b'f' => 4,
                b'd' | b'q' => 8,
                _ => ps,
            };
            off = off.div_ceil(s) * s;
            let o = off;
            off += s;
            o
        })
        .collect()
}

impl Params {
    fn u32(&self, c: &Call<'_>, i: usize) -> R<u32> {
        c.g.u32(self.at + self.offs[i])
    }
    fn i32(&self, c: &Call<'_>, i: usize) -> R<i32> {
        Ok(self.u32(c, i)? as i32)
    }
    fn u8(&self, c: &Call<'_>, i: usize) -> R<u8> {
        c.g.u8(self.at + self.offs[i])
    }
    fn f32(&self, c: &Call<'_>, i: usize) -> R<f32> {
        Ok(f32::from_bits(self.u32(c, i)?))
    }
    fn f64(&self, c: &Call<'_>, i: usize) -> R<f64> {
        Ok(f64::from_bits(c.g.u64(self.at + self.offs[i])?))
    }
    fn ptr(&self, c: &Call<'_>, i: usize) -> R<u64> {
        c.g.ptr(self.at + self.offs[i])
    }
    /// A pointer-sized signed integer (GLintptr, GLsizeiptr).
    fn isize(&self, c: &Call<'_>, i: usize) -> R<i64> {
        Ok(match c.p.abi {
            Abi::P64 => c.g.u64(self.at + self.offs[i])? as i64,
            Abi::P32 => c.g.u32(self.at + self.offs[i])? as i32 as i64,
        })
    }
    fn set_u32(&self, c: &mut Call<'_>, i: usize, v: u32) -> R<()> {
        c.g.w32(self.at + self.offs[i], v)
    }
    #[allow(dead_code)]
    fn set_u8(&self, c: &mut Call<'_>, i: usize, v: u8) -> R<()> {
        c.g.w8(self.at + self.offs[i], v)
    }
    fn set_ptr(&self, c: &mut Call<'_>, i: usize, v: u64) -> R<()> {
        c.g.wptr(self.at + self.offs[i], v)
    }
    /// The last field: a call's result.
    fn ret(&self) -> usize {
        self.offs.len() - 1
    }
}

/// Unpack/pack pixel storage, for sizing pixel transfers.
#[derive(Clone, Copy)]
struct PixelStore {
    alignment: u32,
    row_length: u32,
    skip_pixels: u32,
    skip_rows: u32,
    image_height: u32,
    skip_images: u32,
}

impl Default for PixelStore {
    fn default() -> Self {
        PixelStore { alignment: 4, row_length: 0, skip_pixels: 0, skip_rows: 0, image_height: 0, skip_images: 0 }
    }
}

/// A client-side vertex array: glVertexPointer and friends.
#[derive(Clone, Copy, Default)]
struct Array {
    enabled: bool,
    size: i32,
    ty: u32,
    stride: i32,
    /// A client pointer, or an offset into `buffer`.
    pointer: u64,
    /// The GL_ARRAY_BUFFER bound when the pointer was set (0: client memory).
    buffer: u32,
    /// Our upload buffer for client memory.
    scratch: u32,
}

const VERTEX: usize = 0;
const COLOR: usize = 1;
/// Texture coordinates for units 0..8 are `TEXCOORD + unit`.
const TEXCOORD: usize = 2;
const ARRAYS: usize = TEXCOORD + 8;
/// The other fixed-function arrays, after the texture coordinates.
const NORMAL: usize = ARRAYS;
const SECONDARY: usize = ARRAYS + 1;
const FOG: usize = ARRAYS + 2;
const FIXED_ARRAYS: usize = ARRAYS + 3;
/// Generic vertex attributes kept (GL_MAX_VERTEX_ATTRIBS is at least 16).
const ATTRIBS: usize = 16;

/// A generic vertex attribute array (glVertexAttribPointer and its integer
/// and double forms).
#[derive(Clone, Copy, Default)]
struct Attrib {
    enabled: bool,
    size: i32,
    ty: u32,
    normalized: bool,
    stride: i32,
    pointer: u64,
    buffer: u32,
    scratch: u32,
    /// 0 glVertexAttribPointer, 1 the I form, 2 the L form.
    kind: u8,
    divisor: u32,
}

/// A buffer mapped into guest memory (glMapBuffer*): a copy, written back
/// at unmap or flush.
#[derive(Clone, Copy)]
struct Mapping {
    target: u32,
    addr: u64,
    offset: u64,
    len: u64,
    write: bool,
    explicit_flush: bool,
}

/// A guest context (`struct opengl_client_context` in guest memory).
struct Ctx {
    hdc: u32,
    major: i32,
    minor: i32,
    synced: bool,
}

pub struct State {
    ring: u64,
    tail: u32,
    dev: Option<Arc<dyn FileHandle>>,
    host_made: bool,
    contexts: HashMap<u64, Ctx>,
    /// tid → current guest context.
    current: HashMap<u32, u64>,
    /// hwnd → pixel format.
    formats: HashMap<u32, i32>,
    /// Interned strings handed to the guest.
    strings: HashMap<Vec<u8>, u64>,
    /// Per GL function since the last [`take_stats`]: calls and nanoseconds
    /// (the round trips they waited for included); and the submits.
    /// (calls, ns, submits it caused).
    stats: HashMap<&'static str, (u64, u64, u64)>,
    submits: (u64, u64, u64),
    /// The GL function being served, which a submit is charged to.
    serving: &'static str,
    stats_frame: u64,
    next_name: u32,
    /// The default framebuffer's size.
    surface: (i32, i32),
    /// The top-level window the host's frames were last attached to
    /// (`op::SURFACE_ATTACH`), when the display takes them directly.
    attached: u32,
    unpack: PixelStore,
    pack: PixelStore,
    arrays: [Array; FIXED_ARRAYS],
    attribs: [Attrib; ATTRIBS],
    /// Every buffer target's binding (GL_ARRAY_BUFFER's is `array_buffer`
    /// too), for mapping.
    bound: HashMap<u32, u32>,
    /// Mapped buffers by name, and the copies' allocations by name for reuse.
    maps: HashMap<u32, Mapping>,
    map_memory: HashMap<u32, (u64, u64)>,
    /// Our upload buffer for client-memory indices.
    element_scratch: u32,
    client_texture: usize,
    array_buffer: u32,
    /// GL_PIXEL_UNPACK_BUFFER's binding: pixel data is an offset into it.
    unpack_buffer: u32,
    element_buffer: u32,
    read_fbo: u32,
    swap_interval: i32,
    frame: u64,
    version: String,
    extensions: String,
}

impl Default for State {
    fn default() -> Self {
        State {
            ring: 0,
            tail: 0,
            dev: None,
            host_made: false,
            contexts: HashMap::new(),
            current: HashMap::new(),
            formats: HashMap::new(),
            strings: HashMap::new(),
            stats: HashMap::new(),
            submits: (0, 0, 0),
            serving: "",
            stats_frame: 0,
            next_name: 1,
            surface: (0, 0),
            attached: 0,
            unpack: PixelStore::default(),
            pack: PixelStore::default(),
            arrays: [Array::default(); FIXED_ARRAYS],
            attribs: [Attrib::default(); ATTRIBS],
            bound: HashMap::new(),
            maps: HashMap::new(),
            map_memory: HashMap::new(),
            element_scratch: 0,
            client_texture: 0,
            array_buffer: 0,
            unpack_buffer: 0,
            element_buffer: 0,
            read_fbo: 0,
            swap_interval: 1,
            frame: 0,
            version: String::new(),
            extensions: String::new(),
        }
    }
}

// ---------------------------------------------------------------- the ring

fn open_device(c: &mut Call<'_>) -> R<()> {
    let mut s = c.p.gl.lock().unwrap();
    if s.dev.is_some() {
        return Ok(());
    }
    drop(s);
    let dev = c.kernel().vfs.open("/dev/gex-gl", 2, 0).ok();
    let Some(dev) = dev else {
        return Err(c.unimplemented("OpenGL without a GL device (run gexc with --gl)"));
    };
    let ring = super::sysinfo::alloc_private(c, HEADER + CMD_CAPACITY as u64 + REPLY_CAPACITY as u64, "[gex-gl ring]")?;
    let words = [proto::MAGIC, CMD_CAPACITY, 0, REPLY_CAPACITY, 0, 0, 0, proto::PROTOCOL_VERSION];
    for (i, w) in words.iter().enumerate() {
        c.g.w32(ring + 4 * i as u64, *w)?;
    }
    s = c.p.gl.lock().unwrap();
    s.ring = ring;
    s.dev = Some(dev);
    Ok(())
}

/// Append one command: `words` of arguments and a `tail` blob (its length
/// word included by the caller where the protocol has one).
fn cmd(c: &mut Call<'_>, opcode: u32, words: &[u32], tail: &[u8]) -> R<()> {
    open_device(c)?;
    let padded = tail.len().div_ceil(4) * 4;
    let len = 8 + 4 * words.len() + padded;
    if len as u32 > CMD_CAPACITY {
        return Err(c.unimplemented(&format!("a GL command of {len} bytes")));
    }
    let tail_now = c.p.gl.lock().unwrap().tail;
    if tail_now as usize + len > CMD_CAPACITY as usize {
        flush(c)?;
    }
    let (ring, at) = {
        let mut s = c.p.gl.lock().unwrap();
        let at = s.tail;
        s.tail += len as u32;
        (s.ring, at)
    };
    let base = ring + HEADER + at as u64;
    let mut buf = Vec::with_capacity(len);
    buf.extend_from_slice(&opcode.to_le_bytes());
    buf.extend_from_slice(&(len as u32).to_le_bytes());
    for w in words {
        buf.extend_from_slice(&w.to_le_bytes());
    }
    buf.extend_from_slice(tail);
    buf.resize(len, 0);
    c.g.write(base, &buf)
}

/// A command whose tail is one length-prefixed blob.
fn cmd_blob(c: &mut Call<'_>, opcode: u32, words: &[u32], blob: &[u8]) -> R<()> {
    let mut t = Vec::with_capacity(4 + blob.len());
    t.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    t.extend_from_slice(blob);
    cmd(c, opcode, words, &t)
}

/// Submit what is queued; the reply, if any, follows.
fn flush(c: &mut Call<'_>) -> R<Vec<u8>> {
    let (ring, tail, dev) = {
        let s = c.p.gl.lock().unwrap();
        (s.ring, s.tail, s.dev.clone())
    };
    let Some(dev) = dev else { return Ok(Vec::new()) };
    if tail == 0 {
        return Ok(Vec::new());
    }
    c.g.w32(ring + 8, tail)?;
    c.g.w32(ring + 16, 0)?;
    c.g.w32(ring + 20, 0)?;
    let t0 = (c.p.clocks.mono_ns)();
    let r = dev.ioctl(&mut c.g.m, GEX_GL_SUBMIT, ring);
    let dt = (c.p.clocks.mono_ns)().saturating_sub(t0);
    {
        let mut s = c.p.gl.lock().unwrap();
        s.tail = 0;
        s.submits.0 += 1;
        s.submits.1 += dt;
        s.submits.2 += tail as u64;
        let who = s.serving;
        s.stats.entry(who).or_insert((0, 0, 0)).2 += 1;
    }
    let status = c.g.u32(ring + 20)?;
    if !matches!(r, Some(Ok(_))) || status != 0 {
        let failed_at = c.g.u32(ring + 24)?;
        return Err(c.unimplemented(&format!(
            "a GL batch the host refused (status {status}, command {failed_at}): an opcode or argument it does not take"
        )));
    }
    let n = c.g.u32(ring + 16)? as usize;
    c.g.bytes(ring + HEADER + CMD_CAPACITY as u64, n)
}

/// Submit what is queued without waiting for it to run ([`GEX_GL_SUBMIT_ASYNC`]).
/// A failure surfaces at the next submit, as that batch's.
fn flush_async(c: &mut Call<'_>) -> R<()> {
    let (ring, tail, dev) = {
        let s = c.p.gl.lock().unwrap();
        (s.ring, s.tail, s.dev.clone())
    };
    let Some(dev) = dev else { return Ok(()) };
    if tail == 0 {
        return Ok(());
    }
    c.g.w32(ring + 8, tail)?;
    c.g.w32(ring + 16, 0)?;
    c.g.w32(ring + 20, 0)?;
    let t0 = (c.p.clocks.mono_ns)();
    let r = dev.ioctl(&mut c.g.m, GEX_GL_SUBMIT_ASYNC, ring);
    if r.is_none() {
        flush(c)?;
        return Ok(());
    }
    let dt = (c.p.clocks.mono_ns)().saturating_sub(t0);
    {
        let mut s = c.p.gl.lock().unwrap();
        s.tail = 0;
        s.submits.0 += 1;
        s.submits.1 += dt;
        s.submits.2 += tail as u64;
        let who = s.serving;
        s.stats.entry(who).or_insert((0, 0, 0)).2 += 1;
    }
    if !matches!(r, Some(Ok(_))) {
        return Err(c.unimplemented("a GL batch the host refused (the one before this frame's last)"));
    }
    Ok(())
}

/// Whether GL calls are timed by function, for [`take_stats`] (a host's
/// GEX_NT_GL_STATS): two clock reads and a lock per call, so off by default.
pub static STATS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the program's GL cost since the last call, for GEX_NT_GL_STATS:
/// frames, submits (round trips to the host GL, their time and bytes) and the
/// GL functions by time spent in them (their submits included), the top
/// `n`. Resets the counters.
pub fn take_stats(p: &super::Process, n: usize) -> String {
    let mut s = p.gl.lock().unwrap();
    let frames = s.frame - s.stats_frame;
    s.stats_frame = s.frame;
    let mut v: Vec<(&'static str, (u64, u64, u64))> = s.stats.drain().collect();
    let (k, ns, bytes) = std::mem::take(&mut s.submits);
    drop(s);
    v.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
    let calls: u64 = v.iter().map(|x| x.1 .0).sum();
    let f = frames.max(1);
    let top: Vec<String> = v
        .iter()
        .take(n)
        .map(|(name, (c, t, k))| {
            format!("{name} {:.1}/f {:.2}ms/f {:.2}sub/f", *c as f64 / f as f64, *t as f64 / 1e6 / f as f64, *k as f64 / f as f64)
        })
        .collect();
    format!(
        "{frames} frames: {} GL calls/f, {:.1} submits/f ({:.2} ms/f, {} KiB/f); {}",
        calls / f,
        k as f64 / f as f64,
        ns as f64 / 1e6 / f as f64,
        bytes / f / 1024,
        top.join(", ")
    )
}

/// [`flush`], leaving the reply where it is: its guest address and length.
/// For a reply the size of a frame, which a host copy would allocate twice
/// a frame.
fn flush_in_place(c: &mut Call<'_>) -> R<(u64, usize)> {
    let (ring, tail, dev) = {
        let s = c.p.gl.lock().unwrap();
        (s.ring, s.tail, s.dev.clone())
    };
    let Some(dev) = dev else { return Ok((0, 0)) };
    if tail == 0 {
        return Ok((0, 0));
    }
    c.g.w32(ring + 8, tail)?;
    c.g.w32(ring + 16, 0)?;
    c.g.w32(ring + 20, 0)?;
    let t0 = (c.p.clocks.mono_ns)();
    let r = dev.ioctl(&mut c.g.m, GEX_GL_SUBMIT, ring);
    let dt = (c.p.clocks.mono_ns)().saturating_sub(t0);
    {
        let mut s = c.p.gl.lock().unwrap();
        s.tail = 0;
        s.submits.0 += 1;
        s.submits.1 += dt;
        s.submits.2 += tail as u64;
        let who = s.serving;
        s.stats.entry(who).or_insert((0, 0, 0)).2 += 1;
    }
    let status = c.g.u32(ring + 20)?;
    if !matches!(r, Some(Ok(_))) || status != 0 {
        let failed_at = c.g.u32(ring + 24)?;
        return Err(c.unimplemented(&format!(
            "a GL batch the host refused (status {status}, command {failed_at}): an opcode or argument it does not take"
        )));
    }
    let n = c.g.u32(ring + 16)? as usize;
    Ok((ring + HEADER + CMD_CAPACITY as u64, n))
}

struct Reply {
    b: Vec<u8>,
    at: usize,
}

impl Reply {
    fn u32(&mut self) -> u32 {
        let v = self.b.get(self.at..self.at + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap())).unwrap_or(0);
        self.at += 4;
        v
    }
    fn blob(&mut self) -> Vec<u8> {
        let n = self.u32() as usize;
        let v = self.b.get(self.at..self.at + n).map(|x| x.to_vec()).unwrap_or_default();
        self.at += n.div_ceil(4) * 4;
        v
    }
}

/// Queue `opcode` and submit, for a call that needs an answer.
fn ask(c: &mut Call<'_>, opcode: u32, words: &[u32], tail: &[u8]) -> R<Reply> {
    cmd(c, opcode, words, tail)?;
    Ok(Reply { b: flush(c)?, at: 0 })
}

fn take_name(c: &Call<'_>) -> u32 {
    let mut s = c.p.gl.lock().unwrap();
    let n = s.next_name;
    s.next_name += 1;
    n
}

/// A NUL-terminated copy of `s` in guest memory, kept for the process's life.
fn intern(c: &mut Call<'_>, s: &[u8]) -> R<u64> {
    if let Some(&p) = c.p.gl.lock().unwrap().strings.get(s) {
        return Ok(p);
    }
    let p = super::sysinfo::alloc_private(c, s.len() as u64 + 1, "[gl string]")?;
    c.g.write(p, s)?;
    c.g.w8(p + s.len() as u64, 0)?;
    c.p.gl.lock().unwrap().strings.insert(s.to_vec(), p);
    Ok(p)
}

fn cstr(c: &Call<'_>, p: u64) -> R<Vec<u8>> {
    let mut v = Vec::new();
    while v.len() < 1 << 20 {
        let b = c.g.u8(p + v.len() as u64)?;
        if b == 0 {
            break;
        }
        v.push(b);
    }
    Ok(v)
}

// ------------------------------------------------------------ pixel sizes

fn group_bytes(format: u32, ty: u32) -> Option<u32> {
    let comps = match format {
        0x1900..=0x1906 | 0x1909 | 0x8d94..=0x8d96 => 1, // index, stencil, depth, red..alpha, luminance, *_INTEGER
        0x190a | 0x8227 | 0x8228 => 2,                    // luminance alpha, RG
        0x1907 | 0x80e0 | 0x8d98 | 0x8d9a => 3,           // RGB, BGR
        0x1908 | 0x80e1 | 0x8d99 | 0x8d9b => 4,           // RGBA, BGRA
        _ => return None,
    };
    Some(match ty {
        0x8032 | 0x8362 => 1,
        0x8033 | 0x8034 | 0x8363..=0x8366 => 2,
        0x8035 | 0x8036 | 0x8367 | 0x8368 | 0x84fa | 0x8c3b | 0x8c3e => 4,
        0x1400 | 0x1401 => comps,
        0x1402 | 0x1403 | 0x140b => 2 * comps,
        0x1404..=0x1406 => 4 * comps,
        _ => return None,
    })
}

/// Bytes a w x h transfer spans from its pointer under `store`.
fn transfer_bytes(format: u32, ty: u32, w: i32, h: i32, store: PixelStore) -> Option<usize> {
    if w <= 0 || h <= 0 {
        return Some(0);
    }
    let g = group_bytes(format, ty)? as usize;
    let row_px = if store.row_length != 0 { store.row_length as usize } else { w as usize };
    let a = store.alignment.max(1) as usize;
    let stride = (row_px * g).div_ceil(a) * a;
    Some(store.skip_rows as usize * stride + store.skip_pixels as usize * g + (h as usize - 1) * stride + w as usize * g)
}

fn set_store(s: &mut State, pname: u32, v: i32) {
    let v = v.max(0) as u32;
    match pname {
        0x0cf5 => s.unpack.alignment = v,
        0x0cf2 => s.unpack.row_length = v,
        0x0cf4 => s.unpack.skip_pixels = v,
        0x0cf3 => s.unpack.skip_rows = v,
        0x806e => s.unpack.image_height = v,
        0x806d => s.unpack.skip_images = v,
        0x0d05 => s.pack.alignment = v,
        0x0d02 => s.pack.row_length = v,
        0x0d04 => s.pack.skip_pixels = v,
        0x0d03 => s.pack.skip_rows = v,
        _ => {}
    }
}

fn element_bytes(size: i32, ty: u32) -> usize {
    let comp = match ty {
        0x1400 | 0x1401 => 1,
        0x1402 | 0x1403 | 0x140b => 2,
        0x1404..=0x1406 | 0x140c => 4,
        0x140a => 8,
        _ => 0,
    };
    // GL_BGRA as a size means four components.
    let n = if size == 0x80e1 { 4 } else { size.clamp(0, 4) as usize };
    comp * n
}

// ------------------------------------------------------------------- WGL

/// The pixel formats offered: RGBA8 double-buffered, with D24S8 or none.
const FORMATS: &[(u8, u8)] = &[(24, 8), (0, 0)];

fn write_formats(c: &mut Call<'_>, at: u64, max: u32) -> R<()> {
    const SIZE: u64 = 140; // struct wgl_pixel_format
    for (i, &(depth, stencil)) in FORMATS.iter().enumerate().take(max as usize) {
        let f = at + SIZE * i as u64;
        c.g.zero(f, SIZE as usize)?;
        c.g.w16(f, 40)?; // nSize
        c.g.w16(f + 2, 1)?;
        // PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER | PFD_SUPPORT_COMPOSITION
        c.g.w32(f + 4, 0x4 | 0x20 | 0x1 | 0x8000)?;
        let pfd: [(u64, u8); 11] =
            [(8, 0), (9, 32), (10, 8), (11, 16), (12, 8), (13, 8), (14, 8), (15, 0), (16, 8), (17, 24), (23, depth)];
        for (o, v) in pfd {
            c.g.w8(f + o, v)?;
        }
        c.g.w8(f + 24, stencil)?;
        let ints = f + 40;
        c.g.w32(ints, 0x2028)?; // swap_method: WGL_SWAP_EXCHANGE_ARB
        c.g.w32(ints + 8, 0x202b)?; // pixel_type: WGL_TYPE_RGBA_ARB
    }
    Ok(())
}

fn dc_window(c: &Call<'_>, hdc: u32) -> u32 {
    match gdi::get(c, hdc) {
        Some(Obj::Dc(d)) if !d.memory => d.hwnd,
        _ => 0,
    }
}

fn client_size(c: &Call<'_>, hwnd: u32) -> (i32, i32) {
    let u = c.p.user.lock().unwrap();
    u.windows.get(&hwnd).map(|w| (w.client_rect.width().max(1), w.client_rect.height().max(1))).unwrap_or((1, 1))
}

/// Make the host context (desktop GL, compatibility profile) on first use.
fn ensure_host(c: &mut Call<'_>) -> R<()> {
    if c.p.gl.lock().unwrap().host_made {
        return Ok(());
    }
    // The fourth word is the profile, 1 for compatibility (what a WGL context
    // made the legacy way is, and what Noita's fixed-function drawing needs):
    // without it a Mesa host makes a core context when it can.
    let mut r = ask(c, op::CONTEXT_CREATE, &[proto::client_api::DESKTOP, 0, 0, 1], &[])?;
    let _vendor = r.blob();
    let _renderer = r.blob();
    let version = String::from_utf8_lossy(&r.blob()).into_owned();
    let mut e = ask(c, op::GET_STRING, &[0x1f03], &[])?;
    let extensions = String::from_utf8_lossy(&e.blob()).into_owned();
    let mut s = c.p.gl.lock().unwrap();
    s.host_made = true;
    s.version = version;
    s.extensions = extensions;
    Ok(())
}

/// Fill a guest context's client struct: version and extensions (Wine's
/// `make_context_current`).
fn sync_client(c: &mut Call<'_>, client: u64) -> R<()> {
    use table::client as f;
    let (version, exts) = {
        let s = c.p.gl.lock().unwrap();
        (s.version.clone(), s.extensions.clone())
    };
    let mut nums = version.split(|ch: char| !ch.is_ascii_digit()).filter(|x| !x.is_empty());
    let major: i32 = nums.next().and_then(|x| x.parse().ok()).unwrap_or(1);
    let minor: i32 = nums.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    // No newer than what this serves every command of.
    let (major, minor) = (major, minor).min(generated::MAX_VERSION);
    c.g.w32(client + f::MAJOR_VERSION, major as u32)?;
    c.g.w32(client + f::MINOR_VERSION, minor as u32)?;
    c.g.w32(client + f::CONTEXT_FLAGS, 0)?;
    c.g.w32(client + f::PROFILE_MASK, if major >= 3 && (major > 3 || minor >= 2) { 2 } else { 0 })?; // compatibility
    let mut on = vec![false; table::EXTENSIONS.len()];
    for name in exts.split_ascii_whitespace() {
        // Only those whose every command is served here.
        if generated::CLAIMABLE.binary_search(&name).is_err() {
            continue;
        }
        if let Some(i) = table::EXTENSIONS[..table::WGL_FIRST_EXTENSION].iter().position(|e| *e == name) {
            on[i] = true;
        }
    }
    for w in [
        "WGL_ARB_create_context",
        "WGL_ARB_create_context_profile",
        "WGL_ARB_extensions_string",
        "WGL_ARB_make_current_read",
        "WGL_ARB_pixel_format",
        "WGL_EXT_extensions_string",
        "WGL_EXT_swap_control",
    ] {
        if let Some(i) = table::EXTENSIONS.iter().position(|e| *e == w) {
            on[i] = true;
        }
    }
    let bytes: Vec<u8> = on.iter().map(|&b| b as u8).collect();
    c.g.write(client + f::EXTENSIONS, &bytes)?;
    // The array lists GL extensions, then the two WGL ones glGetStringi's
    // GL_EXTENSIONS includes (Wine's order).
    let mut list: Vec<u16> = (0..table::WGL_FIRST_EXTENSION).filter(|&i| on[i]).map(|i| i as u16).collect();
    for w in ["WGL_EXT_extensions_string", "WGL_EXT_swap_control"] {
        if let Some(i) = table::EXTENSIONS.iter().position(|e| *e == w) {
            list.push(i as u16);
        }
    }
    c.g.w32(client + f::EXTENSION_COUNT, list.len() as u32)?;
    let arr: Vec<u8> = list.iter().flat_map(|x| x.to_le_bytes()).collect();
    c.g.write(client + f::EXTENSION_ARRAY, &arr)?;
    if let Some(ctx) = c.p.gl.lock().unwrap().contexts.get_mut(&client) {
        ctx.major = major;
        ctx.minor = minor;
        ctx.synced = true;
    }
    Ok(())
}

/// Resize the default framebuffer to the window's client area.
fn fit_surface(c: &mut Call<'_>, hdc: u32) -> R<()> {
    let hwnd = dc_window(c, hdc);
    if hwnd == 0 {
        return Ok(());
    }
    let size = client_size(c, hwnd);
    if c.p.gl.lock().unwrap().surface == size {
        return Ok(());
    }
    cmd(c, op::SURFACE_RESIZE, &[size.0 as u32, size.1 as u32], &[])?;
    c.p.gl.lock().unwrap().surface = size;
    Ok(())
}

/// SwapBuffers: the default framebuffer into the window's surface.
fn swap(c: &mut Call<'_>, hdc: u32) -> R<bool> {
    let hwnd = dc_window(c, hdc);
    if hwnd == 0 {
        return Err(c.unimplemented("SwapBuffers on a memory or display DC"));
    }
    let (w, h) = c.p.gl.lock().unwrap().surface;
    if w <= 0 || h <= 0 {
        return Ok(true);
    }
    // A display that takes the host's frame as it is (the browser's
    // compositor draws Mesa's frame straight into the window, on the GPU):
    // say which window it belongs to and present it -- nothing comes back.
    if let (Some(d), Some(g)) = (c.p.display(), gdi::surface::geometry(c, hwnd, false)) {
        if let Some(token) = d.gl_target(g.toplevel, (g.origin.0, g.origin.1, w, h)) {
            if c.p.gl.lock().unwrap().attached != g.toplevel {
                cmd(c, op::SURFACE_ATTACH, &[proto::surface::WAYLAND, g.toplevel, token, 0], &[])?;
                c.p.gl.lock().unwrap().attached = g.toplevel;
                // And the window itself, once: its frame and caption are its
                // own pixels, and a window that only ever draws with GL
                // would otherwise never be published at all.
                gdi::surface::surface(c, g.toplevel, g.size)?;
                gdi::surface::flush(c, g.toplevel);
            }
            // Not waited for: the guest draws the next frame while the
            // host runs this one, and the next submit waits for it.
            cmd(c, op::PRESENT, &[], &[])?;
            flush_async(c)?;
            return finish_swap(c, hdc, hwnd);
        }
    }
    // Read from the default framebuffer with default pack state, then put
    // the program's back.
    let (read_fbo, pack) = {
        let s = c.p.gl.lock().unwrap();
        (s.read_fbo, s.pack)
    };
    const GL_READ_FRAMEBUFFER: u32 = 0x8ca8;
    if read_fbo != 0 {
        cmd(c, op::BIND_FRAMEBUFFER, &[GL_READ_FRAMEBUFFER, 0], &[])?;
    }
    let packs = [(0x0d05u32, pack.alignment, 4u32), (0x0d02, pack.row_length, 0), (0x0d04, pack.skip_pixels, 0), (0x0d03, pack.skip_rows, 0)];
    for (pn, cur, want) in packs {
        if cur != want {
            cmd(c, op::PIXEL_STOREI, &[pn, want], &[])?;
        }
    }
    cmd(c, op::READ_PIXELS, &[0, 0, w as u32, h as u32, 0x80e1, 0x1401, 0, 0, 0], &[])?;
    // The pixels stay in the ring's reply area: `{offset, length, bytes}`.
    let (reply, n) = flush_in_place(c)?;
    for (pn, cur, want) in packs {
        if cur != want {
            cmd(c, op::PIXEL_STOREI, &[pn, cur], &[])?;
        }
    }
    if read_fbo != 0 {
        cmd(c, op::BIND_FRAMEBUFFER, &[GL_READ_FRAMEBUFFER, read_fbo], &[])?;
    }
    let px_len = if n >= 8 { c.g.u32(reply + 4)? as usize } else { 0 };
    let px = reply + 8;
    let stride = w as usize * 4;
    if px_len < stride * h as usize || n < 8 + px_len {
        return Ok(false);
    }
    // Into the top-level window's surface at the client area's place.
    let Some(g) = gdi::surface::geometry(c, hwnd, false) else { return Ok(true) };
    let surf = gdi::surface::surface(c, g.toplevel, g.size)?;
    let (ox, oy) = g.origin;
    let vis = g.vis.first().copied().unwrap_or_default();
    for y in 0..h {
        let sy = oy + y;
        if sy < vis.top || sy >= vis.bottom {
            continue;
        }
        let x0 = ox.max(vis.left);
        let x1 = (ox + w).min(vis.right);
        if x1 <= x0 {
            continue;
        }
        let src = (h - 1 - y) as usize * stride + (x0 - ox) as usize * 4;
        c.g.m
            .copy_within(px + src as u64, surf.row(sy) + x0 as u64 * 4, (x1 - x0) as usize * 4)
            .map_err(|_| Status(st::ACCESS_VIOLATION))?;
    }
    gdi::surface::flush(c, g.toplevel);
    finish_swap(c, hdc, hwnd)
}

/// After a frame went to the window, however: count it, let scripted input
/// run, and size the next frame to the window.
fn finish_swap(c: &mut Call<'_>, hdc: u32, hwnd: u32) -> R<bool> {
    let frame = {
        let mut s = c.p.gl.lock().unwrap();
        s.frame += 1;
        c.p.presents.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        s.frame
    };
    // Scripted input (GEX_NT_INPUT) is timed by frames.
    super::user::input::script_tick(c, frame, hwnd);
    // The next frame is drawn at the window's size.
    fit_surface(c, hdc)?;
    Ok(true)
}

// ------------------------------------------------------------ client arrays

/// Upload each enabled client-memory array for a draw reaching `vertices`
/// elements, and point GL at the copies.
fn upload_arrays(c: &mut Call<'_>, vertices: usize) -> R<()> {
    upload_arrays_instanced(c, vertices, 1)
}

/// The opcode of a pointer command (`generated::POINTER_PLANS`).
fn pointer_op(name: &str) -> u32 {
    generated::POINTER_PLANS.binary_search_by(|e| e.0.cmp(name)).map(|i| generated::POINTER_PLANS[i].1).unwrap_or(0)
}

/// Send fixed array `i`'s pointer, as `offset` into the bound array buffer.
fn send_array_pointer(c: &mut Call<'_>, i: usize, a: &Array, offset: u64) -> R<()> {
    let [lo, hi] = proto::Batch::words64(offset);
    let (size, ty, stride) = (a.size as u32, a.ty, a.stride as u32);
    match i {
        VERTEX => cmd(c, op::VERTEX_POINTER, &[size, ty, stride, lo, hi], &[]),
        COLOR => cmd(c, op::COLOR_POINTER, &[size, ty, stride, lo, hi], &[]),
        NORMAL => cmd(c, pointer_op("glNormalPointer"), &[ty, stride, lo, hi], &[]),
        SECONDARY => cmd(c, pointer_op("glSecondaryColorPointer"), &[size, ty, stride, lo, hi], &[]),
        FOG => cmd(c, pointer_op("glFogCoordPointer"), &[ty, stride, lo, hi], &[]),
        _ => {
            cmd(c, op::CLIENT_ACTIVE_TEXTURE, &[0x84c0 + (i - TEXCOORD) as u32], &[])?;
            cmd(c, op::TEX_COORD_POINTER, &[size, ty, stride, lo, hi], &[])
        }
    }
}

/// Send generic attribute `i`'s pointer, as `offset` into the bound array
/// buffer.
fn send_attrib_pointer(c: &mut Call<'_>, i: usize, a: &Attrib, offset: u64) -> R<()> {
    let [lo, hi] = proto::Batch::words64(offset);
    let (idx, size, ty, stride) = (i as u32, a.size as u32, a.ty, a.stride as u32);
    match a.kind {
        0 => cmd(c, pointer_op("glVertexAttribPointer"), &[idx, size, ty, a.normalized as u32, stride, lo, hi], &[]),
        1 => cmd(c, pointer_op("glVertexAttribIPointer"), &[idx, size, ty, stride, lo, hi], &[]),
        _ => cmd(c, pointer_op("glVertexAttribLPointer"), &[idx, size, ty, stride, lo, hi], &[]),
    }
}

/// Copy `bytes` of client memory at `pointer` into scratch buffer `*scratch`
/// (made on first use), left bound to GL_ARRAY_BUFFER.
fn upload_scratch(c: &mut Call<'_>, scratch: u32, target: u32, data: &[u8]) -> R<u32> {
    let scratch = if scratch != 0 {
        scratch
    } else {
        let n = take_name(c);
        cmd(c, op::GEN_BUFFERS, &[1, n], &[])?;
        n
    };
    cmd(c, op::BIND_BUFFER, &[target, scratch], &[])?;
    let [lo, hi] = proto::Batch::words64(data.len() as u64);
    cmd_blob(c, op::BUFFER_DATA, &[target, 0x88e8, lo, hi, 1], data)?; // GL_DYNAMIC_DRAW
    Ok(scratch)
}

/// Client-memory arrays (fixed and generic) copied into buffers of ours for
/// a draw reaching `vertices` vertices and `instances` instances.
fn upload_arrays_instanced(c: &mut Call<'_>, vertices: usize, instances: usize) -> R<()> {
    let (arrays, attribs, bound) = {
        let s = c.p.gl.lock().unwrap();
        (s.arrays, s.attribs, s.array_buffer)
    };
    const GL_ARRAY_BUFFER: u32 = 0x8892;
    let mut rebound = false;
    for (i, a) in arrays.iter().enumerate() {
        if !a.enabled || a.buffer != 0 || a.pointer == 0 || vertices == 0 {
            continue;
        }
        let elem = element_bytes(if i == NORMAL { 3 } else if i == FOG { 1 } else { a.size }, a.ty);
        if elem == 0 {
            return Err(c.unimplemented(&format!("a client array of type {:#x}", a.ty)));
        }
        let stride = if a.stride > 0 { a.stride as usize } else { elem };
        let data = c.g.bytes(a.pointer, (vertices - 1) * stride + elem)?;
        let scratch = upload_scratch(c, a.scratch, GL_ARRAY_BUFFER, &data)?;
        c.p.gl.lock().unwrap().arrays[i].scratch = scratch;
        rebound = true;
        send_array_pointer(c, i, a, 0)?;
    }
    for (i, a) in attribs.iter().enumerate() {
        if !a.enabled || a.buffer != 0 || a.pointer == 0 || vertices == 0 {
            continue;
        }
        let elem = attrib_bytes(a.size, a.ty);
        if elem == 0 {
            return Err(c.unimplemented(&format!("a client vertex attribute of type {:#x}", a.ty)));
        }
        let stride = if a.stride > 0 { a.stride as usize } else { elem };
        let count = if a.divisor == 0 { vertices } else { instances.div_ceil(a.divisor as usize).max(1) };
        let data = c.g.bytes(a.pointer, (count - 1) * stride + elem)?;
        let scratch = upload_scratch(c, a.scratch, GL_ARRAY_BUFFER, &data)?;
        c.p.gl.lock().unwrap().attribs[i].scratch = scratch;
        rebound = true;
        send_attrib_pointer(c, i, a, 0)?;
    }
    if rebound {
        cmd(c, op::BIND_BUFFER, &[GL_ARRAY_BUFFER, bound], &[])?;
        let unit = c.p.gl.lock().unwrap().client_texture;
        cmd(c, op::CLIENT_ACTIVE_TEXTURE, &[0x84c0 + unit as u32], &[])?;
    }
    Ok(())
}

/// Whether any enabled array is in client memory.
fn client_arrays(c: &Call<'_>) -> bool {
    let s = c.p.gl.lock().unwrap();
    s.arrays.iter().any(|a| a.enabled && a.buffer == 0 && a.pointer != 0)
        || s.attribs.iter().any(|a| a.enabled && a.buffer == 0 && a.pointer != 0)
}

/// A generic attribute's element: `size` components of `ty` (the packed
/// types are one 4-byte element; GL_BGRA as a size is four).
fn attrib_bytes(size: i32, ty: u32) -> usize {
    match ty {
        0x8d9f | 0x8368 | 0x8c3b => 4, // INT_2_10_10_10_REV, UNSIGNED_INT_2_10_10_10_REV, 10F_11F_11F
        _ => element_bytes(size, ty),
    }
}

/// glDrawElements and its family: `vals` are the command's parameters in
/// its plan's order, the indices pointer at `at`. Client-memory indices are
/// uploaded to a buffer of ours, and client arrays as far as they reach.
fn draw_elements(c: &mut Call<'_>, name: &str, mut vals: Vec<u64>, at: usize, count: i32, ty: u32, base_vertex: i64, instances: usize) -> R<()> {
    let (element_buffer, scratch) = {
        let s = c.p.gl.lock().unwrap();
        (s.element_buffer, s.element_scratch)
    };
    let size = match ty {
        0x1401 => 1,
        0x1403 => 2,
        0x1405 => 4,
        _ => 0,
    };
    let pointer = vals[at];
    let client_indices = element_buffer == 0 && pointer != 0 && count > 0 && size > 0;
    let mut indices = Vec::new();
    if client_indices {
        indices = c.g.bytes(pointer, count as usize * size)?;
    } else if element_buffer != 0 && count > 0 && size > 0 && client_arrays(c) {
        // The indices are the host's; read back as far as the draw goes to
        // know how much of the client arrays it reaches.
        let mut r = ask_plan(c, "glGetBufferSubData", &[0x8893, pointer, (count as usize * size) as u64, 0])?;
        indices = r.blob();
    }
    if !indices.is_empty() {
        let max = indices
            .chunks_exact(size)
            .map(|x| match size {
                1 => x[0] as u64,
                2 => u16::from_le_bytes([x[0], x[1]]) as u64,
                _ => u32::from_le_bytes([x[0], x[1], x[2], x[3]]) as u64,
            })
            .max()
            .unwrap_or(0);
        let reach = (max as i64 + 1 + base_vertex).max(0) as usize;
        upload_arrays_instanced(c, reach, instances)?;
    }
    if client_indices {
        const GL_ELEMENT_ARRAY_BUFFER: u32 = 0x8893;
        let n = upload_scratch(c, scratch, GL_ELEMENT_ARRAY_BUFFER, &indices)?;
        c.p.gl.lock().unwrap().element_scratch = n;
        vals[at] = 0;
        send_pointer_plan(c, name, &vals)?;
        cmd(c, op::BIND_BUFFER, &[GL_ELEMENT_ARRAY_BUFFER, 0], &[])?;
        return Ok(());
    }
    send_pointer_plan(c, name, &vals)
}

/// Send pointer command `name` (`generated::POINTER_PLANS`) with `vals`,
/// its parameters, a pointer as its offset.
fn send_pointer_plan(c: &mut Call<'_>, name: &str, vals: &[u64]) -> R<()> {
    let Ok(i) = generated::POINTER_PLANS.binary_search_by(|e| e.0.cmp(name)) else {
        return Err(c.unimplemented(&format!("opengl32 {name}")));
    };
    let (_, opcode, plan) = generated::POINTER_PLANS[i];
    let mut words = Vec::new();
    for (t, v) in parse_plan(plan)[1..].iter().zip(vals) {
        match t.kind {
            b'q' | b'P' => words.extend([*v as u32, (*v >> 32) as u32]),
            _ => words.push(*v as u32),
        }
    }
    cmd(c, opcode, &words, &[])
}

/// Run planned command `name` with scalar `vals` from our own code, waiting
/// for its reply.
fn ask_plan(c: &mut Call<'_>, name: &str, vals: &[u64]) -> R<Reply> {
    let Ok(i) = generated::PLANS.binary_search_by(|e| e.0.cmp(name)) else {
        return Err(c.unimplemented(&format!("opengl32 {name} (internal)")));
    };
    let (_, opcode, plan) = generated::PLANS[i];
    let mut words = Vec::new();
    for (t, v) in parse_plan(plan)[1..].iter().zip(vals) {
        match t.kind {
            b'q' => words.extend([*v as u32, (*v >> 32) as u32]),
            b'o' => {}
            _ => words.push(*v as u32),
        }
    }
    ask(c, opcode, &words, &[])
}

/// The parameters of opengl32 call `p` as raw words, by its params struct.
fn raw_params(c: &Call<'_>, p: &Params) -> R<Vec<u64>> {
    let kinds = p.sig.as_bytes();
    (1..kinds.len())
        .map(|f| {
            Ok(match kinds[f] {
                b'b' => p.u8(c, f)? as u64,
                b'h' => c.g.u16(p.at + p.offs[f])? as u64,
                b'i' | b'f' => p.u32(c, f)? as u64,
                b'd' | b'q' => c.g.u64(p.at + p.offs[f])?,
                _ => p.ptr(c, f)?,
            })
        })
        .collect()
}

/// glMapBuffer/glMapBufferRange: a copy of the range in guest memory, read
/// back first when the access reads.
fn map_buffer(c: &mut Call<'_>, target: u32, offset: u64, len: Option<u64>, read: bool, write: bool, explicit_flush: bool) -> R<u64> {
    let name = c.p.gl.lock().unwrap().bound.get(&target).copied().unwrap_or(0);
    if name == 0 {
        return Ok(0);
    }
    let len = match len {
        Some(l) => l,
        None => {
            let mut r = ask_plan(c, "glGetBufferParameteriv", &[target as u64, 0x8764, 0])?; // GL_BUFFER_SIZE
            let b = r.blob();
            b.get(..4).map(|x| u32::from_le_bytes(x.try_into().unwrap()) as u64).unwrap_or(0)
        }
    };
    let cached = c.p.gl.lock().unwrap().map_memory.get(&name).copied();
    let addr = match cached {
        Some((a, cap)) if cap >= len.max(1) => a,
        _ => {
            let a = super::sysinfo::alloc_private(c, len.max(1), "[gl mapped buffer]")?;
            c.p.gl.lock().unwrap().map_memory.insert(name, (a, len.max(1)));
            a
        }
    };
    if read && len > 0 {
        let mut r = ask_plan(c, "glGetBufferSubData", &[target as u64, offset, len, 0])?;
        let data = r.blob();
        c.g.write(addr, &data[..data.len().min(len as usize)])?;
    }
    c.p.gl.lock().unwrap().maps.insert(name, Mapping { target, addr, offset, len, write, explicit_flush });
    Ok(addr)
}

/// Write `len` bytes of mapping `m` from `at` (relative to the mapping) back.
fn write_back_map(c: &mut Call<'_>, m: &Mapping, at: u64, len: u64) -> R<()> {
    let len = len.min(m.len.saturating_sub(at));
    if len == 0 {
        return Ok(());
    }
    let bytes = c.g.bytes(m.addr + at, len as usize)?;
    let [ol, oh] = proto::Batch::words64(m.offset + at);
    let [sl, sh] = proto::Batch::words64(len);
    cmd_blob(c, op::BUFFER_SUB_DATA, &[m.target, ol, oh, sl, sh], &bytes)
}

/// Generated opcodes of the GL 4.4 transport the hand-written calls here
/// use (tools/gen_gl44_transport.py: 1024 plus the command's index in the
/// 4.4 core command list).
const GL44_GET_COMPRESSED_TEX_IMAGE: u32 = 1194;
const GL44_GET_TEX_IMAGE: u32 = 1247;

/// glNormalPointer, glFogCoordPointer, glSecondaryColorPointer: fixed array
/// `i` with `size` components.
fn set_typed_pointer(c: &mut Call<'_>, i: usize, size: i32, ty: u32, stride: i32, pointer: u64) -> R<()> {
    let a = {
        let mut s = c.p.gl.lock().unwrap();
        let buffer = s.array_buffer;
        let a = &mut s.arrays[i];
        a.size = size;
        a.ty = ty;
        a.stride = stride;
        a.pointer = pointer;
        a.buffer = buffer;
        *a
    };
    if a.buffer != 0 {
        send_array_pointer(c, i, &a, pointer)?;
    }
    Ok(())
}

fn array_index(c: &Call<'_>, cap: u32) -> Option<usize> {
    match cap {
        0x8074 => Some(VERTEX),
        0x8076 => Some(COLOR),
        0x8078 => Some(TEXCOORD + c.p.gl.lock().unwrap().client_texture),
        0x8075 => Some(NORMAL),
        0x845e => Some(SECONDARY),
        0x8457 => Some(FOG),
        _ => None,
    }
}

fn set_pointer(c: &mut Call<'_>, i: usize, opcode: u32, size: i32, ty: u32, stride: i32, pointer: u64) -> R<()> {
    let buffer = c.p.gl.lock().unwrap().array_buffer;
    {
        let mut s = c.p.gl.lock().unwrap();
        let a = &mut s.arrays[i];
        a.size = size;
        a.ty = ty;
        a.stride = stride;
        a.pointer = pointer;
        a.buffer = buffer;
    }
    if buffer != 0 {
        let [lo, hi] = proto::Batch::words64(pointer);
        cmd(c, opcode, &[size as u32, ty, stride as u32, lo, hi], &[])?;
    }
    Ok(())
}

// ------------------------------------------------------------ dispatch

/// One opengl32 unix call.
pub fn call(c: &mut Call<'_>, code: u32, args: u64) -> R {
    let Some(&(name, sig)) = table::FUNCS.get(code as usize) else {
        return Err(c.unimplemented(&format!("opengl32 unix call {code}")));
    };
    let p = Params { at: args, offs: layout(sig, c.p.abi), sig };
    let timed = STATS.load(std::sync::atomic::Ordering::Relaxed);
    let t0 = if timed {
        c.p.gl.lock().unwrap().serving = name;
        (c.p.clocks.mono_ns)()
    } else {
        0
    };
    let r = one(c, name, &p);
    if timed {
        let dt = (c.p.clocks.mono_ns)().saturating_sub(t0);
        let mut s = c.p.gl.lock().unwrap();
        let e = s.stats.entry(name).or_insert((0, 0, 0));
        e.0 += 1;
        e.1 += dt;
    }
    if c.p.cfg.trace {
        crate::nt_log!("[nt:{:x}]   opengl32 {name}", c.t.tid);
    }
    r.map(|()| st::SUCCESS)
}

/// The FBO functions come in core and EXT spellings with the same arguments.
fn core_name(name: &str) -> &str {
    match name {
        "glGenFramebuffersEXT" => "glGenFramebuffers",
        "glBindFramebufferEXT" => "glBindFramebuffer",
        "glDeleteFramebuffersEXT" => "glDeleteFramebuffers",
        "glFramebufferTexture2DEXT" => "glFramebufferTexture2D",
        "glCheckFramebufferStatusEXT" => "glCheckFramebufferStatus",
        "glGenRenderbuffersEXT" => "glGenRenderbuffers",
        "glBindRenderbufferEXT" => "glBindRenderbuffer",
        "glDeleteRenderbuffersEXT" => "glDeleteRenderbuffers",
        "glRenderbufferStorageEXT" => "glRenderbufferStorage",
        "glFramebufferRenderbufferEXT" => "glFramebufferRenderbuffer",
        "glActiveTextureARB" => "glActiveTexture",
        "glClientActiveTextureARB" => "glClientActiveTexture",
        "glBindBufferARB" => "glBindBuffer",
        "glGenBuffersARB" => "glGenBuffers",
        "glBufferDataARB" => "glBufferData",
        "glBufferSubDataARB" => "glBufferSubData",
        "glDeleteBuffersARB" => "glDeleteBuffers",
        "glBlendFuncSeparateEXT" => "glBlendFuncSeparate",
        "glBlendEquationEXT" => "glBlendEquation",
        n => n,
    }
}

fn gen_names(c: &mut Call<'_>, opcode: u32, n: i32, out: u64) -> R<()> {
    let n = n.max(0) as usize;
    let mut words = vec![n as u32];
    for i in 0..n {
        let name = take_name(c);
        c.g.w32(out + 4 * i as u64, name)?;
        words.push(name);
    }
    cmd(c, opcode, &words, &[])
}

fn delete_names(c: &mut Call<'_>, opcode: u32, n: i32, names: u64) -> R<()> {
    let n = n.max(0) as usize;
    let mut words = vec![n as u32];
    for i in 0..n {
        words.push(c.g.u32(names + 4 * i as u64)?);
    }
    cmd(c, opcode, &words, &[])
}

// ------------------------------------------------- the planned commands

/// A plan token (`tools/gen_gl_compat.py`, `guest/gl/gexgl-plan.h`).
#[derive(Clone, Copy, Default)]
struct Tok {
    kind: u8,
    size: u32,
    spec: u8,
    arg: [i32; 5],
}

fn parse_plan(plan: &str) -> Vec<Tok> {
    plan.split(' ')
        .filter(|t| !t.is_empty())
        .map(|t| {
            let b = t.as_bytes();
            let mut tok = Tok { kind: b[0], arg: [-1; 5], ..Default::default() };
            if matches!(b[0], b'i' | b'j' | b'o') {
                let (size, spec) = t[1..].split_once(':').unwrap_or(("0", "n0"));
                tok.size = size.parse().unwrap_or(0);
                tok.spec = spec.as_bytes()[0];
                for (i, a) in spec[1..].split([',', '*']).filter(|a| !a.is_empty()).take(5).enumerate() {
                    tok.arg[i] = a.parse().unwrap_or(-1);
                }
                if tok.spec == b'p' && tok.arg[1] < 0 {
                    tok.arg[1] = 1;
                }
            }
            tok
        })
        .collect()
}

/// `gexgl_compute_pixel_layout`'s end: the bytes an unpack transfer spans.
fn pixel_end(format: u32, ty: u32, w: i32, h: i32, d: i32, s: PixelStore) -> usize {
    if w <= 0 || h <= 0 || d == 0 || d < -1 {
        return 0;
    }
    let Some(g) = group_bytes(format, ty) else { return 0 };
    let g = g as usize;
    let is_3d = d >= 0;
    let row_px = if s.row_length != 0 { s.row_length as usize } else { w as usize };
    let a = s.alignment.max(1) as usize;
    let stride = (row_px * g).div_ceil(a) * a;
    let rows = if is_3d && s.image_height != 0 { s.image_height as usize } else { h as usize };
    let image = stride * rows;
    let offset = if is_3d { s.skip_images as usize * image } else { 0 } + s.skip_rows as usize * stride + s.skip_pixels as usize * g;
    let layers = if is_3d { d as usize } else { 1 };
    offset + (layers - 1) * image + (h as usize - 1) * stride + w as usize * g
}

/// How many elements (bytes, for byte-sized specs) array token `t` holds.
fn plan_elements(t: &Tok, v: &[u64], store: PixelStore) -> usize {
    let a = |i: usize| v.get(t.arg[i] as usize).copied().unwrap_or(0);
    let count = |x: i64, m: i64| if x <= 0 || m <= 0 { 0 } else { (x.saturating_mul(m) as usize).min(1 << 28) };
    match t.spec {
        b'n' => t.arg[0].max(0) as usize,
        b'p' => count(a(0) as u32 as i32 as i64, t.arg[1] as i64),
        b'e' => generated::pname_count(a(0) as u32) as usize,
        b'z' => count(a(0) as i64, 1),
        b'L' => {
            let size = match a(1) as u32 {
                0x1400 | 0x1401 => 1,
                0x1402 | 0x1403 | 0x1407 => 2,
                0x1408 => 3,
                0x1404 | 0x1405 | 0x1406 | 0x1409 => 4,
                _ => 0,
            };
            count(a(0) as u32 as i32 as i64, size)
        }
        b'T' => 128,
        // A uniform: the host says how many, at most a 4x4 matrix.
        b'U' => 16,
        b'B' => {
            let (w, h) = (a(0) as u32 as i32, a(1) as u32 as i32);
            if w <= 0 || h <= 0 || w > 1 << 20 || h > 1 << 20 {
                return 0;
            }
            let px = if store.row_length != 0 { store.row_length as usize } else { w as usize };
            let al = store.alignment.max(1) as usize;
            let row = px.div_ceil(8).div_ceil(al) * al;
            (store.skip_rows as usize + h as usize) * row + (store.skip_pixels as usize).div_ceil(8)
        }
        b'X' => {
            let dim = |i: usize, d: i32| if t.arg[i] >= 0 { a(i) as u32 as i32 } else { d };
            pixel_end(a(0) as u32, a(1) as u32, dim(2, 1), dim(3, 1), dim(4, -1), store)
        }
        _ => 0,
    }
}

/// A command with a plan and no hand-written arm (`generated::PLANS`): its
/// parameters read as the plan says, sent on the wire it describes, and its
/// return value and outputs written back.
fn planned(c: &mut Call<'_>, name: &str, p: &Params) -> R<()> {
    if let Ok(i) = generated::ALIASES.binary_search_by(|e| e.0.cmp(name)) {
        return one(c, generated::ALIASES[i].1, p);
    }
    let Ok(i) = generated::PLANS.binary_search_by(|e| e.0.cmp(name)) else {
        return Err(c.unimplemented(&format!("opengl32 {name}")));
    };
    let (_, opcode, plan) = generated::PLANS[i];
    let toks = parse_plan(plan);
    let (ret, params) = (toks[0], &toks[1..]);
    let kinds = p.sig.as_bytes();
    let mut v = Vec::with_capacity(params.len());
    for (i, t) in params.iter().enumerate() {
        let f = i + 1;
        v.push(match kinds[f] {
            b'b' => p.u8(c, f)? as u64,
            b'h' => c.g.u16(p.at + p.offs[f])? as u64,
            b'i' | b'f' => p.u32(c, f)? as u64,
            b'd' | b'q' => c.g.u64(p.at + p.offs[f])?,
            // GLintptr and GLsizeiptr are signed; pointers are addresses.
            _ if t.kind == b'q' => p.isize(c, f)? as u64,
            _ => p.ptr(c, f)?,
        });
    }
    let (store, unpack_buffer) = {
        let s = c.p.gl.lock().unwrap();
        (s.unpack, s.unpack_buffer)
    };
    let mut words: Vec<u32> = Vec::new();
    let mut outputs = false;
    for (i, t) in params.iter().enumerate() {
        match t.kind {
            b'w' | b'f' => words.push(v[i] as u32),
            b'q' => words.extend([v[i] as u32, (v[i] >> 32) as u32]),
            b'o' => outputs = true,
            _ => {
                let ptr = v[i];
                if t.kind == b'j' && unpack_buffer != 0 {
                    words.extend([2, ptr as u32, (ptr >> 32) as u32]);
                    continue;
                }
                if ptr == 0 {
                    words.push(0);
                    continue;
                }
                let bytes = match t.spec {
                    b's' => cstr(c, ptr)?,
                    b'S' => {
                        let len = v[t.arg[0] as usize] as u32 as i32;
                        if len < 0 {
                            cstr(c, ptr)?
                        } else {
                            c.g.bytes(ptr, len as usize)?
                        }
                    }
                    _ => c.g.bytes(ptr, plan_elements(t, &v, store) * t.size as usize)?,
                };
                words.extend([1, bytes.len() as u32]);
                words.extend(bytes.chunks(4).map(|w| {
                    let mut x = [0u8; 4];
                    x[..w.len()].copy_from_slice(w);
                    u32::from_le_bytes(x)
                }));
            }
        }
    }
    if ret.kind == b'v' && !outputs {
        return cmd(c, opcode, &words, &[]);
    }
    let mut r = ask(c, opcode, &words, &[])?;
    if ret.kind != b'v' {
        let lo = r.u32() as u64;
        let value = if ret.kind == b'q' { lo | (r.u32() as u64) << 32 } else { lo };
        let at = p.ret();
        match kinds[at] {
            b'b' => p.set_u8(c, at, value as u8)?,
            b'h' => c.g.w16(p.at + p.offs[at], value as u16)?,
            b'd' | b'q' => c.g.w64(p.at + p.offs[at], value)?,
            _ => p.set_u32(c, at, value as u32)?,
        }
    }
    for (i, t) in params.iter().enumerate() {
        if t.kind != b'o' {
            continue;
        }
        let blob = r.blob();
        let want = plan_elements(t, &v, store) * t.size as usize;
        if v[i] != 0 && (blob.len() == want || (t.spec == b'U' && blob.len() <= want)) {
            c.g.write(v[i], &blob)?;
        }
    }
    Ok(())
}

#[allow(clippy::cognitive_complexity)]
fn one(c: &mut Call<'_>, name: &str, p: &Params) -> R<()> {
    let f = |v: f32| v.to_bits();
    match core_name(name) {
        // --- process and thread, pixel formats, contexts
        "process_attach" | "thread_attach" | "process_detach" => {}
        "get_pixel_formats" => {
            // { teb, hdc, formats, max, *num, *num_onscreen } by value.
            let (formats, max) = (p.ptr(c, 2)?, p.u32(c, 3)?);
            if formats != 0 {
                write_formats(c, formats, max)?;
            }
            p.set_u32(c, 4, FORMATS.len() as u32)?;
            p.set_u32(c, 5, FORMATS.len() as u32)?;
        }
        "wglGetPixelFormat" => {
            let hwnd = dc_window(c, p.u32(c, 1)?);
            let v = c.p.gl.lock().unwrap().formats.get(&hwnd).copied().unwrap_or(0);
            p.set_u32(c, 2, v as u32)?;
        }
        "wglSetPixelFormat" | "wglSetPixelFormatWINE" => {
            let (hdc, fmt) = (p.u32(c, 1)?, p.i32(c, 2)?);
            let hwnd = dc_window(c, hdc);
            let ok = hwnd != 0 && fmt >= 1 && fmt as usize <= FORMATS.len();
            if ok {
                let mut s = c.p.gl.lock().unwrap();
                // A window's format is set once (wglSetPixelFormatWINE may
                // change it).
                if name == "wglSetPixelFormatWINE" || !s.formats.contains_key(&hwnd) {
                    s.formats.insert(hwnd, fmt);
                }
            }
            let r = p.ret();
            p.set_u32(c, r, ok as u32)?;
        }
        "wglCreateContextAttribsARB" => {
            // { teb, hdc, share, attribs, ret = the client context }
            let (hdc, attribs, client) = (p.u32(c, 1)?, p.ptr(c, 3)?, p.ptr(c, 4)?);
            let (mut major, mut minor, mut profile) = (1, 0, 2);
            if attribs != 0 {
                let mut a = attribs;
                loop {
                    let k = c.g.u32(a)?;
                    if k == 0 {
                        break;
                    }
                    let v = c.g.u32(a + 4)? as i32;
                    match k {
                        0x2091 => major = v,   // WGL_CONTEXT_MAJOR_VERSION_ARB
                        0x2092 => minor = v,   // WGL_CONTEXT_MINOR_VERSION_ARB
                        0x9126 => profile = v, // WGL_CONTEXT_PROFILE_MASK_ARB
                        _ => {}
                    }
                    a += 8;
                }
            }
            if profile & 1 != 0 && (major > 3 || (major == 3 && minor >= 2)) {
                return Err(c.unimplemented(&format!("a {major}.{minor} core profile OpenGL context")));
            }
            ensure_host(c)?;
            c.g.w64(client + table::client::UNIX_HANDLE, client)?;
            c.g.w64(client + table::client::UNIX_FUNCS, 1)?;
            c.p.gl.lock().unwrap().contexts.insert(client, Ctx { hdc, major, minor, synced: false });
        }
        "wglDeleteContext" => {
            let client = p.ptr(c, 1)?;
            let ok = c.p.gl.lock().unwrap().contexts.remove(&client).is_some();
            let r = p.ret();
            p.set_u32(c, r, ok as u32)?;
        }
        "wglMakeContextCurrentARB" => {
            // { teb, draw_hdc, read_hdc, client, ret }
            let (draw, client) = (p.u32(c, 1)?, p.ptr(c, 3)?);
            let tid = c.t.tid;
            let ok = if client == 0 {
                c.p.gl.lock().unwrap().current.remove(&tid);
                true
            } else {
                let known = c.p.gl.lock().unwrap().contexts.contains_key(&client);
                if known {
                    c.p.gl.lock().unwrap().current.insert(tid, client);
                    let synced = c.p.gl.lock().unwrap().contexts.get(&client).is_some_and(|x| x.synced);
                    if !synced {
                        sync_client(c, client)?;
                    }
                    if let Some(x) = c.p.gl.lock().unwrap().contexts.get_mut(&client) {
                        x.hdc = draw;
                    }
                    fit_surface(c, draw)?;
                }
                known
            };
            let r = p.ret();
            p.set_u32(c, r, ok as u32)?;
        }
        "wglSwapBuffers" => {
            let hdc = p.u32(c, 1)?;
            let ok = swap(c, hdc)?;
            let r = p.ret();
            p.set_u32(c, r, ok as u32)?;
        }
        "wglSwapIntervalEXT" => {
            c.p.gl.lock().unwrap().swap_interval = p.i32(c, 1)?;
            p.set_u32(c, 2, 1)?;
        }
        "wglGetSwapIntervalEXT" => {
            let v = c.p.gl.lock().unwrap().swap_interval;
            p.set_u32(c, 1, v as u32)?;
        }

        // --- queries
        "glGetString" => {
            let pname = p.u32(c, 1)?;
            let s: Vec<u8> = if pname == 0x1f03 {
                // GL_EXTENSIONS: the ones the client context exposes.
                let client = c.p.gl.lock().unwrap().current.get(&c.t.tid).copied().unwrap_or(0);
                let mut out = Vec::new();
                if client != 0 {
                    let on = c.g.bytes(client + table::client::EXTENSIONS, table::EXTENSIONS.len())?;
                    for (i, e) in table::EXTENSIONS.iter().enumerate() {
                        if on[i] != 0 && (i < table::WGL_FIRST_EXTENSION || e.starts_with("WGL_EXT_")) {
                            out.extend_from_slice(e.as_bytes());
                            out.push(b' ');
                        }
                    }
                }
                out
            } else {
                let mut r = ask(c, op::GET_STRING, &[pname], &[])?;
                let host = r.blob();
                // The version, and the shading language's, as far as this
                // serves (`generated::MAX_VERSION`), the host's string after.
                let client = c.p.gl.lock().unwrap().current.get(&c.t.tid).copied().unwrap_or(0);
                let v = c.p.gl.lock().unwrap().contexts.get(&client).map(|x| (x.major, x.minor));
                match (pname, v) {
                    (0x1f02, Some((ma, mi))) => format!("{ma}.{mi} (Compatibility Profile) ({})", String::from_utf8_lossy(&host)).into_bytes(),
                    (0x8b8c, Some((ma, mi))) => {
                        let glsl = match (ma, mi) {
                            (2, 0) => "1.10",
                            (2, _) => "1.20",
                            (3, 0) => "1.30",
                            (3, 1) => "1.40",
                            (3, 2) => "1.50",
                            (3, _) => "3.30",
                            _ => "",
                        };
                        let glsl = if ma >= 4 { format!("{ma}.{mi}0") } else { glsl.to_string() };
                        if glsl.is_empty() { host } else { glsl.into_bytes() }
                    }
                    _ => host,
                }
            };
            let at = intern(c, &s)?;
            p.set_ptr(c, 2, at)?;
        }
        "glGetIntegerv" => {
            let (pname, out) = (p.u32(c, 1)?, p.ptr(c, 2)?);
            let local = {
                let s = c.p.gl.lock().unwrap();
                match pname {
                    0x8894 => Some(s.array_buffer),   // GL_ARRAY_BUFFER_BINDING
                    0x8895 => Some(s.element_buffer), // GL_ELEMENT_ARRAY_BUFFER_BINDING
                    _ => None,
                }
            };
            if let Some(v) = local {
                c.g.w32(out, v)?;
            } else {
                // A blob of as many values as the pname has (the host sizes
                // it with the table generated::pname_count is, plus the core
                // multi-valued ones).
                let mut r = ask(c, op::GET_INTEGERV, &[pname], &[])?;
                let values = r.blob();
                let n = match pname {
                    0x0ba2 | 0x0c10 | 0x0c22 | 0x0c23 => 4, // viewport, scissor box, clear color, write mask
                    0x0d3a | 0x846e | 0x846d | 0x0b70 | 0x0b12 | 0x0b22 | 0x825d | 0x0b40 => 2,
                    0x91be | 0x91bf => 3,                    // compute work group count, size
                    _ => generated::pname_count(pname) as usize,
                };
                if out != 0 && values.len() == 4 * n {
                    c.g.write(out, &values)?;
                }
            }
        }
        "glGetError" => {
            let mut r = ask(c, op::GET_ERROR, &[], &[])?;
            let v = r.u32();
            p.set_u32(c, 1, v)?;
        }
        "glFlush" => {
            flush(c)?;
        }
        "glFinish" => {
            let _ = ask(c, op::FINISH, &[], &[])?;
        }

        // --- state
        "glEnable" => cmd(c, op::ENABLE, &[p.u32(c, 1)?], &[])?,
        "glDisable" => cmd(c, op::DISABLE, &[p.u32(c, 1)?], &[])?,
        "glBlendFunc" => cmd(c, op::BLEND_FUNC, &[p.u32(c, 1)?, p.u32(c, 2)?], &[])?,
        "glBlendFuncSeparate" => {
            cmd(c, op::BLEND_FUNC_SEPARATE, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?, p.u32(c, 4)?], &[])?
        }
        "glBlendEquation" => cmd(c, op::BLEND_EQUATION, &[p.u32(c, 1)?], &[])?,
        "glDepthFunc" => cmd(c, op::DEPTH_FUNC, &[p.u32(c, 1)?], &[])?,
        "glDepthMask" => cmd(c, op::DEPTH_MASK, &[p.u8(c, 1)? as u32], &[])?,
        "glColorMask" => cmd(c, op::COLOR_MASK, &[p.u8(c, 1)? as u32, p.u8(c, 2)? as u32, p.u8(c, 3)? as u32, p.u8(c, 4)? as u32], &[])?,
        "glCullFace" => cmd(c, op::CULL_FACE, &[p.u32(c, 1)?], &[])?,
        "glFrontFace" => cmd(c, op::FRONT_FACE, &[p.u32(c, 1)?], &[])?,
        "glHint" => cmd(c, op::HINT, &[p.u32(c, 1)?, p.u32(c, 2)?], &[])?,
        "glLineWidth" => cmd(c, op::LINE_WIDTH, &[p.u32(c, 1)?], &[])?,
        "glScissor" => cmd(c, op::SCISSOR, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?, p.u32(c, 4)?], &[])?,
        "glViewport" => cmd(c, op::VIEWPORT, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?, p.u32(c, 4)?], &[])?,
        "glClearColor" => cmd(c, op::CLEAR_COLOR, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?, p.u32(c, 4)?], &[])?,
        "glClearDepth" => cmd(c, op::CLEAR_DEPTH, &[f(p.f64(c, 1)? as f32)], &[])?,
        "glClearStencil" => cmd(c, op::CLEAR_STENCIL, &[p.u32(c, 1)?], &[])?,
        "glClear" => cmd(c, op::CLEAR, &[p.u32(c, 1)?], &[])?,
        "glPixelStorei" => {
            let (pn, v) = (p.u32(c, 1)?, p.i32(c, 2)?);
            set_store(&mut c.p.gl.lock().unwrap(), pn, v);
            cmd(c, op::PIXEL_STOREI, &[pn, v as u32], &[])?;
        }

        // --- fixed function
        "glMatrixMode" => cmd(c, op::MATRIX_MODE, &[p.u32(c, 1)?], &[])?,
        "glLoadIdentity" => cmd(c, op::LOAD_IDENTITY, &[], &[])?,
        "glPushMatrix" => cmd(c, op::PUSH_MATRIX, &[], &[])?,
        "glPopMatrix" => cmd(c, op::POP_MATRIX, &[], &[])?,
        "glTranslatef" => cmd(c, op::TRANSLATEF, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?], &[])?,
        "glRotatef" => cmd(c, op::ROTATEF, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?, p.u32(c, 4)?], &[])?,
        "glScalef" => cmd(c, op::SCALEF, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?], &[])?,
        "glOrtho" | "glFrustum" => {
            let mut w = Vec::new();
            for i in 1..=6 {
                w.extend_from_slice(&proto::Batch::words64(p.f64(c, i)?.to_bits()));
            }
            cmd(c, if name == "glOrtho" { op::ORTHO } else { op::FRUSTUM }, &w, &[])?;
        }
        "glLoadMatrixf" | "glMultMatrixf" => {
            let m = p.ptr(c, 1)?;
            let w: Vec<u32> = (0..16).map(|i| c.g.u32(m + 4 * i)).collect::<R<_>>()?;
            cmd(c, if name == "glLoadMatrixf" { op::LOAD_MATRIXF } else { op::MULT_MATRIXF }, &w, &[])?;
        }
        "glColor4f" => cmd(c, op::COLOR4F, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?, p.u32(c, 4)?], &[])?,
        "glColor4ub" => {
            let v: Vec<u32> = (1..=4).map(|i| p.u8(c, i).map(|b| f(b as f32 / 255.0))).collect::<R<_>>()?;
            cmd(c, op::COLOR4F, &v, &[])?;
        }
        "glColor3f" => cmd(c, op::COLOR4F, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?, f(1.0)], &[])?,
        "glTexEnvi" => cmd(c, op::TEX_ENVI, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?], &[])?,
        "glTexEnvf" => cmd(c, op::TEX_ENVI, &[p.u32(c, 1)?, p.u32(c, 2)?, p.f32(c, 3)? as i32 as u32], &[])?,
        "glAlphaFunc" => cmd(c, op::ALPHA_FUNC, &[p.u32(c, 1)?, p.u32(c, 2)?], &[])?,
        "glShadeModel" => cmd(c, op::SHADE_MODEL, &[p.u32(c, 1)?], &[])?,
        "glPushAttrib" => cmd(c, op::PUSH_ATTRIB, &[p.u32(c, 1)?], &[])?,
        "glPopAttrib" => cmd(c, op::POP_ATTRIB, &[], &[])?,
        "glBegin" => cmd(c, op::BEGIN, &[p.u32(c, 1)?], &[])?,
        "glEnd" => cmd(c, op::END, &[], &[])?,
        "glVertex3f" => cmd(c, op::VERTEX3F, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?], &[])?,
        "glVertex2f" => cmd(c, op::VERTEX3F, &[p.u32(c, 1)?, p.u32(c, 2)?, 0], &[])?,

        // --- client arrays
        "glEnableClientState" | "glDisableClientState" => {
            let cap = p.u32(c, 1)?;
            let on = name == "glEnableClientState";
            let Some(i) = array_index(c, cap) else {
                return Err(c.unimplemented(&format!("{name}({cap:#x})")));
            };
            c.p.gl.lock().unwrap().arrays[i].enabled = on;
            cmd(c, if on { op::ENABLE_CLIENT_STATE } else { op::DISABLE_CLIENT_STATE }, &[cap], &[])?;
        }
        "glVertexPointer" => set_pointer(c, VERTEX, op::VERTEX_POINTER, p.i32(c, 1)?, p.u32(c, 2)?, p.i32(c, 3)?, p.ptr(c, 4)?)?,
        "glColorPointer" => set_pointer(c, COLOR, op::COLOR_POINTER, p.i32(c, 1)?, p.u32(c, 2)?, p.i32(c, 3)?, p.ptr(c, 4)?)?,
        "glTexCoordPointer" => {
            let i = TEXCOORD + c.p.gl.lock().unwrap().client_texture;
            set_pointer(c, i, op::TEX_COORD_POINTER, p.i32(c, 1)?, p.u32(c, 2)?, p.i32(c, 3)?, p.ptr(c, 4)?)?
        }
        "glClientActiveTexture" => {
            let unit = p.u32(c, 1)?.wrapping_sub(0x84c0) as usize;
            if unit >= ARRAYS - TEXCOORD {
                return Err(c.unimplemented(&format!("texture unit {unit} for client arrays")));
            }
            c.p.gl.lock().unwrap().client_texture = unit;
            cmd(c, op::CLIENT_ACTIVE_TEXTURE, &[0x84c0 + unit as u32], &[])?;
        }
        "glDrawArrays" => {
            let (mode, first, count) = (p.u32(c, 1)?, p.i32(c, 2)?, p.i32(c, 3)?);
            if first >= 0 && count > 0 {
                upload_arrays(c, first as usize + count as usize)?;
            }
            cmd(c, op::DRAW_ARRAYS, &[mode, first as u32, count as u32], &[])?;
        }
        "glNormalPointer" => set_typed_pointer(c, NORMAL, 3, p.u32(c, 1)?, p.i32(c, 2)?, p.ptr(c, 3)?)?,
        "glFogCoordPointer" => set_typed_pointer(c, FOG, 1, p.u32(c, 1)?, p.i32(c, 2)?, p.ptr(c, 3)?)?,
        "glSecondaryColorPointer" => set_typed_pointer(c, SECONDARY, p.i32(c, 1)?, p.u32(c, 2)?, p.i32(c, 3)?, p.ptr(c, 4)?)?,

        // --- generic vertex attributes and the other draws
        "glVertexAttribPointer" | "glVertexAttribIPointer" | "glVertexAttribLPointer" => {
            let index = p.u32(c, 1)? as usize;
            let (normalized, stride, pointer, kind) = if name == "glVertexAttribPointer" {
                (p.u8(c, 4)? != 0, p.i32(c, 5)?, p.ptr(c, 6)?, 0)
            } else {
                (false, p.i32(c, 4)?, p.ptr(c, 5)?, if name == "glVertexAttribIPointer" { 1 } else { 2 })
            };
            if index >= ATTRIBS {
                return Err(c.unimplemented(&format!("vertex attribute {index}")));
            }
            let a = {
                let mut s = c.p.gl.lock().unwrap();
                let buffer = s.array_buffer;
                let a = &mut s.attribs[index];
                a.size = p.i32(c, 2)?;
                a.ty = p.u32(c, 3)?;
                a.normalized = normalized;
                a.stride = stride;
                a.pointer = pointer;
                a.buffer = buffer;
                a.kind = kind;
                *a
            };
            if a.buffer != 0 {
                send_attrib_pointer(c, index, &a, pointer)?;
            }
        }
        "glEnableVertexAttribArray" | "glDisableVertexAttribArray" => {
            let index = p.u32(c, 1)?;
            let on = name == "glEnableVertexAttribArray";
            if let Some(a) = c.p.gl.lock().unwrap().attribs.get_mut(index as usize) {
                a.enabled = on;
            }
            cmd(c, if on { op::ENABLE_VERTEX_ATTRIB_ARRAY } else { op::DISABLE_VERTEX_ATTRIB_ARRAY }, &[index], &[])?;
        }
        "glVertexAttribDivisor" => {
            let (index, divisor) = (p.u32(c, 1)?, p.u32(c, 2)?);
            if let Some(a) = c.p.gl.lock().unwrap().attribs.get_mut(index as usize) {
                a.divisor = divisor;
            }
            planned(c, name, p)?;
        }
        "glDrawArraysInstanced" | "glDrawArraysInstancedBaseInstance" => {
            let (first, count, instances) = (p.i32(c, 2)?, p.i32(c, 3)?, p.i32(c, 4)?);
            if first >= 0 && count > 0 {
                upload_arrays_instanced(c, first as usize + count as usize, instances.max(1) as usize)?;
            }
            planned(c, name, p)?;
        }
        "glMultiDrawArrays" => {
            let (first, count, n) = (p.ptr(c, 2)?, p.ptr(c, 3)?, p.i32(c, 4)?);
            let mut reach = 0usize;
            for i in 0..n.max(0) as u64 {
                let (f, k) = (c.g.u32(first + 4 * i)? as i32, c.g.u32(count + 4 * i)? as i32);
                if f >= 0 && k > 0 {
                    reach = reach.max(f as usize + k as usize);
                }
            }
            upload_arrays(c, reach)?;
            planned(c, name, p)?;
        }
        "glDrawElements" => {
            let v = raw_params(c, p)?;
            draw_elements(c, name, v, 3, p.i32(c, 2)?, p.u32(c, 3)?, 0, 1)?;
        }
        "glDrawRangeElements" => {
            let v = raw_params(c, p)?;
            draw_elements(c, name, v, 5, p.i32(c, 4)?, p.u32(c, 5)?, 0, 1)?;
        }
        "glDrawElementsBaseVertex" => {
            let v = raw_params(c, p)?;
            draw_elements(c, name, v, 3, p.i32(c, 2)?, p.u32(c, 3)?, p.i32(c, 5)? as i64, 1)?;
        }
        "glDrawRangeElementsBaseVertex" => {
            let v = raw_params(c, p)?;
            draw_elements(c, name, v, 5, p.i32(c, 4)?, p.u32(c, 5)?, p.i32(c, 7)? as i64, 1)?;
        }
        "glDrawElementsInstanced" | "glDrawElementsInstancedBaseInstance" => {
            let v = raw_params(c, p)?;
            draw_elements(c, name, v, 3, p.i32(c, 2)?, p.u32(c, 3)?, 0, p.i32(c, 5)?.max(1) as usize)?;
        }
        "glDrawElementsInstancedBaseVertex" | "glDrawElementsInstancedBaseVertexBaseInstance" => {
            let v = raw_params(c, p)?;
            draw_elements(c, name, v, 3, p.i32(c, 2)?, p.u32(c, 3)?, p.i32(c, 6)? as i64, p.i32(c, 5)?.max(1) as usize)?;
        }
        "glDrawArraysIndirect" | "glDrawElementsIndirect" | "glMultiDrawArraysIndirect" | "glMultiDrawElementsIndirect" => {
            // The commands are in GL_DRAW_INDIRECT_BUFFER: the pointer is an
            // offset into it.
            let v = raw_params(c, p)?;
            send_pointer_plan(c, name, &v)?;
        }

        // --- object names the host generates
        "glGenQueries" => gen_names(c, op::GEN_QUERIES, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glDeleteQueries" => delete_names(c, op::DELETE_QUERIES, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glGenVertexArrays" => gen_names(c, op::GEN_VERTEX_ARRAYS, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glDeleteVertexArrays" => delete_names(c, op::DELETE_VERTEX_ARRAYS, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glBindVertexArray" => cmd(c, op::BIND_VERTEX_ARRAY, &[p.u32(c, 1)?], &[])?,
        "glGenSamplers" => gen_names(c, op::GEN_SAMPLERS, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glDeleteSamplers" => delete_names(c, op::DELETE_SAMPLERS, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glGenTransformFeedbacks" => gen_names(c, op::GEN_TRANSFORM_FEEDBACKS, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glDeleteTransformFeedbacks" => delete_names(c, op::DELETE_TRANSFORM_FEEDBACKS, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glGenLists" => {
            let mut r = ask(c, op::GEN_LISTS, &[p.u32(c, 1)?], &[])?;
            let base = r.u32();
            let at = p.ret();
            p.set_u32(c, at, base)?;
        }
        "glNewList" => cmd(c, op::NEW_LIST, &[p.u32(c, 1)?, p.u32(c, 2)?], &[])?,
        "glEndList" => cmd(c, op::END_LIST, &[], &[])?,
        "glDeleteLists" => cmd(c, op::DELETE_LISTS, &[p.u32(c, 1)?, p.u32(c, 2)?], &[])?,
        "glBindBufferBase" | "glBindBufferRange" => {
            let (t, b) = (p.u32(c, 1)?, p.u32(c, 3)?);
            c.p.gl.lock().unwrap().bound.insert(t, b);
            planned(c, name, p)?;
        }
        "glPixelStoref" => {
            let (pn, v) = (p.u32(c, 1)?, p.f32(c, 2)?.round() as i32);
            set_store(&mut c.p.gl.lock().unwrap(), pn, v);
            cmd(c, op::PIXEL_STOREI, &[pn, v as u32], &[])?;
        }
        "glGetStringi" => {
            let mut r = ask(c, op::GET_STRING_I, &[p.u32(c, 1)?, p.u32(c, 2)?], &[])?;
            let s = r.blob();
            let at = intern(c, &s)?;
            p.set_ptr(c, 3, at)?;
        }

        // --- mapping buffers: a copy in guest memory, written back
        "glMapBuffer" => {
            let (t, access) = (p.u32(c, 1)?, p.u32(c, 2)?);
            // GL_READ_ONLY, GL_WRITE_ONLY, GL_READ_WRITE
            let addr = map_buffer(c, t, 0, None, access != 0x88b9, access != 0x88b8, false)?;
            p.set_ptr(c, 3, addr)?;
        }
        "glMapBufferRange" => {
            let (t, off, len, access) = (p.u32(c, 1)?, p.isize(c, 2)?, p.isize(c, 3)?, p.u32(c, 4)?);
            // GL_MAP_READ_BIT, _WRITE_BIT, _INVALIDATE_RANGE_BIT,
            // _INVALIDATE_BUFFER_BIT, _FLUSH_EXPLICIT_BIT
            let read = access & 1 != 0 && access & 0xc == 0;
            let addr = map_buffer(c, t, off.max(0) as u64, Some(len.max(0) as u64), read, access & 2 != 0, access & 0x10 != 0)?;
            p.set_ptr(c, 5, addr)?;
        }
        "glFlushMappedBufferRange" => {
            let (t, off, len) = (p.u32(c, 1)?, p.isize(c, 2)?, p.isize(c, 3)?);
            let m = {
                let s = c.p.gl.lock().unwrap();
                s.bound.get(&t).and_then(|b| s.maps.get(b)).copied()
            };
            if let Some(m) = m {
                write_back_map(c, &m, off.max(0) as u64, len.max(0) as u64)?;
            }
        }
        "glUnmapBuffer" => {
            let t = p.u32(c, 1)?;
            let m = {
                let mut s = c.p.gl.lock().unwrap();
                let b = s.bound.get(&t).copied().unwrap_or(0);
                s.maps.remove(&b)
            };
            if let Some(m) = m {
                if m.write && !m.explicit_flush {
                    write_back_map(c, &m, 0, m.len)?;
                }
            }
            let at = p.ret();
            p.set_u8(c, at, m.is_some() as u8)?;
        }
        "glGetBufferPointerv" => {
            let (t, out) = (p.u32(c, 1)?, p.ptr(c, 3)?);
            let addr = {
                let s = c.p.gl.lock().unwrap();
                s.bound.get(&t).and_then(|b| s.maps.get(b)).map(|m| m.addr).unwrap_or(0)
            };
            if out != 0 {
                c.g.wptr(out, addr)?;
            }
        }

        // --- sync objects: the command stream runs in order, so a fence is
        // signaled by the time anything can wait on it.
        // opengl32 names the sync itself (its `ret` arrives as the client
        // sync's handle, which a success leaves as it is).
        "glFenceSync" => {}
        "glClientWaitSync" => {
            let at = p.ret();
            p.set_u32(c, at, 0x911a)?; // GL_ALREADY_SIGNALED
        }
        "glWaitSync" | "glDeleteSync" => {}
        "glIsSync" => {
            let v = p.ptr(c, 1)? != 0;
            let at = p.ret();
            p.set_u8(c, at, v as u8)?;
        }
        "glGetSynciv" => {
            let (pname, count, length, values) = (p.u32(c, 2)?, p.i32(c, 3)?, p.ptr(c, 4)?, p.ptr(c, 5)?);
            let v = match pname {
                0x9112 => 0x9116, // GL_OBJECT_TYPE: GL_SYNC_FENCE
                0x9113 => 0x9117, // GL_SYNC_CONDITION: GL_SYNC_GPU_COMMANDS_COMPLETE
                0x9114 => 0x9119, // GL_SYNC_STATUS: GL_SIGNALED
                _ => 0,           // GL_SYNC_FLAGS
            };
            if count > 0 && values != 0 {
                c.g.w32(values, v)?;
            }
            if length != 0 {
                c.g.w32(length, (count > 0) as u32)?;
            }
        }

        // --- reading textures back
        "glGetTexImage" => {
            let (t, level, format, ty, pixels) = (p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?, p.u32(c, 4)?, p.ptr(c, 5)?);
            let pbo = c.p.gl.lock().unwrap().bound.get(&0x88eb).copied().unwrap_or(0) != 0;
            let [lo, hi] = proto::Batch::words64(pixels);
            cmd(c, GL44_GET_TEX_IMAGE, &[t, level, format, ty, pbo as u32, lo, hi], &[])?;
            let mut r = Reply { b: flush(c)?, at: 0 };
            if !pbo && pixels != 0 {
                let (offset, row_stride, row_bytes, rows, image_stride, images) =
                    (r.u32() as u64, r.u32() as u64, r.u32() as u64, r.u32() as u64, r.u32() as u64, r.u32() as u64);
                let data = r.blob();
                for image in 0..images {
                    for row in 0..rows {
                        let at = image * image_stride + row * row_stride;
                        if at + row_bytes > data.len() as u64 {
                            break;
                        }
                        c.g.write(pixels + offset + at, &data[at as usize..(at + row_bytes) as usize])?;
                    }
                }
            }
        }
        "glGetCompressedTexImage" => {
            let (t, level, img) = (p.u32(c, 1)?, p.u32(c, 2)?, p.ptr(c, 3)?);
            let pbo = c.p.gl.lock().unwrap().bound.get(&0x88eb).copied().unwrap_or(0) != 0;
            let [lo, hi] = proto::Batch::words64(img);
            cmd(c, GL44_GET_COMPRESSED_TEX_IMAGE, &[t, level, pbo as u32, lo, hi], &[])?;
            let mut r = Reply { b: flush(c)?, at: 0 };
            if !pbo && img != 0 {
                let data = r.blob();
                c.g.write(img, &data)?;
            }
        }

        // --- buffers
        "glGenBuffers" => gen_names(c, op::GEN_BUFFERS, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glDeleteBuffers" => delete_names(c, op::DELETE_BUFFERS, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glBindBuffer" => {
            let (t, b) = (p.u32(c, 1)?, p.u32(c, 2)?);
            {
                let mut s = c.p.gl.lock().unwrap();
                match t {
                    0x8892 => s.array_buffer = b,
                    0x8893 => s.element_buffer = b,
                    0x88ec => s.unpack_buffer = b,
                    _ => {}
                }
                s.bound.insert(t, b);
            }
            cmd(c, op::BIND_BUFFER, &[t, b], &[])?;
        }
        "glBufferData" => {
            let (t, size, data, usage) = (p.u32(c, 1)?, p.isize(c, 2)?, p.ptr(c, 3)?, p.u32(c, 4)?);
            let bytes = if data != 0 { c.g.bytes(data, size.max(0) as usize)? } else { Vec::new() };
            let [lo, hi] = proto::Batch::words64(size.max(0) as u64);
            cmd_blob(c, op::BUFFER_DATA, &[t, usage, lo, hi, (data != 0) as u32], &bytes)?;
        }
        "glBufferSubData" => {
            let (t, off, size, data) = (p.u32(c, 1)?, p.isize(c, 2)?, p.isize(c, 3)?, p.ptr(c, 4)?);
            let bytes = c.g.bytes(data, size.max(0) as usize)?;
            let [ol, oh] = proto::Batch::words64(off.max(0) as u64);
            let [sl, sh] = proto::Batch::words64(size.max(0) as u64);
            cmd_blob(c, op::BUFFER_SUB_DATA, &[t, ol, oh, sl, sh], &bytes)?;
        }

        // --- textures
        "glGenTextures" => gen_names(c, op::GEN_TEXTURES, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glDeleteTextures" => delete_names(c, op::DELETE_TEXTURES, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glBindTexture" => cmd(c, op::BIND_TEXTURE, &[p.u32(c, 1)?, p.u32(c, 2)?], &[])?,
        "glActiveTexture" => cmd(c, op::ACTIVE_TEXTURE, &[p.u32(c, 1)?], &[])?,
        "glTexParameteri" => cmd(c, op::TEX_PARAMETER_I, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?], &[])?,
        "glTexParameterf" => cmd(c, op::TEX_PARAMETER_F, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?], &[])?,
        "glTexImage2D" | "glTexSubImage2D" => {
            // TexImage2D { target, level, internal, w, h, border, format, type, pixels }
            // TexSubImage2D { target, level, x, y, w, h, format, type, pixels }
            let v: Vec<u32> = (1..=8).map(|i| p.u32(c, i)).collect::<R<_>>()?;
            let pixels = p.ptr(c, 9)?;
            let (w, h, format, ty) = if name == "glTexImage2D" {
                (v[3] as i32, v[4] as i32, v[6], v[7])
            } else {
                (v[4] as i32, v[5] as i32, v[6], v[7])
            };
            let unpack = c.p.gl.lock().unwrap().unpack;
            let data = if pixels != 0 {
                let Some(n) = transfer_bytes(format, ty, w, h, unpack) else {
                    return Err(c.unimplemented(&format!("{name} with format {format:#x} type {ty:#x}")));
                };
                c.g.bytes(pixels, n)?
            } else {
                Vec::new()
            };
            let mut words = v.clone();
            // { pbo, pointer:u64 } then the blob; the pointer only says
            // whether there was one.
            words.extend_from_slice(&[0, (pixels != 0) as u32, 0]);
            let opcode = if name == "glTexImage2D" { op::TEX_IMAGE_2D } else { op::TEX_SUB_IMAGE_2D };
            cmd_blob(c, opcode, &words, &data)?;
        }

        // --- framebuffers
        "glGenFramebuffers" => gen_names(c, op::GEN_FRAMEBUFFERS, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glDeleteFramebuffers" => delete_names(c, op::DELETE_FRAMEBUFFERS, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glGenRenderbuffers" => gen_names(c, op::GEN_RENDERBUFFERS, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glDeleteRenderbuffers" => delete_names(c, op::DELETE_RENDERBUFFERS, p.i32(c, 1)?, p.ptr(c, 2)?)?,
        "glBindFramebuffer" => {
            let (t, fb) = (p.u32(c, 1)?, p.u32(c, 2)?);
            if t == 0x8d40 || t == 0x8ca8 {
                c.p.gl.lock().unwrap().read_fbo = fb; // GL_FRAMEBUFFER / GL_READ_FRAMEBUFFER
            }
            cmd(c, op::BIND_FRAMEBUFFER, &[t, fb], &[])?;
        }
        "glBindRenderbuffer" => cmd(c, op::BIND_RENDERBUFFER, &[p.u32(c, 1)?, p.u32(c, 2)?], &[])?,
        "glRenderbufferStorage" => cmd(c, op::RENDERBUFFER_STORAGE, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?, p.u32(c, 4)?], &[])?,
        "glFramebufferRenderbuffer" => {
            cmd(c, op::FRAMEBUFFER_RENDERBUFFER, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?, p.u32(c, 4)?], &[])?
        }
        "glFramebufferTexture2D" => {
            cmd(c, op::FRAMEBUFFER_TEXTURE_2D, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?, p.u32(c, 4)?, p.u32(c, 5)?], &[])?
        }
        "glCheckFramebufferStatus" => {
            let mut r = ask(c, op::CHECK_FRAMEBUFFER_STATUS, &[p.u32(c, 1)?], &[])?;
            let v = r.u32();
            p.set_u32(c, 2, v)?;
        }

        // --- shaders
        "glCreateShader" => {
            let n = take_name(c);
            cmd(c, op::CREATE_SHADER, &[n, p.u32(c, 1)?], &[])?;
            p.set_u32(c, 2, n)?;
        }
        "glShaderSource" => {
            let (shader, count, strings, lengths) = (p.u32(c, 1)?, p.i32(c, 2)?, p.ptr(c, 3)?, p.ptr(c, 4)?);
            let mut src = Vec::new();
            let ps = c.p.abi.ptr_size();
            for i in 0..count.max(0) as u64 {
                let s = c.g.ptr(strings + ps * i)?;
                let len = if lengths != 0 { c.g.u32(lengths + 4 * i)? as i32 } else { -1 };
                if len >= 0 {
                    src.extend_from_slice(&c.g.bytes(s, len as usize)?);
                } else {
                    src.extend_from_slice(&cstr(c, s)?);
                }
            }
            cmd_blob(c, op::SHADER_SOURCE, &[shader], &src)?;
        }
        "glCompileShader" => cmd(c, op::COMPILE_SHADER, &[p.u32(c, 1)?], &[])?,
        "glDeleteShader" => cmd(c, op::DELETE_SHADER, &[p.u32(c, 1)?], &[])?,
        "glCreateProgram" => {
            let n = take_name(c);
            cmd(c, op::CREATE_PROGRAM, &[n], &[])?;
            p.set_u32(c, 1, n)?;
        }
        "glAttachShader" => cmd(c, op::ATTACH_SHADER, &[p.u32(c, 1)?, p.u32(c, 2)?], &[])?,
        "glDetachShader" => cmd(c, op::DETACH_SHADER, &[p.u32(c, 1)?, p.u32(c, 2)?], &[])?,
        "glLinkProgram" => cmd(c, op::LINK_PROGRAM, &[p.u32(c, 1)?], &[])?,
        "glUseProgram" => cmd(c, op::USE_PROGRAM, &[p.u32(c, 1)?], &[])?,
        "glDeleteProgram" => cmd(c, op::DELETE_PROGRAM, &[p.u32(c, 1)?], &[])?,
        // ARB_shader_objects' one delete for both kinds (opengl32 makes it
        // for a context's objects): programs and shaders share one name
        // space here, so whichever it is, the other delete finds nothing.
        "glDeleteObjectARB" => {
            let n = p.u32(c, 1)?;
            cmd(c, op::DELETE_PROGRAM, &[n], &[])?;
            cmd(c, op::DELETE_SHADER, &[n], &[])?;
        }
        "glGetShaderiv" | "glGetProgramiv" => {
            let (obj, pname, out) = (p.u32(c, 1)?, p.u32(c, 2)?, p.ptr(c, 3)?);
            let opcode = if name == "glGetShaderiv" { op::GET_SHADER_IV } else { op::GET_PROGRAM_IV };
            let mut r = ask(c, opcode, &[obj, pname], &[])?;
            let v = r.u32();
            c.g.w32(out, v)?;
        }
        "glGetShaderInfoLog" | "glGetProgramInfoLog" => {
            let (obj, max, len_p, out) = (p.u32(c, 1)?, p.i32(c, 2)?, p.ptr(c, 3)?, p.ptr(c, 4)?);
            let opcode = if name == "glGetShaderInfoLog" { op::GET_SHADER_INFO_LOG } else { op::GET_PROGRAM_INFO_LOG };
            let mut r = ask(c, opcode, &[obj], &[])?;
            let log = r.blob();
            let n = log.len().min((max.max(1) - 1) as usize);
            if out != 0 && max > 0 {
                c.g.write(out, &log[..n])?;
                c.g.w8(out + n as u64, 0)?;
            }
            c.g.out32(len_p, n as u32)?;
        }
        "glGetUniformLocation" | "glGetAttribLocation" => {
            let (prog, s) = (p.u32(c, 1)?, p.ptr(c, 2)?);
            let n = cstr(c, s)?;
            let opcode = if name == "glGetUniformLocation" { op::GET_UNIFORM_LOCATION } else { op::GET_ATTRIB_LOCATION };
            let mut t = (n.len() as u32).to_le_bytes().to_vec();
            t.extend_from_slice(&n);
            let mut r = ask(c, opcode, &[prog], &t)?;
            let v = r.u32();
            p.set_u32(c, 3, v)?;
        }
        "glBindAttribLocation" => {
            let (prog, idx, s) = (p.u32(c, 1)?, p.u32(c, 2)?, p.ptr(c, 3)?);
            let n = cstr(c, s)?;
            cmd_blob(c, op::BIND_ATTRIB_LOCATION, &[prog, idx], &n)?;
        }
        "glUniform1i" => cmd(c, op::UNIFORM_1I, &[p.u32(c, 1)?, p.u32(c, 2)?], &[])?,
        "glUniform1f" => cmd(c, op::UNIFORM_1F, &[p.u32(c, 1)?, p.u32(c, 2)?], &[])?,
        "glUniform2f" => cmd(c, op::UNIFORM_2F, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?], &[])?,
        "glUniform3f" => cmd(c, op::UNIFORM_3F, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?, p.u32(c, 4)?], &[])?,
        "glUniform4f" => {
            cmd(c, op::UNIFORM_4F, &[p.u32(c, 1)?, p.u32(c, 2)?, p.u32(c, 3)?, p.u32(c, 4)?, p.u32(c, 5)?], &[])?
        }
        "glUniform1fv" | "glUniform2fv" | "glUniform3fv" | "glUniform4fv" | "glUniform1iv" | "glUniform2iv"
        | "glUniform3iv" | "glUniform4iv" => {
            let (loc, count, v) = (p.u32(c, 1)?, p.i32(c, 2)?, p.ptr(c, 3)?);
            let n: usize = name[9..10].parse().unwrap_or(1);
            let data = c.g.bytes(v, 4 * n * count.max(0) as usize)?;
            let opcode = match &name[9..] {
                "1fv" => op::UNIFORM_1FV,
                "2fv" => op::UNIFORM_2FV,
                "3fv" => op::UNIFORM_3FV,
                "4fv" => op::UNIFORM_4FV,
                "1iv" => op::UNIFORM_1IV,
                "2iv" => op::UNIFORM_2IV,
                "3iv" => op::UNIFORM_3IV,
                _ => op::UNIFORM_4IV,
            };
            cmd_blob(c, opcode, &[loc, count as u32], &data)?;
        }
        "glUniformMatrix4fv" => {
            let (loc, count, transpose, v) = (p.u32(c, 1)?, p.i32(c, 2)?, p.u8(c, 3)?, p.ptr(c, 4)?);
            let data = c.g.bytes(v, 64 * count.max(0) as usize)?;
            cmd_blob(c, op::UNIFORM_MATRIX4FV, &[loc, count as u32, transpose as u32], &data)?;
        }

        // --- reading back
        "glReadPixels" => {
            let v: Vec<u32> = (1..=6).map(|i| p.u32(c, i)).collect::<R<_>>()?;
            let out = p.ptr(c, 7)?;
            let mut r = ask(c, op::READ_PIXELS, &[v[0], v[1], v[2], v[3], v[4], v[5], 0, 0, 0], &[])?;
            let offset = r.u32() as u64;
            let px = r.blob();
            c.g.write(out + offset, &px)?;
        }

        // Every other registry command, by its plan.
        _ => return planned(c, name, p),
    }
    Ok(())
}

#[allow(dead_code)]
fn _unused(_: Rect, _: Status) {}
