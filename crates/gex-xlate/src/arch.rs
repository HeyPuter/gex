//! The guest architecture behind the translator: the decoder and lifter pair
//! the `x86` cargo feature selects. Everything else in this crate is written
//! against these few functions, so it never names an instruction set.

use gex_ir::Block;

/// Buffer a fetch fills: the longest x86 instruction is 15 bytes, and the A64
/// adapter reads one word from it.
pub type FetchBuf = [u8; 16];

#[cfg(not(feature = "x86"))]
mod imp {
    use super::*;
    pub use gex_lift::BlockLimits;

    /// Lift one block. `fetch(pc, buf)` fills `buf` with the executable bytes
    /// at `pc` and returns how many it could provide.
    pub fn lift_block(
        fetch: impl Fn(u64, &mut FetchBuf) -> usize,
        start: u64,
        limits: BlockLimits,
    ) -> Block {
        gex_lift::lift_block(
            |pc| {
                if pc % 4 != 0 {
                    return None;
                }
                let mut buf = [0u8; 16];
                if fetch(pc, &mut buf) >= 4 {
                    Some(u32::from_le_bytes(buf[..4].try_into().unwrap()))
                } else {
                    None
                }
            },
            start,
            limits,
        )
    }

    /// Whether the bytes at `pc` begin an instruction gex can name, as
    /// opposed to padding, data or an encoding it does not know.
    pub fn decodable(bytes: &[u8], pc: u64) -> bool {
        if bytes.len() < 4 || pc % 4 != 0 {
            return false;
        }
        let raw = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        !matches!(
            gex_isa::decode(raw, pc),
            gex_isa::Inst::Unknown { .. } | gex_isa::Inst::Undefined { .. }
        )
    }

    /// Scan code for instructions that materialise a code address into a
    /// register, calling `seed` with each executable target. A function whose
    /// address is *taken* rather than called -- `adrp`+`add` -- is reached by
    /// no branch and lies in no data word, so neither successor-following nor
    /// the data scan finds it. Over-seeding is safe: a target that is really
    /// data is rejected by `is_exec`.
    pub fn scan_code_pointers(
        bytes: &[u8],
        vaddr: u64,
        is_exec: &dyn Fn(u64) -> bool,
        seed: &mut dyn FnMut(u64),
    ) {
        let mut page: [Option<u64>; 32] = [None; 32];
        let end = vaddr + bytes.len() as u64;
        let mut a = vaddr;
        while a + 4 <= end {
            if !is_exec(a) {
                page = [None; 32];
                a += 4;
                continue;
            }
            let off = (a - vaddr) as usize;
            let raw = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
            match gex_isa::decode(raw, a) {
                gex_isa::Inst::Adrp {
                    rd: gex_isa::RegSel::Gp(r),
                    target,
                } => {
                    page[r as usize] = Some(target);
                }
                gex_isa::Inst::Adr { target, .. } => {
                    if target % 4 == 0 && is_exec(target) {
                        seed(target);
                    }
                }
                gex_isa::Inst::AddSubImm {
                    op: gex_isa::AddOp::Add,
                    rd: gex_isa::RegSel::Gp(rd),
                    rn: gex_isa::RegSel::Gp(rn),
                    imm,
                    ..
                } => {
                    if let Some(p) = page[rn as usize] {
                        let target = p.wrapping_add(imm);
                        if target % 4 == 0 && is_exec(target) {
                            seed(target);
                        }
                    }
                    page[rd as usize] = None;
                }
                _ => {}
            }
            a += 4;
        }
    }

    /// Every candidate instruction start in `[lo, hi)`: for A64, every aligned
    /// word that decodes. `anchors` (known instruction starts) are not needed
    /// on a fixed-width ISA.
    pub fn sweep(
        fetch: &dyn Fn(u64, &mut FetchBuf) -> usize,
        lo: u64,
        hi: u64,
        _anchors: &[u64],
        emit: &mut dyn FnMut(u64),
    ) {
        let mut a = lo & !3;
        while a < hi {
            let mut buf = [0u8; 16];
            let n = fetch(a, &mut buf);
            if n > 0 && decodable(&buf[..n], a) {
                emit(a);
            }
            a += 4;
        }
    }

