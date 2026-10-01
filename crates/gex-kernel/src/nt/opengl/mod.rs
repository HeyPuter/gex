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

/// A params struct: where each field of a unix call's arguments is, for the
/// process's ABI.
struct Params {
    at: u64,
    offs: Vec<u64>,
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
}

impl Default for PixelStore {
    fn default() -> Self {
        PixelStore { alignment: 4, row_length: 0, skip_pixels: 0, skip_rows: 0 }
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
    next_name: u32,
    /// The default framebuffer's size.
    surface: (i32, i32),
    unpack: PixelStore,
    pack: PixelStore,
    arrays: [Array; ARRAYS],
    client_texture: usize,
    array_buffer: u32,
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
            next_name: 1,
            surface: (0, 0),
            unpack: PixelStore::default(),
            pack: PixelStore::default(),
            arrays: [Array::default(); ARRAYS],
            client_texture: 0,
            array_buffer: 0,
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
    let r = dev.ioctl(&mut c.g.m, GEX_GL_SUBMIT, ring);
    c.p.gl.lock().unwrap().tail = 0;
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
    let mut r = ask(c, op::CONTEXT_CREATE, &[proto::client_api::DESKTOP, 0, 0], &[])?;
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
    c.g.w32(client + f::MAJOR_VERSION, major as u32)?;
    c.g.w32(client + f::MINOR_VERSION, minor as u32)?;
    c.g.w32(client + f::CONTEXT_FLAGS, 0)?;
    c.g.w32(client + f::PROFILE_MASK, if major >= 3 && (major > 3 || minor >= 2) { 2 } else { 0 })?; // compatibility
    let mut on = vec![false; table::EXTENSIONS.len()];
    for name in exts.split_ascii_whitespace() {
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
    let mut r = ask(c, op::READ_PIXELS, &[0, 0, w as u32, h as u32, 0x80e1, 0x1401, 0, 0, 0], &[])?;
    for (pn, cur, want) in packs {
        if cur != want {
            cmd(c, op::PIXEL_STOREI, &[pn, cur], &[])?;
        }
    }
    if read_fbo != 0 {
        cmd(c, op::BIND_FRAMEBUFFER, &[GL_READ_FRAMEBUFFER, read_fbo], &[])?;
    }
    let _offset = r.u32();
    let px = r.blob();
    let stride = w as usize * 4;
    if px.len() < stride * h as usize {
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
        let row = &px[src..src + (x1 - x0) as usize * 4];
        c.g.write(surf.row(sy) + x0 as u64 * 4, row)?;
    }
    gdi::surface::flush(c, g.toplevel);
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
    let (arrays, bound) = {
        let s = c.p.gl.lock().unwrap();
        (s.arrays, s.array_buffer)
    };
    let mut rebound = false;
    for (i, a) in arrays.iter().enumerate() {
        if !a.enabled || a.buffer != 0 || a.pointer == 0 || vertices == 0 {
            continue;
        }
        let elem = element_bytes(a.size, a.ty);
        if elem == 0 {
            return Err(c.unimplemented(&format!("a client array of type {:#x}", a.ty)));
        }
        let stride = if a.stride > 0 { a.stride as usize } else { elem };
        let bytes = (vertices - 1) * stride + elem;
        let data = c.g.bytes(a.pointer, bytes)?;
        let scratch = if a.scratch != 0 {
            a.scratch
        } else {
            let n = take_name(c);
            c.p.gl.lock().unwrap().arrays[i].scratch = n;
            cmd(c, op::GEN_BUFFERS, &[1, n], &[])?;
            n
        };
        const GL_ARRAY_BUFFER: u32 = 0x8892;
        cmd(c, op::BIND_BUFFER, &[GL_ARRAY_BUFFER, scratch], &[])?;
        rebound = true;
        let [lo, hi] = proto::Batch::words64(bytes as u64);
        cmd_blob(c, op::BUFFER_DATA, &[GL_ARRAY_BUFFER, 0x88e8, lo, hi, 1], &data)?; // GL_DYNAMIC_DRAW
        let opcode = match i {
            VERTEX => op::VERTEX_POINTER,
            COLOR => op::COLOR_POINTER,
            _ => {
                cmd(c, op::CLIENT_ACTIVE_TEXTURE, &[0x84c0 + (i - TEXCOORD) as u32], &[])?;
                op::TEX_COORD_POINTER
            }
        };
        cmd(c, opcode, &[a.size as u32, a.ty, a.stride as u32, 0, 0], &[])?;
    }
    if rebound {
        cmd(c, op::BIND_BUFFER, &[0x8892, bound], &[])?;
        let unit = c.p.gl.lock().unwrap().client_texture;
        cmd(c, op::CLIENT_ACTIVE_TEXTURE, &[0x84c0 + unit as u32], &[])?;
    }
    Ok(())
}

fn array_index(c: &Call<'_>, cap: u32) -> Option<usize> {
    match cap {
        0x8074 => Some(VERTEX),
        0x8076 => Some(COLOR),
        0x8078 => Some(TEXCOORD + c.p.gl.lock().unwrap().client_texture),
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
    let p = Params { at: args, offs: layout(sig, c.p.abi) };
    let r = one(c, name, &p);
    if c.p.cfg.trace {
        eprintln!("[nt:{:x}]   opengl32 {name}", c.t.tid);
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
                r.blob()
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
                let mut r = ask(c, op::GET_INTEGERV, &[pname], &[])?;
                let n = match pname {
                    0x0ba2 | 0x0c10 | 0x0c22 | 0x0c23 => 4, // viewport, scissor box, clear color, write mask
                    0x0d3a | 0x846e | 0x846d => 2,          // max viewport dims, aliased line/point ranges
                    _ => 1,
                };
                for i in 0..n {
                    let v = r.u32();
                    c.g.w32(out + 4 * i, v)?;
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
                    _ => {}
                }
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

        _ => return Err(c.unimplemented(&format!("opengl32 {name}"))),
    }
    Ok(())
}

#[allow(dead_code)]
fn _unused(_: Rect, _: Status) {}
