//! The guest's memory map: mappings carved from blocks of the host's heap.
//!
//! A mapping the guest asks for without an address is a block of its own (`memalign`, zeroed); `MAP_FIXED` may only
//! land inside a block that exists (the linker reserves a library's span, then maps its segments into it). A block
//! goes back to the host's heap once nothing in it is mapped. There is no protection: `PROT_NONE` and read-only
//! pages are as writable as any other. What protections do matter for is execution: a range turning executable is
//! handed to the translator, and one that stops being executable has its translations retired.

use std::collections::BTreeMap;
use std::sync::Mutex;

pub const PROT_EXEC: u32 = 4;
pub const PAGE: u64 = 4096;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;
const EINVAL: i64 = 22;
const ENOMEM: i64 = 12;

extern "C" {
    fn memalign(align: usize, size: usize) -> *mut u8;
    fn free(p: *mut u8);
    fn gex_embed_pread(fd: i32, buf: u64, n: u64, off: u64) -> i64;
}

#[derive(Clone, Copy)]
struct Vma {
    hi: u64,
    prot: u32,
}

struct Map {
    /// Mappings by start.
    vmas: BTreeMap<u64, Vma>,
    /// The host's blocks the mappings are in, by start: their lengths.
    blocks: BTreeMap<u64, u64>,
}

static MAP: Mutex<Map> = Mutex::new(Map { vmas: BTreeMap::new(), blocks: BTreeMap::new() });

fn round_up(n: u64) -> u64 {
    (n + PAGE - 1) & !(PAGE - 1)
}

fn zero(lo: u64, len: u64) {
    // SAFETY: a range inside a block of ours.
    unsafe { (lo as usize as *mut u8).write_bytes(0, len as usize) }
}

impl Map {
    fn block_of(&self, lo: u64, hi: u64) -> Option<u64> {
        let (&b, &len) = self.blocks.range(..=lo).next_back()?;
        (hi <= b + len).then_some(b)
    }

    /// Split the mapping that `a` falls strictly inside, so a mapping starts at `a`.
    fn split(&mut self, a: u64) {
        if let Some((&lo, &v)) = self.vmas.range(..a).next_back() {
            if v.hi > a {
                self.vmas.insert(lo, Vma { hi: a, ..v });
                self.vmas.insert(a, v);
            }
        }
    }

    /// Mappings in `[lo, hi)`, split at the ends: their starts.
    fn carve(&mut self, lo: u64, hi: u64) -> Vec<u64> {
        self.split(lo);
        self.split(hi);
        self.vmas.range(lo..hi).map(|(&a, _)| a).collect()
    }

    /// Whether every byte of `[lo, hi)` is mapped.
    fn covered(&self, lo: u64, hi: u64) -> bool {
        let mut at = lo;
        for (&a, v) in self.vmas.range(..hi) {
            if v.hi <= at {
                continue;
            }
            if a > at {
                return false;
            }
            at = v.hi;
            if at >= hi {
                return true;
            }
        }
        at >= hi
    }
}

/// What changed about execution, done after the map's lock is let go (the translator takes its own).
#[derive(Default)]
struct Exec {
    gained: Vec<(u64, u64)>,
    lost: Vec<(u64, u64)>,
}

impl Exec {
    fn apply(self) {
        for (lo, hi) in self.lost {
            crate::jit::unmap_exec(lo, hi);
        }
        for (lo, hi) in self.gained {
            crate::jit::map_exec(lo, hi);
        }
    }
}

