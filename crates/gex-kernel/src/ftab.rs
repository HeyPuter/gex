//! The function table's host half, shared by both
//! shims: which guest address has which [`gex_abi::ftab`] index, process-wide.
//!
//! A table belongs to one store natively and one Worker in the browser, so
//! every thread has its own, numbered alike. What is the same everywhere
//! lives here:
//!
//! * **indices**, allocated in ranges (a module fills a range from its
//!   `__table_base`), never reused: a retired index keeps meaning "re-resolve"
//!   for as long as anything may still hold it;
//! * **the address map**, authoritative: guest address → index;
//! * **the log** every table replays from its own watermark, so a thread
//!   that has not caught up yet finds out what to fill and what to point
//!   back at the resolve stub ([`Event`]);
//! * **the probe cache** in guest memory (`gex_abi::FCACHE`), written on a
//!   lookup and cleared on retirement.
//!
//! What a table *holds* is the host's: a store's `wasmtime::Table`, a
//! Worker's `WebAssembly.Table`. A slot nobody has filled holds the resolve
//! stub, so a thread behind the log is never wrong, only slower.

use gex_abi::ftab::RESERVED;
use std::collections::BTreeMap;

/// What made a function: which tier built it, for statistics and for what a
/// retirement has to check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// A unit built ahead of time (clang).
    Unit,
    /// The function JIT.
    Jit,
    /// The region tier, or tier-1 behind its adapter.
    Region,
}

/// Losses of its bridge in a row after which a provider address is left to
/// tier-1 (`elfprov::rebridge`'s loop guard).
pub const REBRIDGE_LIMIT: u32 = 64;

/// Whether the re-bridge loop guard gives a provider address up to tier-1
/// after `losses` losses in a row: only past [`REBRIDGE_LIMIT`], and only an
/// address tier-1 can lift. An IFUNC's resolver+1 is odd, and handed one the
/// emitter panicked ("text base must be instruction-aligned": node
/// --build-snapshot and `node -e` in the page), so such an address keeps
/// its bridge whatever the count.
pub fn rebridge_gives_up(pc: u64, losses: u32) -> bool {
    losses > REBRIDGE_LIMIT && pc % gex_abi::INST_ALIGN == 0
}

/// An install's module once its program is gone ([`FuncMap::kill_module`]):
/// a table catching up skips it.
pub const DEAD_MODULE: u32 = u32::MAX;

/// One module's functions in the table: `len` slots from `base`, filled by
/// the module's element segment when a thread instantiates it. `module` is
/// the host's handle for it (an index into its own list of modules).
#[derive(Clone, Debug)]
pub struct Install {
    pub base: u32,
    pub len: u32,
    pub module: u32,
    pub tier: Tier,
}

/// What a table replays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Instantiate install `i` (an index into [`FuncMap::installs`]).
    Install(u32),
    /// Point this index back at the resolve stub.
    Retire(u32),
}

/// An address's function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Func {
    pub idx: u32,
    /// The install holding it.
    pub install: u32,
}

/// The process's function table, host side.
#[derive(Clone)]
pub struct FuncMap {
    next: u32,
    by_addr: BTreeMap<u64, Func>,
    /// How many addresses map to each index: one module serving several
    /// images (Wine's PE side, bound per image as each maps) gives one
    /// index to the same function at every address it is mapped at, and an
    /// index is retired only when the last of them goes.
    refs: std::collections::HashMap<u32, u32>,
    installs: Vec<Install>,
    log: Vec<Event>,
    /// Indices handed out and later retired, for statistics.
    pub retired: u64,
}

impl Default for FuncMap {
    fn default() -> Self {
        Self::new()
    }
}

impl FuncMap {
    pub fn new() -> FuncMap {
        FuncMap { next: RESERVED, by_addr: BTreeMap::new(), refs: Default::default(), installs: Vec::new(), log: Vec::new(), retired: 0 }
    }

    /// Map `addr` to `f`, and retire the index it had if nothing else
    /// maps to that one.
    fn map_addr(&mut self, addr: u64, f: Func) {
        *self.refs.entry(f.idx).or_insert(0) += 1;
        if let Some(old) = self.by_addr.insert(addr, f) {
            self.unref(old.idx);
        }
    }

    /// One address fewer maps to `idx`: when it was the last, the index is
    /// pointed back at the resolve stub in every table.
    fn unref(&mut self, idx: u32) {
        let n = self.refs.entry(idx).or_insert(1);
        *n = n.saturating_sub(1);
        if *n == 0 {
            self.refs.remove(&idx);
            self.log.push(Event::Retire(idx));
            self.retired += 1;
        }
    }

