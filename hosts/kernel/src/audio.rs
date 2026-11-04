//! The page's audio device: `/dev/dsp`'s streams, mixed and played by the
//! page's AudioWorklet (page/src/audio.js), which reads this memory through
//! the global kernel's `SharedArrayBuffer`.
//!
//! Each open `/dev/dsp` that writes gets one of [`SLOTS`] rings of stereo
//! `f32` frames at its own rate; the worklet resamples each to the
//! context's rate and sums them, so programs playing at once are mixed. A
//! slot whose stream has written nothing for [`IDLE_MS`] may be taken by
//! another when all are in use.
//!
//! The player is the clock while it plays: it publishes how far it has read
//! each slot, and the OSS device (gex-kernel `vfs/oss.rs`) counts what its
//! stream wrote and the worklet has not read as queued ([`k_audio_pending`]),
//! so a writer paces itself to the audio hardware and what is queued is what
//! the writer keeps queued. A player that has not moved for [`STALL_MS`] (no
//! gesture yet, a background tab) lets the device drain by the wall clock
//! again; what it accepts is still put in the ring, the oldest overwritten.
//!
//! The page writes the context's rate and its own latency (base + output)
//! here, so a program can ask (`SNDCTL_DSP_GEX_INFO`, [`k_audio_info`]) what
//! rate needs no resampling and how far behind the hardware its position is.
//!
//! Capture: with `?audioloop=1` the worklet copies what it plays into the
//! capture ring (a digital loopback: the software round trip measured
//! exactly); `/dev/dsp` reads take from it ([`k_audio_capture`]).
//!
//! The layout is read by JavaScript, so it is fixed (little-endian `u32`
//! words, then frames):
//!
//! | word | |
//! |------|-|
//! | 0 | magic `GXAU` |
//! | 1 | layout version, 2 |
//! | 2 | slots ([`SLOTS`]) |
//! | 3 | each slot's capacity in frames ([`FRAMES`]) |
//! | 4 | the player's heartbeat, bumped each render quantum (the worklet's) |
//! | 5 | the context's sample rate (the page's; 0 before it starts) |
//! | 6 | the browser's latency, base + output, in microseconds (the page's) |
//! | 7 | the capture ring's write position, in frames at the context's rate (the worklet's) |
//! | 8 | the capture ring's capacity in frames ([`CAP_FRAMES`]) |
//! | 9 | render quantum in frames (the worklet's: 128) |
//! | 10..32 | reserved |
//!
//! Then [`SLOTS`] slots of 16 words: stream (0: free), rate, generation
//! (bumped when the stream or its rate changes), frames written (wraps),
//! frames read (the worklet's), when last written (ms of the device's
//! clock), underruns (the worklet's), and reserved words. Then each slot's
//! `FRAMES` frames of two `f32`s, then the capture ring's `CAP_FRAMES`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// Streams that can play at once.
pub const SLOTS: usize = 8;
/// Frames in each slot's ring: 0.37 s at 44.1 kHz, more than the OSS
/// device's largest buffer (32 KiB: 8192 frames of 16-bit stereo, 16384 of
/// 8-bit mono), so a writer filling the device never laps a reader keeping
/// up.
pub const FRAMES: usize = 16384;
/// Frames of capture (at the context's rate): 0.34 s at 48 kHz.
pub const CAP_FRAMES: usize = 16384;
const HEADER_WORDS: usize = 32;
const SLOT_WORDS: usize = 16;
/// How long a stream may be quiet before another may take its slot.
const IDLE_MS: u32 = 250;
const MAGIC: u32 = u32::from_le_bytes(*b"GXAU");
/// How long the player may not move before the device goes back to the
/// wall clock (a quantum is 2.7-2.9 ms; a busy page's audio thread is late
/// by tens of ms at worst).
const STALL_MS: u32 = 200;

// Slot words.
const S_STREAM: usize = 0;
const S_RATE: usize = 1;
const S_GEN: usize = 2;
const S_W: usize = 3;
const S_R: usize = 4;
const S_LAST: usize = 5;

#[repr(C, align(16))]
struct Ring {
    words: [AtomicU32; HEADER_WORDS + SLOTS * SLOT_WORDS],
    frames: [[[f32; 2]; FRAMES]; SLOTS],
    capture: [[f32; 2]; CAP_FRAMES],
}

static mut RING: Ring = Ring {
    words: [const { AtomicU32::new(0) }; HEADER_WORDS + SLOTS * SLOT_WORDS],
    frames: [[[0.0; 2]; FRAMES]; SLOTS],
    capture: [[0.0; 2]; CAP_FRAMES],
};

/// Held while a slot is chosen and written (the frames are plain memory).
static WRITE: Mutex<()> = Mutex::new(());

/// The player's heartbeat as last seen, and when it was seen to change (ms
/// of the callers' clock).
static BEAT: Mutex<(u32, u32)> = Mutex::new((0, 0));

