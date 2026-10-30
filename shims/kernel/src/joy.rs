//! The page's game controllers: a table the page writes from the Gamepad
//! API (web/runtime/src/gamepad.js) and every process's `/dev/input/eventN`
//! reads (gex-kernel `vfs/evdev.rs`, which has the pad words' layout).
//!
//! The layout is written by JavaScript, so it is fixed (little-endian `u32`
//! words):
//!
//! | word | |
//! |------|-|
//! | 0 | magic `GXJY` |
//! | 1 | layout version, 1 |
//! | 2 | pads ([`PADS`]) |
//! | 3 | words per pad ([`PAD_WORDS`]) |
//! | 4 | when a pad last connected or went, seconds since the epoch (the page's) |
//! | 5..16 | reserved |
//!
//! Then the pads. The page makes a pad's serial (its word 0) odd while it
//! writes the pad and even again after, then calls [`k_joy_notify`]; a
//! reader copies a pad only between two equal, even serials.

use gex_kernel::vfs::evdev::{PADS, PAD_WORDS};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

const HEADER_WORDS: usize = 16;
const MAGIC: u32 = u32::from_le_bytes(*b"GXJY");

static TABLE: [AtomicU32; HEADER_WORDS + PADS * PAD_WORDS] =
    [const { AtomicU32::new(0) }; HEADER_WORDS + PADS * PAD_WORDS];

fn word(i: usize) -> &'static AtomicU32 {
    &TABLE[i]
}

/// The queue a reader of any pad waits on.
fn queue() -> &'static gex_kernel::wait::Queue {
    static Q: OnceLock<gex_kernel::wait::Queue> = OnceLock::new();
    Q.get_or_init(gex_kernel::wait::Queue::new)
}

/// The table's address in this module's memory, for the page.
#[no_mangle]
pub extern "C" fn k_joy_table() -> u32 {
    if word(0).load(Ordering::Acquire) != MAGIC {
        word(1).store(1, Ordering::Relaxed);
        word(2).store(PADS as u32, Ordering::Relaxed);
        word(3).store(PAD_WORDS as u32, Ordering::Relaxed);
        word(0).store(MAGIC, Ordering::Release);
    }
    TABLE.as_ptr() as u32
}

/// The page changed a pad: wake its readers.
#[no_mangle]
pub extern "C" fn k_joy_notify() {
    queue().notify();
}

/// The queue's number.
#[no_mangle]
pub extern "C" fn k_joy_queue() -> u32 {
    queue().id()
}

/// When a pad last connected or went (seconds since the epoch).
#[no_mangle]
pub extern "C" fn k_joy_changed() -> u32 {
    word(4).load(Ordering::Acquire)
}

/// Copy pad `pad`'s words to `out` in the caller's memory, whole: 0, or
/// -errno.
#[no_mangle]
pub extern "C" fn k_joy_read(pad: u32, out: gex_kmem::PAddr) -> i32 {
    if pad as usize >= PADS {
        return -22; // EINVAL
    }
    k_joy_table();
    let base = HEADER_WORDS + pad as usize * PAD_WORDS;
    let mut w = [0u32; PAD_WORDS];
    let mut tries = 0;
    loop {
        let s1 = word(base).load(Ordering::Acquire);
        if s1 & 1 == 0 {
            for (i, v) in w.iter_mut().enumerate() {
                *v = word(base + i).load(Ordering::Relaxed);
            }
            if word(base).load(Ordering::Acquire) == s1 {
                break;
            }
        }
        tries += 1;
        if tries > 10_000 {
            // A page that stopped mid-write: what is there.
            break;
        }
        std::hint::spin_loop();
    }
    // SAFETY: to the caller's memory, checked.
    if unsafe { gex_kmem::push_checked(out, w.as_ptr().cast(), (PAD_WORDS * 4) as u32) } {
        0
    } else {
        -14
    }
}