    /// How large every table must be to hold every index handed out so far.
    pub fn size(&self) -> u32 {
        self.next
    }

    /// The function at `addr`, if any is installed.
    /// A lifted twin's marked address is none (only [`FuncMap::twin`]
    /// finds it): a guest's jump there must not reach it, nor fill the probe
    /// cache with it under the export's own low half.
    pub fn get(&self, addr: u64) -> Option<Func> {
        if addr & gex_abi::ftab::TWIN != 0 {
            return None;
        }
        self.by_addr.get(&addr).copied()
    }

    /// The lifted twin of the export at `addr` (`gex_abi::ftab::TWIN`), if
    /// a unit installed one.
    pub fn twin(&self, addr: u64) -> Option<Func> {
        self.by_addr.get(&(addr | gex_abi::ftab::TWIN)).copied()
    }

    pub fn installs(&self) -> &[Install] {
        &self.installs
    }

    pub fn install(&self, i: u32) -> &Install {
        &self.installs[i as usize]
    }

    /// Everything a table at watermark `from` has still to do.
    pub fn log_from(&self, from: usize) -> &[Event] {
        &self.log[from.min(self.log.len())..]
    }

    pub fn log_len(&self) -> usize {
        self.log.len()
    }

    /// Reserve `len` consecutive indices for a module's element segment and
    /// record it, mapping each `(address, offset)` of `funcs` to `base +
    /// offset`. An address already mapped is taken over: the newer
    /// translation wins (the old index is retired). Returns the install's
    /// number; the caller instantiates it with `__table_base` =
    /// `installs()[n].base`.
    pub fn publish(&mut self, len: u32, module: u32, tier: Tier, funcs: &[(u64, u32)]) -> u32 {
        let base = self.next;
        self.next = self.next.checked_add(len.max(1)).expect("function table index space exhausted");
        let n = self.installs.len() as u32;
        self.installs.push(Install { base, len, module, tier });
        self.log.push(Event::Install(n));
        for &(addr, off) in funcs {
            if addr == 0 || off >= len {
                continue;
            }
            self.map_addr(addr, Func { idx: base + off, install: n });
        }
        n
    }

    /// [`FuncMap::publish`] at `base` when nothing has taken it yet: a fork's
    /// child of a wasm32 program installs the parent's module where the
    /// parent had it, because the function pointers in the data it copied
    /// are `__table_base + i` for the parent's base (an exec'd program's base
    /// is past its predecessor's range, which is never reused). Returns
    /// whether the install got that base.
    pub fn publish_at(&mut self, base: u32, len: u32, module: u32, tier: Tier, funcs: &[(u64, u32)]) -> (u32, bool) {
        if base >= self.next {
            self.next = base;
        }
        let n = self.publish(len, module, tier, funcs);
        (n, self.installs[n as usize].base == base)
    }

    /// Map more addresses to slots of install `install`: one module serving
    /// several images (Wine's PE side as one module, bound DLL by DLL as
    /// each maps). Nothing changes in any table
    /// (the slots are filled already); an address already mapped is taken
    /// over as by [`FuncMap::publish`].
    pub fn bind(&mut self, install: u32, funcs: &[(u64, u32)]) {
        let Install { base, len, .. } = self.installs[install as usize].clone();
        for &(addr, off) in funcs {
            if addr == 0 || off >= len {
                continue;
            }
            self.map_addr(addr, Func { idx: base + off, install });
        }
    }

    /// Forget `addr`'s function: its index is pointed back at the resolve
    /// stub in every table (as each catches up) and a lookup translates the
    /// address again. Returns the retired index. An entry's lifted twin
    /// (`gex_abi::ftab::TWIN`) goes with it.
    pub fn retire(&mut self, addr: u64) -> Option<u32> {
        use gex_abi::ftab::TWIN;
        if addr & TWIN == 0 {
            if let Some(t) = self.by_addr.remove(&(addr | TWIN)) {
                self.unref(t.idx);
            }
        }
        let f = self.by_addr.remove(&addr)?;
        self.unref(f.idx);
        Some(f.idx)
    }