fn ring() -> &'static Ring {
    // SAFETY: the frames are written only under `WRITE`'s lock and read by
    // JavaScript as plain memory; the words are atomics.
    unsafe { &*std::ptr::addr_of!(RING) }
}

fn word(i: usize) -> &'static AtomicU32 {
    &ring().words[i]
}

fn slot(s: usize, w: usize) -> &'static AtomicU32 {
    &ring().words[HEADER_WORDS + s * SLOT_WORDS + w]
}

/// The ring's address in this module's memory, for the page to find it.
#[no_mangle]
pub extern "C" fn k_audio_ring() -> u32 {
    if word(0).load(Ordering::Relaxed) != MAGIC {
        word(1).store(2, Ordering::Relaxed);
        word(2).store(SLOTS as u32, Ordering::Relaxed);
        word(3).store(FRAMES as u32, Ordering::Relaxed);
        word(8).store(CAP_FRAMES as u32, Ordering::Relaxed);
        word(0).store(MAGIC, Ordering::Release);
    }
    ring() as *const Ring as usize as u32
}

/// The slot `stream` plays in, if it has one.
fn slot_of(stream: u32) -> Option<usize> {
    (0..SLOTS).find(|&s| slot(s, S_STREAM).load(Ordering::Acquire) == stream)
}

/// Whether the player has moved within [`STALL_MS`] of `now_ms`.
fn playing(now_ms: u32) -> bool {
    let beat = word(4).load(Ordering::Acquire);
    let mut b = BEAT.lock().unwrap_or_else(|e| e.into_inner());
    if beat != b.0 {
        *b = (beat, now_ms);
        true
    } else {
        beat != 0 && now_ms.wrapping_sub(b.1) <= STALL_MS
    }
}

/// Put what `/dev/dsp` stream `stream` accepted (`len` bytes at `at` in the
/// caller's memory) in its slot, taking one if it has none. `now_ms` is the
/// caller's clock. Returns the frames put, 0 when every slot is busy with
/// other streams, or -errno.
#[no_mangle]
pub extern "C" fn k_audio_write(stream: u32, at: gex_kmem::PAddr, len: u32, format: u32, rate: u32, channels: u32, now_ms: u32) -> i32 {
    k_audio_ring();
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let s = match slot_of(stream) {
        Some(s) => s,
        None => {
            // A free slot, else the one quiet longest (past IDLE_MS).
            let free = (0..SLOTS).find(|&s| slot(s, S_STREAM).load(Ordering::Relaxed) == 0);
            let idle = || {
                (0..SLOTS)
                    .map(|s| (s, now_ms.wrapping_sub(slot(s, S_LAST).load(Ordering::Relaxed))))
                    .filter(|&(_, quiet)| quiet >= IDLE_MS && quiet < u32::MAX / 2)
                    .max_by_key(|&(_, quiet)| quiet)
                    .map(|(s, _)| s)
            };
            let Some(s) = free.or_else(idle) else { return 0 };
            slot(s, S_STREAM).store(stream, Ordering::Relaxed);
            slot(s, S_GEN).fetch_add(1, Ordering::Release);
            s
        }
    };
    if slot(s, S_RATE).load(Ordering::Relaxed) != rate {
        slot(s, S_RATE).store(rate, Ordering::Relaxed);
        slot(s, S_GEN).fetch_add(1, Ordering::Release);
    }
    slot(s, S_LAST).store(now_ms, Ordering::Relaxed);
    let mut pcm = vec![0u8; len as usize];
    // SAFETY: `pcm` is `len` bytes of this module's memory.
    if !unsafe { gex_kmem::pull_checked(pcm.as_mut_ptr(), at, len) } {
        return -14; // EFAULT
    }
    let mut frames = Vec::with_capacity(pcm.len() / 2);
    let n = gex_kernel::vfs::oss::to_stereo_f32(&pcm, format, channels, &mut frames);
    // Only the newest ring's worth matters: older frames would be overwritten.
    let skip = frames.len().saturating_sub(FRAMES);
    let mut w = slot(s, S_W).load(Ordering::Relaxed).wrapping_add(skip as u32);
    // SAFETY: under `WRITE`; the worklet only reads.
    let ring_frames = unsafe { &mut (*std::ptr::addr_of_mut!(RING.frames))[s] };
    for f in &frames[skip..] {
        ring_frames[w as usize % FRAMES] = *f;
        w = w.wrapping_add(1);
    }
    slot(s, S_W).store(w, Ordering::Release);
    n as i32
}

/// A stream closed: its slot is free at once (not after [`IDLE_MS`]).
#[no_mangle]
pub extern "C" fn k_audio_close(stream: u32) {
    k_audio_ring();
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = slot_of(stream) {
        // What it wrote still plays out: the worklet keeps reading a slot
        // until it has caught up, whoever owns it. A new owner bumps the
        // generation, which drops the rest.
        slot(s, S_STREAM).store(0, Ordering::Release);
    }
}

