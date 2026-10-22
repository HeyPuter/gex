//! Who makes a process's commonest syscalls, by module+offset (a debug
//! switch: with `?heartbeat=N` on). A sampled call's guest stack, from the
//! context's stack pointer up, is scanned for the first word inside an
//! executable file mapping that is not Wine's own (`/w64/`, `/w3/`, `wine`),
//! else the first inside any: the return address into whatever called the
//! API that ended in this call. Printed with the heartbeat's census as the
//! file's name and the offset from its lowest mapping (an image's RVA).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

const SLOTS: usize = 4096;
/// `nr << 48 | address` (48-bit addresses), and its count.
static KEYS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static HITS: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];
static SEEN: AtomicU64 = AtomicU64::new(0);
/// One call in this many is scanned.
const EVERY: u64 = 16;
/// How many stack words a scan reads.
const WORDS: u64 = 96;

struct Maps {
    at: u64,
    maps: Vec<gex_kernel::CodeMap>,
}
static MAPS: Mutex<Maps> = Mutex::new(Maps { at: 0, maps: Vec::new() });

fn own(path: &str) -> bool {
    path.contains("/w64/") || path.contains("/w3/") || path.contains("wine")
}

fn find(maps: &[gex_kernel::CodeMap], w: u64) -> Option<&gex_kernel::CodeMap> {
    maps.iter().find(|m| m.start <= w && w < m.end)
}

pub(crate) fn note(nr: u64, ctx: u64, sp_index: u8) {
    let n = SEEN.fetch_add(1, Relaxed);
    if n % EVERY != 0 {
        return;
    }
    let gm = gex_kernel::GuestMem::new(crate::guest_bytes());
    let Ok(sp) = gm.read_u64(ctx + gex_abi::ctx::x(sp_index) as u64) else { return };
    let mut g = MAPS.lock().unwrap_or_else(|e| e.into_inner());
    // The maps again every 4096 samples (a new image is caught soon enough).
    if g.maps.is_empty() || n / EVERY >= g.at + 4096 {
        g.maps = crate::sigs::proc_home().code_maps();
        g.at = n / EVERY;
    }
    let mut any = None;
    let mut found = None;
    for i in 0..WORDS {
        let Ok(w) = gm.read_u64(sp + 8 * i) else { break };
        if let Some(m) = find(&g.maps, w) {
            any.get_or_insert(w);
            if !own(&m.path) {
                found = Some(w);
                break;
            }
        }
    }
    drop(g);
    let w = found.or(any).unwrap_or(0) & 0xffff_ffff_ffff;
    let key = (nr << 48) | w;
    let mut s = (key.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 52) as usize % SLOTS;
    for _ in 0..SLOTS {
        match KEYS[s].compare_exchange(0, key, Relaxed, Relaxed) {
            Ok(_) => {
                HITS[s].fetch_add(1, Relaxed);
                return;
            }
            Err(k) if k == key => {
                HITS[s].fetch_add(1, Relaxed);
                return;
            }
            Err(_) => s = (s + 1) % SLOTS,
        }
    }
}

/// The top callers of the `nrs` calls, a line each.
pub(crate) fn report(nrs: &[usize]) {
    let maps = MAPS.lock().unwrap_or_else(|e| e.into_inner()).maps.clone();
    let name = |w: u64| -> String {
        if w == 0 {
            return "?".into();
        }
        match find(&maps, w) {
            Some(m) => {
                let base = maps.iter().filter(|x| x.path == m.path).map(|x| x.start).min().unwrap_or(m.start);
                format!("{}+{:#x}", m.path.rsplit('/').next().unwrap_or(&m.path), w - base)
            }
            None => format!("{w:#x}"),
        }
    };
    for &nr in nrs {
        let mut v: Vec<(u32, u64)> = (0..SLOTS)
            .filter_map(|s| {
                let k = KEYS[s].load(Relaxed);
                (k != 0 && (k >> 48) as usize == nr).then(|| (HITS[s].load(Relaxed), k & 0xffff_ffff_ffff))
            })
            .collect();
        if v.is_empty() {
            continue;
        }
        let total: u64 = v.iter().map(|x| x.0 as u64).sum();
        v.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        let mut line = format!(
            "[gex] callers of {} ({total} sampled, 1 in {EVERY}):",
            gex_kernel::syscalls::syscall_name(nr as u64)
        );
        for (c, w) in v.into_iter().take(6) {
            line.push_str(&format!(" {}={}%", name(w), 100 * c as u64 / total.max(1)));
        }
        line.push('\n');
        crate::trace_line(&line);
    }
}