    /// Mark every install of `module` [`DEAD_MODULE`]: a table that catches
    /// up later (a Worker for a thread started after an `execve`) skips it
    /// rather than instantiate a module whose program is gone -- a replaced
    /// library's provider, linked as something else and with its data where
    /// the new program's now is. Unit modules are not killed: an exec'd
    /// program that maps the same object at the same place reuses its
    /// install, and a Worker that skipped it would lift block by block.
    pub fn kill_module(&mut self, module: u32) {
        for i in &mut self.installs {
            if i.module == module {
                i.module = DEAD_MODULE;
            }
        }
    }

    /// Retire every function whose entry lies in `[start, end)`.
    pub fn retire_range(&mut self, start: u64, end: u64) -> Vec<(u64, u32)> {
        let gone: Vec<u64> = self.by_addr.range(start..end).map(|(a, _)| *a).collect();
        let mut out = Vec::with_capacity(gone.len());
        for a in gone {
            if let Some(i) = self.retire(a) {
                out.push((a, i));
            }
        }
        out
    }

    /// The functions whose entry lies in `[start, end)`.
    pub fn range(&self, start: u64, end: u64) -> impl Iterator<Item = (u64, Func)> + '_ {
        self.by_addr.range(start..end).map(|(a, f)| (*a, *f))
    }

    /// Number of addresses mapped.
    pub fn len(&self) -> usize {
        self.by_addr.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_addr.is_empty()
    }
}

/// What the host asks a run-time translator (the function JIT)
/// for when a lookup finds no function at an address.
pub struct Request<'a> {
    /// The guest process asking: what a translator learns of one process's
    /// code must not be applied to another's (one native runner hosts a whole
    /// process tree, whose images share addresses). Unique for the run; a
    /// forked child shares its parent's until it `execve`s.
    pub process: u64,
    /// Guest memory, as the guest sees it.
    pub mem: &'a [u8],
    /// The address looked up: a function entry, as far as the caller knows.
    pub addr: u64,
    /// Whether the guest may execute the byte at an address.
    pub executable: &'a dyn Fn(u64) -> bool,
    /// Whether `n` bytes at an address are mapped readable and not
    /// writable now: a jump table read from them is as fixed as one in an
    /// image's read-only segment (not tracked past a later `mprotect`).
    pub read_only: &'a dyn Fn(u64, u64) -> bool,
    /// Whether an address is mapped writable and executable now (a JIT's
    /// code area): its bytes may change with no flush to announce it, so
    /// tier-1 guards each block there (`Block::guard`) and the function
    /// JIT leaves it alone.
    pub volatile: &'a dyn Fn(u64) -> bool,
    /// The value `gex_abi::PAGE_GEN` holds for a page (guest address /
    /// `DISPATCH_PAGE`) now, never 0: what the translation's page checks
    /// compare against. Asking numbers the page if it was not.
    pub page_gen: &'a dyn Fn(u64) -> u32,
    /// An i386 (compatibility-mode) program.
    pub class32: bool,
}

/// A translation: one module, whose `gex.funcs` section lists the
/// addresses of the slots its element segment fills from `__table_base`.
/// It imports `gex.ftab`, `env.__table_base`, `gex.memory` and any `gex.*`
/// host function translated code imports.
pub struct Translated {
    pub wasm: Vec<u8>,
}

/// A run-time translator. `None` leaves the address to the fallback
/// ([`gex_abi::ftab::FALLBACK`]: tier-1, until the region tier).
pub trait Translator: Send + Sync {
    fn translate(&self, req: &Request<'_>) -> Option<Translated>;
    /// Guest process `process` starts running `program` (the file `bytes`,
    /// when known): a new process, or one that `execve`d. Whatever was
    /// learned of `process`'s old image -- function starts, read-only
    /// ranges, builtins by address -- describes addresses the new one holds.
    fn image(&self, _process: u64, _program: &gex_ld::Program, _bytes: Option<&[u8]>) {}
    /// Guest process `process` is gone (its last reference dropped).
    fn forget(&self, _process: u64) {}
    /// Guest process `process` unmapped or reprotected the code in
    /// `ranges` (`Kernel::retired_code`): what the translator learned of
    /// those addresses no longer describes them.
    fn retire(&self, _process: u64, _ranges: &[(u64, u64)]) {}
}

static TRANSLATOR: std::sync::OnceLock<Box<dyn Translator>> = std::sync::OnceLock::new();

/// Install the process's run-time translator (`gexc run --fjit`), before
/// the program starts. A second call is ignored.
pub fn set_translator(t: Box<dyn Translator>) {
    let _ = TRANSLATOR.set(t);
}

/// The run-time translator, if one was installed.
pub fn translator() -> Option<&'static dyn Translator> {
    TRANSLATOR.get().map(|b| &**b)
}

