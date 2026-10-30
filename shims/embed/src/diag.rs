//! Saying where the guest was: guest addresses as `library+offset (symbol+delta)`, and a backtrace by the frame
//! pointer chain, for what ends the process (a raised signal, an illegal instruction, a jump into nothing).

use std::collections::HashMap;
use std::sync::Mutex;

struct Image {
    lo: u64,
    hi: u64,
    path: String,
    bias: u64,
}

static IMAGES: Mutex<Vec<Image>> = Mutex::new(Vec::new());
/// Each file's functions, by value, read the first time an address in it is named.
static SYMBOLS: Mutex<Option<HashMap<String, Vec<(u64, String)>>>> = Mutex::new(None);

/// Code of `path` (load bias `bias`) is at `[lo, hi)`.
pub fn mapped(lo: u64, hi: u64, path: &str, bias: u64) {
    let mut v = IMAGES.lock().unwrap();
    v.retain(|i| i.hi <= lo || i.lo >= hi);
    v.push(Image { lo, hi, path: path.to_string(), bias });
}

fn symbols_of(path: &str) -> Vec<(u64, String)> {
    let Ok(bytes) = crate::elf::read_file(path) else { return Vec::new() };
    let Ok(e) = crate::elf::Elf::parse(&bytes) else { return Vec::new() };
    let mut v: Vec<(u64, String)> =
        e.symbols().into_iter().filter(|(_, v)| *v != 0).map(|(n, v)| (v, String::from_utf8_lossy(n).into_owned())).collect();
    v.sort();
    v
}

/// `a` as `library+offset (symbol+delta)`, or just the address when it is in nothing mapped.
pub fn name(a: u64) -> String {
    let (path, bias) = {
        let v = IMAGES.lock().unwrap();
        match v.iter().find(|i| i.lo <= a && a < i.hi) {
            Some(i) => (i.path.clone(), i.bias),
            None => return format!("{a:#x}"),
        }
    };
    let off = a - bias;
    let mut g = SYMBOLS.lock().unwrap();
    let syms = g.get_or_insert_with(HashMap::new).entry(path.clone()).or_insert_with(|| symbols_of(&path));
    let i = syms.partition_point(|(v, _)| *v <= off);
    match i.checked_sub(1).map(|i| &syms[i]) {
        Some((v, n)) => format!("{a:#x} {path}+{off:#x} ({n}+{:#x})", off - v),
        None => format!("{a:#x} {path}+{off:#x}"),
    }
}

/// The guest's backtrace on this thread: the pc, x30, and the frame records x29 chains through.
pub fn backtrace(ctx: u32) -> String {
    let t = crate::thread::current();
    backtrace_in(ctx, t.stack_base, t.stack_base + crate::thread::STACK_SIZE.load(std::sync::atomic::Ordering::Relaxed) as u64)
}

/// The backtrace of the thread whose context is `ctx` and whose guest stack is `[lo, hi)`: as it last left its
/// registers there (at a system call or a call to the host, for a thread inside one).
pub fn backtrace_in(ctx: u32, lo: u64, hi: u64) -> String {
    use gex_abi::ctx as c;
    let pc = crate::cpu::get_u64(ctx, c::PC);
    let mut out = format!("  pc  {}\n  x30 {}\n", name(pc), name(crate::cpu::x(ctx, 30)));
    let mut fp = crate::cpu::x(ctx, 29);
    for _ in 0..32 {
        if fp < lo || fp + 16 > hi || fp % 8 != 0 {
            break;
        }
        // SAFETY: a frame record {next fp, return address} inside this thread's guest stack.
        let (next, lr) = unsafe { (*(fp as usize as *const u64), *((fp + 8) as usize as *const u64)) };
        if lr == 0 {
            break;
        }
        out += &format!("  ... {}\n", name(lr));
        if next <= fp {
            break;
        }
        fp = next;
    }
    out
}
