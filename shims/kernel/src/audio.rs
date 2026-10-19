//! `/dev/dsp`'s output in the page: one ring of stereo `f32` frames in the
//! global kernel's memory, which the page's AudioWorklet reads through the
//! kernel memory's `SharedArrayBuffer` (web/src/audio.js).
//!
//! The player is the clock while it plays: the worklet publishes how far it
//! has read, and the OSS device (gex-kernel `vfs/oss.rs`) counts what the
//! stream playing has written and the worklet not read yet as queued
//! ([`k_audio_pending`]), so the writer paces itself to the audio hardware
//! and the backlog stays what the writer keeps queued. A player that has not
//! moved for [`STALL_MS`] (no gesture yet, a background tab) lets the device
//! drain by the wall clock again; what it accepts is put in the ring all the
//! same, never blocking the writer, the oldest frames overwritten.
//!
//! One stream plays at a time: the `/dev/dsp` that wrote last, once the one
//! playing has written nothing for [`IDLE_MS`]. Two programs playing at once
//! are not mixed; the first keeps the device until it goes quiet.
//!
//! The layout is read by JavaScript, so it is fixed (all little-endian
//! `u32` words, then the frames):
//!
//! | word | |
//! |------|-|
//! | 0 | magic `GXAU` |
//! | 1 | frames written, ever (wraps) |
//! | 2 | the stream's sample rate |
//! | 3 | the ring's capacity in frames ([`FRAMES`]) |
//! | 4 | the stream playing (0: none yet) |
//! | 5 | a generation, bumped when the stream or its rate changes |
//! | 6 | the player's read position, in frames (as word 1; the worklet's) |
//! | 7 | the player's heartbeat, bumped each render quantum (the worklet's) |
//! | 8.. | `FRAMES` frames of two `f32`s |

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// Frames in the ring: about 0.37 s at 44.1 kHz, more than the OSS device
/// buffers (about 190 ms), so a writer filling the device never laps a
/// reader keeping up.
pub const FRAMES: usize = 16384;
/// How long the playing stream may be quiet before another takes over.
const IDLE_MS: u32 = 250;
const MAGIC: u32 = u32::from_le_bytes(*b"GXAU");
/// How long the player may not move before the device goes back to the
/// wall clock (a quantum is 2.7-2.9 ms; a busy page's audio thread is late
/// by tens of ms at worst).
const STALL_MS: u32 = 200;

#[repr(C, align(16))]
struct Ring {
    words: [AtomicU32; 8],
    frames: [[f32; 2]; FRAMES],
}

static mut RING: Ring = Ring { words: [const { AtomicU32::new(0) }; 8], frames: [[0.0; 2]; FRAMES] };

/// When the playing stream last wrote (ms of the caller's clock).
static LAST: Mutex<u32> = Mutex::new(0);

/// The player's heartbeat as last seen, and when it was seen to change (ms
/// of the callers' clock).
static BEAT: Mutex<(u32, u32)> = Mutex::new((0, 0));

fn ring() -> &'static Ring {
    // SAFETY: the frames are written only under `LAST`'s lock and read by
    // JavaScript as plain memory; the words are atomics.
    unsafe { &*std::ptr::addr_of!(RING) }
}

/// The ring's address in this module's memory, for the page to find it.
#[no_mangle]
pub extern "C" fn k_audio_ring() -> u32 {
    let r = ring();
    if r.words[0].load(Ordering::Relaxed) != MAGIC {
        r.words[3].store(FRAMES as u32, Ordering::Relaxed);
        r.words[0].store(MAGIC, Ordering::Release);
    }
    r as *const Ring as usize as u32
}

/// Put what `/dev/dsp` stream `stream` accepted (`len` bytes at `at` in the
/// caller's memory) in the ring. `now_ms` is the caller's clock. Returns
/// the frames put, 0 when another stream has the device, or -errno.
#[no_mangle]
pub extern "C" fn k_audio_write(stream: u32, at: gex_kmem::PAddr, len: u32, format: u32, rate: u32, channels: u32, now_ms: u32) -> i32 {
    k_audio_ring();
    let r = ring();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let owner = r.words[4].load(Ordering::Relaxed);
    if owner != stream {
        if owner != 0 && now_ms.wrapping_sub(*last) < IDLE_MS {
            return 0;
        }
        r.words[4].store(stream, Ordering::Relaxed);
        r.words[5].fetch_add(1, Ordering::Release);
    }
    if r.words[2].load(Ordering::Relaxed) != rate {
        r.words[2].store(rate, Ordering::Relaxed);
        r.words[5].fetch_add(1, Ordering::Release);
    }
    *last = now_ms;
    let mut pcm = vec![0u8; len as usize];
    // SAFETY: `pcm` is `len` bytes of this module's memory.
    if !unsafe { gex_kmem::pull_checked(pcm.as_mut_ptr(), at, len) } {
        return -14; // EFAULT
    }
    let mut frames = Vec::with_capacity(pcm.len() / 2);
    let n = gex_kernel::vfs::oss::to_stereo_f32(&pcm, format, channels, &mut frames);
    // Only the newest ring's worth matters: older frames would be overwritten.
    let skip = frames.len().saturating_sub(FRAMES);
    let mut w = r.words[1].load(Ordering::Relaxed).wrapping_add(skip as u32);
    let ring_frames = unsafe { &mut *std::ptr::addr_of_mut!(RING.frames) };
    for f in &frames[skip..] {
        ring_frames[w as usize % FRAMES] = *f;
        w = w.wrapping_add(1);
    }
    r.words[1].store(w, Ordering::Release);
    n as i32
}

/// The frames of `/dev/dsp` stream `stream` put in the ring and not played
/// yet, when the player is playing it; -1 when it is not (another stream
/// has the device, or the player has not moved for [`STALL_MS`]). `now_ms`
/// is the caller's clock.
#[no_mangle]
pub extern "C" fn k_audio_pending(stream: u32, now_ms: u32) -> i32 {
    k_audio_ring();
    let r = ring();
    if r.words[4].load(Ordering::Relaxed) != stream {
        return -1;
    }
    let beat = r.words[7].load(Ordering::Acquire);
    {
        let mut b = BEAT.lock().unwrap_or_else(|e| e.into_inner());
        if beat != b.0 {
            *b = (beat, now_ms);
        } else if beat == 0 || now_ms.wrapping_sub(b.1) > STALL_MS {
            return -1;
        }
    }
    let w = r.words[1].load(Ordering::Acquire);
    let rd = r.words[6].load(Ordering::Acquire);
    // A reader ahead of the writer (a new generation's start) has nothing
    // pending; one more than a ring behind has been lapped.
    let pending = w.wrapping_sub(rd);
    if pending > FRAMES as u32 {
        return if (pending as i32) < 0 { 0 } else { FRAMES as i32 };
    }
    pending as i32
}