/// Write `addr → idx` into the probe cache in guest memory `mem`. An
/// address or index no entry can hold (`gex_abi::fcache_pack`) clears the
/// slot instead, so it is left to miss.
pub fn fcache_write(mem: &mut [u8], addr: u64, idx: u32) {
    let at = gex_abi::fcache_entry(addr) as usize;
    let v = gex_abi::fcache_pack(addr, idx).unwrap_or(FCACHE_EMPTY);
    if let Some(e) = mem.get_mut(at..at + 8) {
        e.copy_from_slice(&v.to_le_bytes());
    }
}

/// An empty probe cache entry: address 0, the resolve stub.
const FCACHE_EMPTY: u64 = 0;

/// Empty the whole probe cache: every entry the resolve stub. For a memory
/// whose cache names another table's indices -- an `execve`'s new program,
/// or a fork's child, whose low region (and the cache in it) is a copy of
/// the parent's while its table is its own.
pub fn fcache_reset(mem: &mut [u8]) {
    let (lo, hi) = (gex_abi::FCACHE as usize, (gex_abi::FCACHE + gex_abi::FCACHE_BYTES) as usize);
    let empty = FCACHE_EMPTY.to_le_bytes();
    if let Some(c) = mem.get_mut(lo..hi) {
        for e in c.chunks_exact_mut(8) {
            e.copy_from_slice(&empty);
        }
    }
}

/// Clear `addr`'s probe cache entry if it names `addr` (another address in
/// the slot keeps it).
pub fn fcache_clear(mem: &mut [u8], addr: u64) {
    let at = gex_abi::fcache_entry(addr) as usize;
    if let Some(e) = mem.get_mut(at..at + 8) {
        let v = u64::from_le_bytes(e.try_into().unwrap());
        if v & gex_abi::FCACHE_TAG_MASK == addr {
            e.copy_from_slice(&FCACHE_EMPTY.to_le_bytes());
        }
    }
}

/// A module's install descriptor (`gex_abi::ftab::FUNCS_SECTION`): the guest
/// address of each slot its element segment fills, in order. `None` if the
/// module has no such section.
pub fn module_funcs(wasm: &[u8]) -> Option<Vec<u64>> {
    let body = custom_section(wasm, gex_abi::ftab::FUNCS_SECTION)?;
    let n = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
    let addrs = body.get(4..4 + 8 * n)?;
    Some(addrs.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect())
}

/// The table slots a position-independent module (wasm-ld `-shared`) needs
/// from its `__table_base`, from its `dylink.0` section.
pub fn dylink_table_size(wasm: &[u8]) -> Option<u32> {
    let body = custom_section(wasm, "dylink.0")?;
    // Subsections: id, size, payload; WASM_DYLINK_MEM_INFO (1) is
    // mem_size, mem_p2align, table_size, table_p2align, all LEB.
    let mut at = 0;
    while at < body.len() {
        let id = body[at];
        at += 1;
        let (len, n) = leb(body.get(at..)?)?;
        at += n;
        let sub = body.get(at..at + len as usize)?;
        if id == 1 {
            let mut p = 0;
            let mut v = [0u32; 4];
            for x in &mut v {
                let (val, n) = leb(&sub[p..])?;
                *x = val;
                p += n;
            }
            return Some(v[2]);
        }
        at += len as usize;
    }
    None
}

/// The payload of the first custom section called `name`.
pub fn custom_section<'a>(wasm: &'a [u8], name: &str) -> Option<&'a [u8]> {
    if wasm.get(..4)? != b"\0asm" {
        return None;
    }
    let mut at = 8;
    while at < wasm.len() {
        let id = wasm[at];
        at += 1;
        let (len, n) = leb(wasm.get(at..)?)?;
        at += n;
        let sec = wasm.get(at..at + len as usize)?;
        at += len as usize;
        if id != 0 {
            continue;
        }
        let (nlen, n) = leb(sec)?;
        let nm = sec.get(n..n + nlen as usize)?;
        if nm == name.as_bytes() {
            return sec.get(n + nlen as usize..);
        }
    }
    None
}