/// The frames of `/dev/dsp` stream `stream` put in its slot and not played
/// yet, when the player is playing; -1 when it is not (the stream has no
/// slot, or the player has not moved for [`STALL_MS`]). `now_ms` is the
/// caller's clock.
#[no_mangle]
pub extern "C" fn k_audio_pending(stream: u32, now_ms: u32) -> i32 {
    k_audio_ring();
    let Some(s) = slot_of(stream) else { return -1 };
    if !playing(now_ms) {
        return -1;
    }
    let w = slot(s, S_W).load(Ordering::Acquire);
    let rd = slot(s, S_R).load(Ordering::Acquire);
    // A reader ahead of the writer (a new generation's start) has nothing
    // pending; one more than a ring behind has been lapped.
    let pending = w.wrapping_sub(rd);
    if pending > FRAMES as u32 {
        return if (pending as i32) < 0 { 0 } else { FRAMES as i32 };
    }
    pending as i32
}

/// What the device knows of the player, for `SNDCTL_DSP_GEX_INFO`: five
/// words at `out` in the caller's memory -- 1 when the player is playing
/// (else 0), the context's rate, the browser's latency in microseconds,
/// the render quantum in frames, and the frames of `stream` the player has
/// read (its slot's position, which wraps; 0 with no slot). Returns 0 or
/// -errno.
#[no_mangle]
pub extern "C" fn k_audio_info(stream: u32, now_ms: u32, out: gex_kmem::PAddr) -> i32 {
    k_audio_ring();
    let read = slot_of(stream).map(|s| slot(s, S_R).load(Ordering::Acquire)).unwrap_or(0);
    let v = [
        playing(now_ms) as u32,
        word(5).load(Ordering::Acquire),
        word(6).load(Ordering::Acquire),
        word(9).load(Ordering::Acquire),
        read,
    ];
    let mut b = [0u8; 20];
    for (i, x) in v.iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&x.to_le_bytes());
    }
    // SAFETY: 20 bytes of this module's memory, to `out`, checked.
    if unsafe { gex_kmem::push_checked(out, b.as_ptr(), 20) } { 0 } else { -14 }
}

/// The capture ring's write position now (frames at the context's rate),
/// where a reader that has read nothing yet starts.
#[no_mangle]
pub extern "C" fn k_audio_capture_pos() -> u32 {
    k_audio_ring();
    word(7).load(Ordering::Acquire)
}

/// Read capture from frame `pos` (at the context's rate): up to `len` bytes
/// of `format`/`channels` at `rate`, resampled from the context's rate
/// (nearest frame), to `at` in the caller's memory. Returns the bytes in
/// the high half (0 when nothing new was captured, or -errno) and the new
/// position in the low half. A reader a whole ring behind skips ahead to
/// half a ring before the newest frame.
#[no_mangle]
pub extern "C" fn k_audio_capture(pos: u32, at: gex_kmem::PAddr, len: u32, format: u32, rate: u32, channels: u32) -> i64 {
    k_audio_ring();
    let ret = |n: i32, pos: u32| ((n as i64) << 32) | pos as i64;
    let ctx = word(5).load(Ordering::Acquire);
    if ctx == 0 || rate == 0 {
        return ret(0, pos);
    }
    let mut pos = pos;
    let w = word(7).load(Ordering::Acquire);
    let mut avail = w.wrapping_sub(pos);
    if avail > CAP_FRAMES as u32 {
        // Lapped, or a reader ahead of a ring that started again.
        pos = w.wrapping_sub((CAP_FRAMES / 2) as u32);
        avail = (CAP_FRAMES / 2) as u32;
    }
    let ss = gex_kernel::vfs::oss::sample_bytes(format);
    let fb = ss * channels.max(1) as usize;
    let want = len as usize / fb;
    // Output frames the available capture makes at the stream's rate.
    let can = (avail as u64 * rate as u64 / ctx as u64) as usize;
    let n = want.min(can);
    if n == 0 {
        return ret(0, pos);
    }
    // SAFETY: the worklet writes the capture ring; a frame read mid-write
    // is one stale frame, not a fault.
    let cap = unsafe { &*std::ptr::addr_of!(RING.capture) };
    let mut out = Vec::with_capacity(n * fb);
    for i in 0..n {
        let src = pos.wrapping_add((i as u64 * ctx as u64 / rate as u64) as u32);
        let f = cap[src as usize % CAP_FRAMES];
        for c in 0..channels.max(1) as usize {
            gex_kernel::vfs::oss::put_sample(format, if c == 0 { f[0] } else if c == 1 { f[1] } else { 0.0 }, &mut out);
        }
    }
    pos = pos.wrapping_add((n as u64 * ctx as u64 / rate as u64) as u32);
    // SAFETY: to the caller's memory, checked.
    if !unsafe { gex_kmem::push_checked(at, out.as_ptr(), out.len() as u32) } {
        return ret(-14, pos);
    }
    ret(out.len() as i32, pos)
}