    /// Disassemble one instruction: its text and length.
    pub fn disasm(bytes: &[u8], pc: u64) -> (String, usize) {
        if bytes.len() < 4 {
            return ("(truncated)".into(), bytes.len().max(1));
        }
        let raw = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        let inst = gex_isa::decode(raw, pc);
        (
            format!("{raw:08x}\t{}", gex_isa::fmt::disasm(&inst, pc)),
            4,
        )
    }
}

#[cfg(feature = "x86")]
mod imp {
    use super::*;
    pub use gex_lift_x86::BlockLimits;
    use gex_isa_x86::Mode;

    /// Address ranges whose code runs in a mode other than the process's:
    /// a WoW64 process is 64-bit with 32-bit DLLs mapped low, and the code
    /// segment a far transfer lands in says which is which. Recorded by the
    /// host when `cs` is loaded (`sysreg::SEG_CS`); consulted per block.
    static MODE_RANGES: std::sync::RwLock<Vec<(u64, u64, Mode)>> = std::sync::RwLock::new(Vec::new());

    /// Record that `[lo, hi)` holds `compat` (32-bit) or long-mode code.
    pub fn set_mode_range(lo: u64, hi: u64, compat: bool) {
        let mode = if compat { Mode::Compat } else { Mode::Long };
        let mut v = MODE_RANGES.write().unwrap();
        v.retain(|(a, b, _)| *b <= lo || *a >= hi);
        v.push((lo, hi, mode));
    }

    thread_local! {
        /// The mode of the thread whose dispatch miss is being served: the
        /// code segment it holds decides how the bytes it is about to run
        /// are decoded. A WoW64 process has 32-bit and 64-bit code at
        /// addresses nothing distinguishes (everything is below 4 GiB
        /// here), so the executing thread's `cs` is the only witness.
        static THREAD_MODE: std::cell::Cell<Option<Mode>> = const { std::cell::Cell::new(None) };
    }

    /// Say which mode the thread now being served runs in (`None`: the
    /// process default, or a recorded range).
    pub fn set_thread_mode(compat: Option<bool>) {
        THREAD_MODE.with(|m| {
            m.set(compat.map(|c| if c { Mode::Compat } else { Mode::Long }))
        });
    }

    /// The serving thread's mode, so a discovery running on the dispatch
    /// thread can carry it to the worker threads it lifts on.
    pub fn thread_mode() -> Option<Mode> {
        THREAD_MODE.with(|m| m.get())
    }

    /// Set the raw mode a lift worker runs in. A discovery pass is one mode
    /// throughout -- a near branch never changes it, and a far one ends the
    /// block -- so every block a pass lifts takes the dispatch thread's mode,
    /// which the parallel workers do not otherwise inherit (it is thread
    /// local). Without this a WoW64 process's 32-bit blocks lifted on a worker
    /// default to long mode and run at the wrong stack width.
    pub fn set_worker_mode(mode: Option<Mode>) {
        THREAD_MODE.with(|m| m.set(mode));
    }

    /// The mode code at `pc` is decoded in: the serving thread's, else a
    /// recorded range, else the process's own.
    pub fn mode_at(pc: u64) -> Mode {
        if let Some(m) = THREAD_MODE.with(|m| m.get()) {
            return m;
        }
        let v = MODE_RANGES.read().unwrap();
        for (lo, hi, m) in v.iter() {
            if pc >= *lo && pc < *hi {
                return *m;
            }
        }
        gex_isa_x86::mode()
    }

    /// The mode each page's blocks were last lifted in, so a code-segment
    /// switch into a page can tell whether its translations still apply.
    static PAGE_MODES: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<u64, Mode>>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

    /// Whether `[target]`'s page holds blocks lifted in the *other* mode from
    /// the one a transfer with `compat` lands in: those translations are
    /// stale and must be retired before the page is entered.
    pub fn page_mode_conflicts(target: u64, compat: bool) -> bool {
        let want = if compat { Mode::Compat } else { Mode::Long };
        let page = target >> 12;
        PAGE_MODES.lock().unwrap().get(&page).is_some_and(|m| *m != want)
    }

