//! The function table's host half (docs/merge-plan.md §9.8), shared by both
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

use gex_abi::ftab::{RESERVED, RESOLVE};
use std::collections::BTreeMap;

/// What made a function: the tiers of §9.2, for statistics and for what a
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
#[derive(Clone, Debug)]
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
pub struct FuncMap {
    next: u32,
    by_addr: BTreeMap<u64, Func>,
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
        FuncMap { next: RESERVED, by_addr: BTreeMap::new(), installs: Vec::new(), log: Vec::new(), retired: 0 }
    }

    /// How large every table must be to hold every index handed out so far.
    pub fn size(&self) -> u32 {
        self.next
    }

    /// The function at `addr`, if any is installed.
    pub fn get(&self, addr: u64) -> Option<Func> {
        self.by_addr.get(&addr).copied()
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
            if let Some(old) = self.by_addr.insert(addr, Func { idx: base + off, install: n }) {
                self.log.push(Event::Retire(old.idx));
                self.retired += 1;
            }
        }
        n
    }

    /// Map more addresses to slots of install `install`: one module serving
    /// several images (Wine's PE side as one module, docs/wine-native.md
    /// §3.14, bound DLL by DLL as each maps). Nothing changes in any table
    /// (the slots are filled already); an address already mapped is taken
    /// over as by [`FuncMap::publish`].
    pub fn bind(&mut self, install: u32, funcs: &[(u64, u32)]) {
        let Install { base, len, .. } = self.installs[install as usize].clone();
        for &(addr, off) in funcs {
            if addr == 0 || off >= len {
                continue;
            }
            if let Some(old) = self.by_addr.insert(addr, Func { idx: base + off, install }) {
                if old.idx != base + off {
                    self.log.push(Event::Retire(old.idx));
                    self.retired += 1;
                }
            }
        }
    }

    /// Forget `addr`'s function: its index is pointed back at the resolve
    /// stub in every table (as each catches up) and a lookup translates the
    /// address again. Returns the retired index.
    pub fn retire(&mut self, addr: u64) -> Option<u32> {
        let f = self.by_addr.remove(&addr)?;
        self.log.push(Event::Retire(f.idx));
        self.retired += 1;
        Some(f.idx)
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

/// What the host asks a run-time translator (the function JIT, stream F)
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

/// Write `addr → idx` into the probe cache in guest memory `mem`.
pub fn fcache_write(mem: &mut [u8], addr: u64, idx: u32) {
    let at = gex_abi::fcache_entry(addr) as usize;
    if let Some(e) = mem.get_mut(at..at + 8) {
        e.copy_from_slice(&gex_abi::fcache_pack(addr, idx).to_le_bytes());
    }
}

/// Clear `addr`'s probe cache entry if it names `addr` (another address in
/// the slot keeps it).
pub fn fcache_clear(mem: &mut [u8], addr: u64) {
    let at = gex_abi::fcache_entry(addr) as usize;
    if let Some(e) = mem.get_mut(at..at + 8) {
        let v = u64::from_le_bytes(e.try_into().unwrap());
        if v & 0xffff_ffff == addr & 0xffff_ffff {
            e.copy_from_slice(&gex_abi::fcache_pack(0, RESOLVE).to_le_bytes());
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
    fn probe_cache_entries() {
        let mut mem = vec![0u8; (gex_abi::FCACHE + gex_abi::FCACHE_BYTES) as usize];
        fcache_write(&mut mem, 0x40_1234, 99);
        let at = gex_abi::fcache_entry(0x40_1234) as usize;
        let v = u64::from_le_bytes(mem[at..at + 8].try_into().unwrap());
        assert_eq!((v as u32, (v >> 32) as u32), (0x40_1234, 99));
        // Another address sharing the slot does not clear it.
        fcache_clear(&mut mem, 0x40_1235);
        assert_eq!(u64::from_le_bytes(mem[at..at + 8].try_into().unwrap()), v);
        fcache_clear(&mut mem, 0x40_1234);
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