pub fn mmap(addr: u64, len: u64, prot: u32, flags: u64, fd: i32, off: u64) -> i64 {
    if len == 0 || off % PAGE != 0 {
        return -EINVAL;
    }
    let len = round_up(len);
    let fixed = flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0;
    let mut exec = Exec::default();
    let lo = {
        let mut m = MAP.lock().unwrap();
        if fixed {
            if addr % PAGE != 0 {
                return -EINVAL;
            }
            if m.block_of(addr, addr + len).is_none() {
                crate::host::log(&format!("gex-embed: MAP_FIXED at {addr:#x}+{len:#x}, outside anything mapped\n"));
                return -ENOMEM;
            }
            if flags & MAP_FIXED_NOREPLACE != 0 && m.vmas.range(..addr + len).any(|(_, v)| v.hi > addr) {
                return -17; // EEXIST
            }
            for a in m.carve(addr, addr + len) {
                let v = m.vmas.remove(&a).unwrap();
                if v.prot & PROT_EXEC != 0 {
                    exec.lost.push((a, v.hi));
                }
            }
            zero(addr, len);
            addr
        } else {
            // SAFETY: a plain allocation, zeroed before anything sees it.
            let p = unsafe { memalign(PAGE as usize, len as usize) } as usize as u64;
            if p == 0 {
                return -ENOMEM;
            }
            zero(p, len);
            m.blocks.insert(p, len);
            p
        }
    };
    if flags & MAP_ANONYMOUS == 0 && fd >= 0 {
        // SAFETY: the C side's, into the range just mapped.
        let r = unsafe { gex_embed_pread(fd, lo, len, off) };
        if r < 0 {
            if !fixed {
                let mut m = MAP.lock().unwrap();
                m.blocks.remove(&lo);
                unsafe { free(lo as usize as *mut u8) };
            }
            return r;
        }
    }
    MAP.lock().unwrap().vmas.insert(lo, Vma { hi: lo + len, prot });
    if prot & PROT_EXEC != 0 {
        exec.gained.push((lo, lo + len));
    }
    exec.apply();
    if prot & PROT_EXEC != 0 && fd >= 0 {
        crate::overlay::exec_mapped(fd, lo, off);
    }
    lo as i64
}

pub fn munmap(addr: u64, len: u64) -> i64 {
    if addr % PAGE != 0 || len == 0 {
        return -EINVAL;
    }
    let hi = addr + round_up(len);
    let mut exec = Exec::default();
    let mut freed = Vec::new();
    {
        let mut m = MAP.lock().unwrap();
        for a in m.carve(addr, hi) {
            let v = m.vmas.remove(&a).unwrap();
            if v.prot & PROT_EXEC != 0 {
                exec.lost.push((a, v.hi));
            }
        }
        // Blocks with nothing left in them go back to the host.
        let touched: Vec<(u64, u64)> = m.blocks.range(..hi).filter(|(&b, &l)| b + l > addr).map(|(&b, &l)| (b, l)).collect();
        for (b, l) in touched {
            if !m.vmas.range(..b + l).any(|(_, v)| v.hi > b) {
                m.blocks.remove(&b);
                freed.push(b);
            }
        }
    }
    exec.apply();
    for b in freed {
        // SAFETY: a block of ours, nothing mapped in it.
        unsafe { free(b as usize as *mut u8) };
    }
    0
}

pub fn mprotect(addr: u64, len: u64, prot: u32) -> i64 {
    if addr % PAGE != 0 {
        return -EINVAL;
    }
    let hi = addr + round_up(len);
    let mut exec = Exec::default();
    {
        let mut m = MAP.lock().unwrap();
        if !m.covered(addr, hi) {
            return -ENOMEM;
        }
        for a in m.carve(addr, hi) {
            let v = m.vmas.get_mut(&a).unwrap();
            let was = v.prot & PROT_EXEC != 0;
            let now = prot & PROT_EXEC != 0;
            v.prot = prot;
            if was && !now {
                exec.lost.push((a, v.hi));
            } else if now && !was {
                exec.gained.push((a, v.hi));
            }
        }
    }
    exec.apply();
    0
}

pub fn madvise(addr: u64, len: u64, advice: u64) -> i64 {
    // MADV_DONTNEED: private anonymous pages read back as zero.
    if advice == 4 {
        let hi = addr + round_up(len);
        if MAP.lock().unwrap().covered(addr, hi) {
            zero(addr, hi - addr);
        }
    }
    0
}

/// A block for an image of `len` bytes at `align`, all of it mapped with no access: `elf::map` protects its
/// segments after copying them in.
pub fn reserve(len: u64, align: u64) -> Option<u64> {
    let len = round_up(len);
    // SAFETY: a plain allocation, zeroed before anything sees it.
    let p = unsafe { memalign(align.max(PAGE) as usize, len as usize) } as usize as u64;
    if p == 0 {
        return None;
    }
    zero(p, len);
    let mut m = MAP.lock().unwrap();
    m.blocks.insert(p, len);
    m.vmas.insert(p, Vma { hi: p + len, prot: 0 });
    Some(p)
}