    pub fn lift_block(
        fetch: impl Fn(u64, &mut FetchBuf) -> usize,
        start: u64,
        limits: BlockLimits,
    ) -> Block {
        let mode = mode_at(start);
        PAGE_MODES.lock().unwrap().insert(start >> 12, mode);
        gex_lift_x86::lift_block(fetch, start, limits, mode)
    }

    pub fn decodable(bytes: &[u8], pc: u64) -> bool {
        !bytes.is_empty() && gex_isa_x86::decode_in(mode_at(pc), bytes, pc).is_known()
    }

    /// A linear sweep of the code: every `lea reg, [rip+disp]` and every
    /// `mov reg, imm` whose value lands in executable memory is an
    /// address-taken function. The sweep starts at the range's beginning and
    /// follows instruction lengths; compiler output keeps it in step, and a
    /// stretch out of step only costs spurious seeds `is_exec` rejects.
    pub fn scan_code_pointers(
        bytes: &[u8],
        vaddr: u64,
        is_exec: &dyn Fn(u64) -> bool,
        seed: &mut dyn FnMut(u64),
    ) {
        use gex_isa_x86::{Op, Operand};
        let end = vaddr + bytes.len() as u64;
        let mut a = vaddr;
        while a < end {
            if !is_exec(a) {
                a += 1;
                continue;
            }
            let off = (a - vaddr) as usize;
            let avail = (bytes.len() - off).min(16);
            let inst = gex_isa_x86::decode_in(mode_at(a), &bytes[off..off + avail], a);
            match (inst.op, inst.ops[1]) {
                (Op::Lea, Some(Operand::Mem(m))) if m.rip => {
                    let t = m.disp as u64;
                    if is_exec(t) {
                        seed(t);
                    }
                }
                (Op::Mov | Op::Movabs, Some(Operand::Imm(v))) => {
                    let t = v as u64;
                    if is_exec(t) {
                        seed(t);
                    }
                }
                _ => {}
            }
            a += inst.len.max(1) as u64;
        }
    }

    /// Every candidate instruction start in `[lo, hi)`, by linear sweep: from
    /// each anchor (a known instruction start -- a symbol, a block discovery
    /// reached, a data pointer) decode forward, following instruction
    /// lengths, until the next anchor. Seeding *every byte* instead, as the
    /// A64 sweep does with every word, is wrong twice over on x86-64: most
    /// byte offsets fall inside an instruction and decode as garbage (libc came
    /// out at 1.2 million blocks, ten times its instruction count), and the
    /// garbage exercises lifter paths no real program has.
    pub fn sweep(
        fetch: &dyn Fn(u64, &mut FetchBuf) -> usize,
        lo: u64,
        hi: u64,
        anchors: &[u64],
        emit: &mut dyn FnMut(u64),
    ) {
        let mut starts: Vec<u64> = anchors.iter().copied().filter(|&a| a >= lo && a < hi).collect();
        starts.sort_unstable();
        starts.dedup();
        if starts.first().map_or(true, |&f| f > lo) {
            starts.insert(0, lo);
        }
        for (i, &s) in starts.iter().enumerate() {
            let next = starts.get(i + 1).copied().unwrap_or(hi);
            let mut a = s;
            while a < next {
                let mut buf = [0u8; 16];
                let n = fetch(a, &mut buf);
                if n == 0 {
                    // Not executable here: step to the next byte and try to
                    // resynchronise.
                    a += 1;
                    continue;
                }
                let inst = gex_isa_x86::decode_in(mode_at(a), &buf[..n], a);
                if inst.is_known() {
                    emit(a);
                }
                a += inst.len.max(1) as u64;
            }
        }
    }

    pub fn disasm(bytes: &[u8], pc: u64) -> (String, usize) {
        let inst = gex_isa_x86::decode_in(mode_at(pc), bytes, pc);
        let n = inst.len as usize;
        let hex: String = bytes[..n.min(bytes.len())]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        (
            format!("{hex:<30}\t{}", gex_isa_x86::fmt::disasm(&inst, pc)),
            n.max(1),
        )
    }
}

pub use imp::*;