fn leb(b: &[u8]) -> Option<(u32, usize)> {
    let mut v: u32 = 0;
    for (i, &x) in b.iter().enumerate().take(5) {
        v |= ((x & 0x7f) as u32) << (7 * i);
        if x & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit's lifted twin (`gex_abi::ftab::TWIN`) is found only as the
    /// twin of its export -- never by the address a guest could jump to --
    /// and goes when the export's own entry is retired.
    #[test]
    fn a_lifted_twin_is_reachable_only_as_a_twin() {
        use gex_abi::ftab::TWIN;
        let mut m = FuncMap::new();
        let unit = m.publish(3, 1, Tier::Unit, &[(0x1000, 0), (0x1000 | TWIN, 1), (0x2000, 2)]);
        let prov = m.publish(1, 2, Tier::Unit, &[(0x1000, 0)]);
        assert_eq!(m.get(0x1000).map(|f| f.install), Some(prov));
        assert_eq!(m.twin(0x1000).map(|f| f.install), Some(unit));
        assert!(m.get(0x1000 | TWIN).is_none());
        assert!(m.twin(0x2000).is_none());
        assert_eq!(m.range(0, 0x10000).count(), 2);
        m.retire(0x1000);
        assert!(m.twin(0x1000).is_none());
        assert!(m.get(0x2000).is_some());
    }

    #[test]
    fn the_rebridge_guard_never_gives_up_an_unliftable_address() {
        // SP's page crash: provider address 0x71dc1a1 (libc +0x9c1a1).
        // Every x86 address is instruction-aligned, so only aarch64 has
        // addresses the guard must keep.
        let unaligned: &[u64] = if gex_abi::INST_ALIGN > 1 { &[0x71dc1a1, 0xa12c1a1, 0xa12cac1] } else { &[] };
        for &pc in unaligned {
            for n in [1, REBRIDGE_LIMIT, REBRIDGE_LIMIT + 1, 1000, u32::MAX] {
                assert!(!rebridge_gives_up(pc, n), "{pc:#x} after {n}");
            }
        }
        let aligned = 0x71dc1a0u64;
        assert!(!rebridge_gives_up(aligned, REBRIDGE_LIMIT));
        assert!(rebridge_gives_up(aligned, REBRIDGE_LIMIT + 1));
    }

    #[test]
    fn kill_module_marks_only_that_modules_installs() {
        let mut m = FuncMap::new();
        let a = m.publish(3, 7, Tier::Unit, &[(0x1000, 0)]);
        let b = m.publish(1, 0x8000_0001, Tier::Unit, &[(0x2000, 0)]);
        m.kill_module(0x8000_0001);
        assert_eq!(m.install(a).module, 7);
        assert_eq!(m.install(b).module, DEAD_MODULE);
    }

    #[test]
    fn publish_maps_and_retires() {
        let mut m = FuncMap::new();
        let a = m.publish(3, 7, Tier::Unit, &[(0x1000, 0), (0x2000, 2), (0, 1)]);
        assert_eq!(m.install(a).base, RESERVED);
        assert_eq!(m.get(0x2000).unwrap().idx, RESERVED + 2);
        assert_eq!(m.size(), RESERVED + 3);
        // A newer translation of 0x2000 takes it over.
        let b = m.publish(1, 8, Tier::Jit, &[(0x2000, 0)]);
        assert_eq!(m.get(0x2000).unwrap(), Func { idx: RESERVED + 3, install: b });
        assert!(matches!(m.log_from(0), [Event::Install(0), Event::Install(1), Event::Retire(i)] if *i == RESERVED + 2));
        assert_eq!(m.retire(0x1000), Some(RESERVED));
        assert!(m.get(0x1000).is_none());
    }

    #[test]
    fn a_shared_index_stays_while_another_address_maps_to_it() {
        // One module serving two images (Wine's PE side: kernel32 loaded,
        // and a view of the same image mapped elsewhere): both addresses of
        // a function share its index. Retiring one (its view unmapped, or
        // its page rewritten) must not retire the index the other uses: a
        // table pointed back at the resolve stub while the map still sends
        // the other address there loops in the resolve stub for ever.
        let mut m = FuncMap::new();
        let a = m.publish(4, 7, Tier::Unit, &[(0x7bfa_21f8, 2)]);
        m.bind(a, &[(0x1fcb_21f8, 2)]);
        let idx = m.get(0x7bfa_21f8).unwrap().idx;
        assert_eq!(m.get(0x1fcb_21f8).unwrap().idx, idx);
        let before = m.log_len();
        assert_eq!(m.retire(0x1fcb_21f8), Some(idx));
        assert!(!m.log_from(before).contains(&Event::Retire(idx)), "the index the loaded image uses was retired");
        assert_eq!(m.get(0x7bfa_21f8).unwrap().idx, idx);
        // The last address going retires it.
        assert_eq!(m.retire(0x7bfa_21f8), Some(idx));
        assert!(m.log_from(before).contains(&Event::Retire(idx)));
        // Rebinding an address to the index it has is no retire either.
        let b = m.publish(2, 9, Tier::Unit, &[(0x5000, 1)]);
        let before = m.log_len();
        m.bind(b, &[(0x5000, 1)]);
        assert!(m.log_from(before).is_empty());
    }

    #[test]
    fn probe_cache_entries() {
        let mut mem = vec![0u8; (gex_abi::FCACHE + gex_abi::FCACHE_BYTES) as usize];
        fcache_write(&mut mem, 0x40_1234, 99);
        let at = gex_abi::fcache_entry(0x40_1234) as usize;
        let v = u64::from_le_bytes(mem[at..at + 8].try_into().unwrap());
        assert_eq!(gex_abi::fcache_probe(v, 0x40_1234), 99);
        // Another address sharing the slot does not clear it.
        fcache_clear(&mut mem, 0x40_1235);
        assert_eq!(u64::from_le_bytes(mem[at..at + 8].try_into().unwrap()), v);
        fcache_clear(&mut mem, 0x40_1234);
        assert_eq!(u64::from_le_bytes(mem[at..at + 8].try_into().unwrap()), 0);
    }

    /// A 64-bit memory's code above 4 GiB: an entry names its whole address,
    /// so a function at t + 4 GiB (the same slot, the same low 32 bits) is
    /// never taken for the one at t, and the one above 4 GiB hits.
    #[test]
    fn probe_cache_entries_above_4_gib() {
        let mut mem = vec![0u8; (gex_abi::FCACHE + gex_abi::FCACHE_BYTES) as usize];
        let (t, high) = (0x2000_0000u64, 0x1_2000_0000u64);
        assert_eq!(gex_abi::fcache_entry(t), gex_abi::fcache_entry(high));
        let at = gex_abi::fcache_entry(t) as usize;
        let entry = |mem: &[u8]| u64::from_le_bytes(mem[at..at + 8].try_into().unwrap());
        fcache_write(&mut mem, high, 7);
        assert_eq!(gex_abi::fcache_probe(entry(&mem), high), 7);
        assert_eq!(gex_abi::fcache_probe(entry(&mem), t), gex_abi::ftab::RESOLVE);
        fcache_write(&mut mem, t, 8);
        assert_eq!(gex_abi::fcache_probe(entry(&mem), t), 8);
        assert_eq!(gex_abi::fcache_probe(entry(&mem), high), gex_abi::ftab::RESOLVE);
        // A Wine image near the top of a 16 GiB memory.
        fcache_write(&mut mem, 0x3_ffdd_1ce4, 9);
        let at = gex_abi::fcache_entry(0x3_ffdd_1ce4) as usize;
        assert_eq!(gex_abi::fcache_probe(u64::from_le_bytes(mem[at..at + 8].try_into().unwrap()), 0x3_ffdd_1ce4), 9);
        // The other address clearing the slot leaves it; its own clears it.
        fcache_clear(&mut mem, 0x3_ffdd_1ce4 + (1 << 32));
        assert_ne!(u64::from_le_bytes(mem[at..at + 8].try_into().unwrap()), 0);
        fcache_clear(&mut mem, 0x3_ffdd_1ce4);
        assert_eq!(u64::from_le_bytes(mem[at..at + 8].try_into().unwrap()), 0);
        // What no entry can hold misses rather than wraps.
        fcache_write(&mut mem, 1 << 40, 3);
        let at = gex_abi::fcache_entry(1 << 40) as usize;
        assert_eq!(u64::from_le_bytes(mem[at..at + 8].try_into().unwrap()), 0);
    }

    #[test]
    fn custom_sections_parse() {
        // A module with only a `gex.funcs` section listing two addresses.
        let mut payload = Vec::new();
        payload.push(9u8);
        payload.extend_from_slice(b"gex.funcs");
        payload.extend_from_slice(&2u32.to_le_bytes());
        payload.extend_from_slice(&0x1000u64.to_le_bytes());
        payload.extend_from_slice(&0u64.to_le_bytes());
        let mut w = b"\0asm\x01\0\0\0".to_vec();
        w.push(0);
        w.push(payload.len() as u8);
        w.extend_from_slice(&payload);
        assert_eq!(module_funcs(&w), Some(vec![0x1000, 0]));
        assert_eq!(dylink_table_size(&w), None);
    }
}
