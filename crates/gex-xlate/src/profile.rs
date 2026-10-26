//! Guest-level execution profiles and the session manifest.
//!
//! A **profile** says how often code ran: exact block and function entry
//! counts, the hottest targets of each indirect branch, returns that did not
//! go back to their call, entries into the middle of known code, and which
//! code made which system calls. A **session manifest** says what a run
//! consisted of: the objects it loaded, the system calls and files it used,
//! its environment and argv, and the guest ISA. `gex-eject` consumes both.
//!
//! **A profile may only ever change speed, never correctness.** Every consumer
//! must stay correct whatever a profile contains: stale, truncated, from
//! another program, or empty. A profile names code by *content*, so a changed
//! binary simply stops matching; it is never a reason to assume a block,
//! target or function exists.
//!
//! # Keys
//!
//! Code is named by `(object, offset)`. The object is `sha256:<hex>` of the
//! whole file, so the same libc in two programs, sysroots or paths is one
//! object. The offset is the **link-time virtual address**: the runtime
//! address less the object's load bias, which is what its symbol table,
//! `.eh_frame` and `gex_ld::elf::load(bytes, 0)` use. Code that belongs to no
//! file (a guest JIT's) is recorded under the pseudo-object `anon`, with the
//! absolute address as its offset; it is not comparable across runs.
//!
//! # Collection
//!
//! `GEX_PROFILE=1` (or `gexc run --record DIR`) builds every module with
//! the counters in `gex_emit::profile`, which documents where they live and
//! their layout. The host reads them back with [`parse_thread_memory`],
//! resolves addresses to objects, and folds them into a [`Profile`]. Only
//! tier-1 code is instrumented; profiling and the function tier are exclusive.
//!
//! # File formats
//!
//! Both files are JSON, versioned by a `format` and a `version` field; a
//! reader rejects a version it does not know. Numbers are decimal and
//! unsigned. See [`Profile::to_json`] and [`Session::to_json`] for the exact
//! shapes.

use std::collections::BTreeMap;
use std::fmt::Write as _;

pub use gex_emit::profile::{
    enabled, set_enabled, EXPORT_INIT, EXPORT_SIZE, MEMORY_IMPORT, REGIONS_START,
};
use gex_emit::profile as layout;

/// The profile format's name and version.
pub const PROFILE_FORMAT: &str = "gex-profile";
pub const PROFILE_VERSION: u64 = 1;
/// The session manifest's name and version.
pub const SESSION_FORMAT: &str = "gex-session";
pub const SESSION_VERSION: u64 = 1;

/// The pseudo-object for code with no file behind it.
pub const ANON: &str = "anon";

// ---------------------------------------------------------------------------
// Reading the counters out of a profile memory
// ---------------------------------------------------------------------------

/// One block's counters as the instrumented code left them, by address.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawBlock {
    pub start: u64,
    pub end: u64,
    pub flags: u32,
    /// Static successor (Goto target, Branch taken, Syscall next), or 0.
    pub target: u64,
    /// Branch not-taken successor, or a call's return address, or 0.
    pub aux: u64,
    pub entries: u64,
    pub call: u64,
    pub jump: u64,
    pub ret: u64,
    pub taken: u64,
    /// Indirect jump or call: `(total, replaced, [(target, count)])`.
    pub indirect: Option<(u64, u64, Vec<(u64, u64)>)>,
    /// Return: `(expected, unexpected, unchecked)`.
    pub returns: Option<(u64, u64, u64)>,
}

impl RawBlock {
    /// Whether anything was counted at all.
    pub fn is_empty(&self) -> bool {
        self.entries == 0
            && self.call == 0
            && self.jump == 0
            && self.ret == 0
            && self.taken == 0
            && self.indirect.as_ref().is_none_or(|i| i.0 == 0)
            && self.returns.is_none_or(|r| r == (0, 0, 0))
    }
}

fn rd32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}
fn rd64(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}

/// Why a profile memory could not be read.
#[derive(Debug)]
pub struct Corrupt(pub String);

impl std::fmt::Display for Corrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "corrupt profile memory: {}", self.0)
    }
}
impl std::error::Error for Corrupt {}

/// Every block record in a thread's profile memory, with nothing-counted
/// blocks left out. When `zero` is set the counters are cleared as they are
/// read (the descriptors stay), so the next read returns only what happened
/// since: that is how a live process is dumped without counting twice.
pub fn parse_thread_memory(mem: &mut [u8], zero: bool) -> Result<Vec<RawBlock>, Corrupt> {
    let bad = |m: &str| Corrupt(m.to_string());
    if rd64(mem, 0) != Some(layout::THREAD_MAGIC) {
        return Err(bad("thread magic"));
    }
    let end = rd32(mem, layout::REGIONS_END as usize).ok_or_else(|| bad("header"))? as usize;
    if end > mem.len() {
        return Err(bad("regions end past the memory"));
    }
    let mut out = Vec::new();
    let mut at = layout::REGIONS_START as usize;
    while at < end {
        if rd32(mem, at) != Some(layout::REGION_MAGIC) {
            return Err(Corrupt(format!("region magic at {at:#x}")));
        }
        if rd32(mem, at + 4) != Some(layout::REGION_VERSION) {
            return Err(Corrupt(format!("region version at {at:#x}")));
        }
        let n = rd32(mem, at + 8).unwrap() as usize;
        let size = rd32(mem, at + 12).ok_or_else(|| bad("region size"))? as usize;
        let desc = rd32(mem, at + 16).ok_or_else(|| bad("desc"))? as usize;
        if size == 0 || at + size > end {
            return Err(Corrupt(format!("region size at {at:#x}")));
        }
        for i in 0..n {
            let d = at + desc + i * layout::DESC_BYTES as usize;
            let start = rd64(mem, d).ok_or_else(|| bad("descriptor"))?;
            let bend = rd64(mem, d + 8).unwrap_or(0);
            let target = rd64(mem, d + 16).unwrap_or(0);
            let aux = rd64(mem, d + 24).unwrap_or(0);
            let rec = at + rd32(mem, d + 32).unwrap_or(0) as usize;
            let fl = rd32(mem, d + 36).unwrap_or(0);
            let len = layout::record_bytes(fl) as usize;
            if rec + len > at + size {
                return Err(Corrupt(format!("record of {start:#x} outside its region")));
            }
            let r = |o: u32| rd64(mem, rec + o as usize).unwrap_or(0);
            let mut b = RawBlock {
                start,
                end: bend,
                flags: fl,
                target,
                aux,
                entries: r(layout::REC_ENTRIES),
                call: r(8),
                jump: r(16),
                ret: r(24),
                ..Default::default()
            };
            match fl & layout::flags::TERM_MASK {
                layout::flags::TERM_BRANCH => b.taken = r(layout::REC_TAKEN),
                layout::flags::TERM_INDIRECT if fl & layout::flags::RET != 0 => {
                    b.returns = Some((
                        r(layout::REC_RET_EXPECTED),
                        r(layout::REC_RET_UNEXPECTED),
                        r(layout::REC_RET_UNCHECKED),
                    ))
                }
                layout::flags::TERM_INDIRECT => {
                    let slots = (0..layout::IND_SLOTS)
                        .map(|k| {
                            let o = layout::REC_IND_SLOTS + k * 16;
                            (r(o), r(o + 8))
                        })
                        .filter(|(_, c)| *c != 0)
                        .collect();
                    b.indirect = Some((r(layout::REC_IND_TOTAL), r(layout::REC_IND_REPLACED), slots));
                }
                _ => {}
            }
            if zero {
                mem[rec..rec + len].fill(0);
            }
            if !b.is_empty() {
                out.push(b);
            }
        }
        at += size;
    }
    Ok(out)
}

/// Write a fresh thread header into a new profile memory (at least
/// `REGIONS_START` bytes).
pub fn init_thread_memory(mem: &mut [u8]) {
    mem[..8].copy_from_slice(&layout::THREAD_MAGIC.to_le_bytes());
    mem[8..12].copy_from_slice(&layout::THREAD_VERSION.to_le_bytes());
    mem[layout::REGIONS_END as usize..layout::REGIONS_END as usize + 4]
        .copy_from_slice(&layout::REGIONS_START.to_le_bytes());
}

/// Reserve a region of `size` bytes: returns its base and bumps `REGIONS_END`.
/// The caller must first make the memory at least `base + size` long.
pub fn region_end(mem: &[u8]) -> u32 {
    rd32(mem, layout::REGIONS_END as usize).unwrap_or(layout::REGIONS_START)
}

/// Record that `[base, base + size)` is taken.
pub fn set_region_end(mem: &mut [u8], end: u32) {
    mem[layout::REGIONS_END as usize..layout::REGIONS_END as usize + 4]
        .copy_from_slice(&end.to_le_bytes());
}

/// Where the next region goes, and the memory size it needs.
pub fn next_region(mem: &[u8], size: u32) -> (u32, u64) {
    let a = layout::REGION_ALIGN;
    let base = (region_end(mem) + a - 1) & !(a - 1);
    (base, base as u64 + size as u64)
}

/// The function the thread is currently in, as far as the shadow stack
/// knows: the callee of the innermost call, or `None` at the outermost
/// frame (or after the ring wrapped).
pub fn current_function(mem: &[u8]) -> Option<u64> {
    let top = rd32(mem, layout::SS_TOP as usize)?;
    let floor = rd32(mem, layout::SS_FLOOR as usize)?;
    if top <= floor {
        return None;
    }
    let i = (top - 1) & (layout::SS_ENTRIES - 1);
    rd64(mem, (layout::SS_BASE + i * layout::SS_ENTRY_BYTES + 8) as usize)
}

// ---------------------------------------------------------------------------
// Naming addresses
// ---------------------------------------------------------------------------

/// Code named by content: an object id (`sha256:…` or [`ANON`]) and the
/// link-time address inside it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Loc {
    pub obj: String,
    pub off: u64,
}

/// Turns a runtime address into a [`Loc`]. The host builds one from what it
/// knows about the process: its images and its file-backed mappings.
pub trait Resolve {
    fn resolve(&self, pc: u64) -> Option<Loc>;
}

/// A resolver over a list of placed objects.
#[derive(Clone, Debug, Default)]
pub struct Placements {
    /// `(start, end, object id, bias)`: `off = pc - bias` for pcs in range.
    pub ranges: Vec<(u64, u64, String, u64)>,
}

impl Placements {
    pub fn add(&mut self, start: u64, end: u64, obj: &str, bias: u64) {
        if !self
            .ranges
            .iter()
            .any(|r| r.0 == start && r.1 == end && r.2 == obj && r.3 == bias)
        {
            self.ranges.push((start, end, obj.to_string(), bias));
        }
    }
}

impl Resolve for Placements {
    fn resolve(&self, pc: u64) -> Option<Loc> {
        // Latest first: a range reused by a later mapping names the later
        // object.
        self.ranges
            .iter()
            .rev()
            .find(|(s, e, _, _)| pc >= *s && pc < *e)
            .map(|(_, _, o, b)| Loc {
                obj: o.clone(),
                off: pc.wrapping_sub(*b),
            })
    }
}

fn loc_or_anon(r: &dyn Resolve, pc: u64) -> Loc {
    r.resolve(pc).unwrap_or(Loc {
        obj: ANON.to_string(),
        off: pc,
    })
}

/// The load bias of an ELF object mapped so that file offset `file_off` sits
/// at address `at`: the `PT_LOAD` holding that offset says which virtual
/// address it is. `None` for a file that is not a little-endian ELF64 or
/// ELF32, or an offset no `PT_LOAD` covers.
pub fn elf_bias(file: &[u8], file_off: u64, at: u64) -> Option<u64> {
    if file.get(..4)? != b"\x7fELF" || *file.get(5)? != 1 {
        return None;
    }
    let is64 = *file.get(4)? == 2;
    let (phoff, phentsize, phnum) = if is64 {
        (
            rd64(file, 0x20)?,
            u16::from_le_bytes(file.get(0x36..0x38)?.try_into().ok()?) as u64,
            u16::from_le_bytes(file.get(0x38..0x3a)?.try_into().ok()?) as u64,
        )
    } else {
        (
            rd32(file, 0x1c)? as u64,
            u16::from_le_bytes(file.get(0x2a..0x2c)?.try_into().ok()?) as u64,
            u16::from_le_bytes(file.get(0x2c..0x2e)?.try_into().ok()?) as u64,
        )
    };
    for i in 0..phnum {
        let p = (phoff + i * phentsize) as usize;
        if rd32(file, p)? != 1 {
            continue; // PT_LOAD
        }
        let (off, vaddr, filesz) = if is64 {
            (rd64(file, p + 8)?, rd64(file, p + 16)?, rd64(file, p + 32)?)
        } else {
            (
                rd32(file, p + 4)? as u64,
                rd32(file, p + 8)? as u64,
                rd32(file, p + 16)? as u64,
            )
        };
        if file_off >= off && file_off < off + filesz.max(1) {
            let vaddr_here = vaddr + (file_off - off);
            return Some(at.wrapping_sub(vaddr_here));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The profile
// ---------------------------------------------------------------------------

/// Per-block counts. `n` counts every time the block began to run; `call`,
/// `jump` and `ret` count the entries that arrived by a call, an indirect
/// jump or a return; `taken` counts a conditional branch's taken arm.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockCounts {
    pub end: u64,
    pub n: u64,
    pub call: u64,
    pub jump: u64,
    pub ret: u64,
    pub taken: u64,
}

/// An indirect jump or call site.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndirectSite {
    pub call: bool,
    pub total: u64,
    /// How many times a table slot was reused for a new target. Zero means
    /// the counts below are exact and complete.
    pub replaced: u64,
    pub targets: BTreeMap<Loc, u64>,
}

/// A return site.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReturnSite {
    pub expected: u64,
    pub unexpected: u64,
    pub unchecked: u64,
}

/// Everything recorded about one object.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectProfile {
    /// Paths this object was seen under (informational).
    pub paths: Vec<String>,
    pub blocks: BTreeMap<u64, BlockCounts>,
    pub indirect: BTreeMap<u64, IndirectSite>,
    pub returns: BTreeMap<u64, ReturnSite>,
    /// `(syscall instruction offset, enclosing function, canonical number)`.
    pub syscalls: BTreeMap<(u64, Option<Loc>, u64), u64>,
}

/// A whole profile.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    /// `aarch64` or `x86_64`.
    pub isa: String,
    /// How many runs were folded in.
    pub runs: u64,
    pub objects: BTreeMap<String, ObjectProfile>,
}

impl Profile {
    pub fn new(isa: &str) -> Profile {
        Profile {
            isa: isa.to_string(),
            runs: 1,
            objects: BTreeMap::new(),
        }
    }

    fn obj(&mut self, id: &str) -> &mut ObjectProfile {
        self.objects.entry(id.to_string()).or_default()
    }

    /// Note a path an object was seen under.
    pub fn note_path(&mut self, id: &str, path: &str) {
        let o = self.obj(id);
        if !o.paths.iter().any(|p| p == path) {
            o.paths.push(path.to_string());
        }
    }

    /// Fold in raw counters, naming addresses through `r`.
    pub fn add_raw(&mut self, raw: &[RawBlock], r: &dyn Resolve) {
        for b in raw {
            let at = loc_or_anon(r, b.start);
            let end = b.end.wrapping_sub(b.start).wrapping_add(at.off);
            let o = self.obj(&at.obj);
            let c = o.blocks.entry(at.off).or_default();
            if c.end == 0 || end < c.end {
                c.end = end;
            }
            c.n += b.entries;
            c.call += b.call;
            c.jump += b.jump;
            c.ret += b.ret;
            c.taken += b.taken;
            if let Some((total, replaced, slots)) = &b.indirect {
                let targets: Vec<(Loc, u64)> =
                    slots.iter().map(|(t, n)| (loc_or_anon(r, *t), *n)).collect();
                let site = self.obj(&at.obj).indirect.entry(at.off).or_default();
                site.call = b.flags & layout::flags::CALL != 0;
                site.total += total;
                site.replaced += replaced;
                for (t, n) in targets {
                    *site.targets.entry(t).or_insert(0) += n;
                }
            }
            if let Some((e, u, x)) = b.returns {
                let site = self.obj(&at.obj).returns.entry(at.off).or_default();
                site.expected += e;
                site.unexpected += u;
                site.unchecked += x;
            }
        }
    }

    /// Count one system call made at `pc` from inside `func`.
    pub fn add_syscall(&mut self, pc: u64, func: Option<u64>, nr: u64, n: u64, r: &dyn Resolve) {
        let at = loc_or_anon(r, pc);
        let func = func.map(|f| loc_or_anon(r, f));
        *self
            .obj(&at.obj)
            .syscalls
            .entry((at.off, func, nr))
            .or_insert(0) += n;
    }

    /// Fold another profile into this one. Objects match by content hash, so
    /// merging profiles of different programs combines their shared
    /// libraries. Refuses profiles of another ISA.
    pub fn merge(&mut self, other: &Profile) -> Result<(), String> {
        if !self.isa.is_empty() && !other.isa.is_empty() && self.isa != other.isa {
            return Err(format!("cannot merge a {} profile into a {} one", other.isa, self.isa));
        }
        if self.isa.is_empty() {
            self.isa = other.isa.clone();
        }
        self.runs += other.runs;
        for (id, o) in &other.objects {
            let me = self.obj(id);
            for p in &o.paths {
                if !me.paths.contains(p) {
                    me.paths.push(p.clone());
                }
            }
            for (off, c) in &o.blocks {
                let m = me.blocks.entry(*off).or_default();
                if m.end == 0 || (c.end != 0 && c.end < m.end) {
                    m.end = c.end;
                }
                m.n += c.n;
                m.call += c.call;
                m.jump += c.jump;
                m.ret += c.ret;
                m.taken += c.taken;
            }
            for (off, s) in &o.indirect {
                let m = me.indirect.entry(*off).or_default();
                m.call |= s.call;
                m.total += s.total;
                m.replaced += s.replaced;
                for (t, n) in &s.targets {
                    *m.targets.entry(t.clone()).or_insert(0) += n;
                }
            }
            for (off, s) in &o.returns {
                let m = me.returns.entry(*off).or_default();
                m.expected += s.expected;
                m.unexpected += s.unexpected;
                m.unchecked += s.unchecked;
            }
            for (k, n) in &o.syscalls {
                *me.syscalls.entry(k.clone()).or_insert(0) += n;
            }
        }
        Ok(())
    }

    /// Only these objects (by id), for a per-object store.
    pub fn only(&self, id: &str) -> Profile {
        Profile {
            isa: self.isa.clone(),
            runs: self.runs,
            objects: self
                .objects
                .iter()
                .filter(|(k, _)| k.as_str() == id)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    /// Entries into the middle of other known code: blocks that start
    /// strictly inside another block of the same object and were reached by
    /// a call, an indirect jump or a return. `(object, offset, containing
    /// block, dynamic entries)`. Function boundaries, once known, give the
    /// stronger form of the same question: dynamic entries that are not a
    /// function's start.
    pub fn interior_entries(&self) -> Vec<(String, u64, u64, u64)> {
        let mut out = Vec::new();
        for (id, o) in &self.objects {
            if id == ANON {
                continue;
            }
            // Sweep in address order holding the block that reaches furthest.
            let mut reach: Option<(u64, u64)> = None; // (start, end)
            for (off, c) in &o.blocks {
                if let Some((s, e)) = reach {
                    let dynamic = c.call + c.jump + c.ret;
                    if *off > s && *off < e && dynamic > 0 {
                        out.push((id.clone(), *off, s, dynamic));
                    }
                }
                if reach.is_none_or(|(_, e)| c.end > e) {
                    reach = Some((*off, c.end));
                }
            }
        }
        out
    }

    /// Function entry counts as far as they are known without function
    /// boundaries: every block entered by a call, with how many calls.
    pub fn function_entries(&self) -> Vec<(Loc, u64)> {
        let mut out = Vec::new();
        for (id, o) in &self.objects {
            for (off, c) in &o.blocks {
                if c.call > 0 {
                    out.push((
                        Loc {
                            obj: id.clone(),
                            off: *off,
                        },
                        c.call,
                    ));
                }
            }
        }
        out
    }

    /// The profile as JSON. Shape (version 1):
    ///
    /// ```text
    /// { "format": "gex-profile", "version": 1, "isa": "aarch64", "runs": 1,
    ///   "objects": [ {
    ///     "id": "sha256:…" | "anon", "paths": ["/lib/libc.so"],
    ///     "blocks":   [[off, end, n, call, jump, ret, taken], …],
    ///     "indirect": [{"off": o, "call": bool, "total": n, "replaced": n,
    ///                   "targets": [[obj, off, n], …] (top 4, hottest first),
    ///                   "other": n}, …],
    ///     "returns":  [[off, expected, unexpected, unchecked], …],
    ///     "interior": [[off, containing_block_off, dynamic_entries], …],
    ///     "syscalls": [[off, func_obj|null, func_off|null, nr, name, n], …]
    ///   } ] }
    /// ```
    pub fn to_json(&self) -> String {
        let interior = self.interior_entries();
        let mut s = String::with_capacity(1 << 16);
        let _ = write!(
            s,
            "{{\"format\":\"{PROFILE_FORMAT}\",\"version\":{PROFILE_VERSION},\"isa\":{},\"runs\":{},\n\"objects\":[",
            jstr(&self.isa),
            self.runs
        );
        for (i, (id, o)) in self.objects.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(s, "\n{{\"id\":{},\"paths\":[", jstr(id));
            for (k, p) in o.paths.iter().enumerate() {
                if k > 0 {
                    s.push(',');
                }
                s.push_str(&jstr(p));
            }
            s.push_str("],\n\"blocks\":[");
            for (k, (off, c)) in o.blocks.iter().enumerate() {
                if k > 0 {
                    s.push(',');
                }
                let _ = write!(
                    s,
                    "[{off},{},{},{},{},{},{}]",
                    c.end, c.n, c.call, c.jump, c.ret, c.taken
                );
            }
            s.push_str("],\n\"indirect\":[");
            for (k, (off, site)) in o.indirect.iter().enumerate() {
                if k > 0 {
                    s.push(',');
                }
                let mut t: Vec<(&Loc, &u64)> = site.targets.iter().collect();
                t.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                let shown: u64 = t.iter().take(4).map(|x| *x.1).sum();
                let _ = write!(
                    s,
                    "{{\"off\":{off},\"call\":{},\"total\":{},\"replaced\":{},\"targets\":[",
                    site.call, site.total, site.replaced
                );
                for (j, (l, n)) in t.iter().take(4).enumerate() {
                    if j > 0 {
                        s.push(',');
                    }
                    let _ = write!(s, "[{},{},{}]", jstr(&l.obj), l.off, n);
                }
                let _ = write!(s, "],\"other\":{}}}", site.total.saturating_sub(shown));
            }
            s.push_str("],\n\"returns\":[");
            for (k, (off, r)) in o.returns.iter().enumerate() {
                if k > 0 {
                    s.push(',');
                }
                let _ = write!(s, "[{off},{},{},{}]", r.expected, r.unexpected, r.unchecked);
            }
            s.push_str("],\n\"interior\":[");
            let mut first = true;
            for (oid, off, within, n) in interior.iter().filter(|x| &x.0 == id) {
                let _ = oid;
                if !first {
                    s.push(',');
                }
                first = false;
                let _ = write!(s, "[{off},{within},{n}]");
            }
            s.push_str("],\n\"syscalls\":[");
            for (k, ((off, func, nr), n)) in o.syscalls.iter().enumerate() {
                if k > 0 {
                    s.push(',');
                }
                let (fo, ff) = match func {
                    Some(l) => (jstr(&l.obj), l.off.to_string()),
                    None => ("null".into(), "null".into()),
                };
                let _ = write!(s, "[{off},{fo},{ff},{nr},{},{n}]", jstr(syscall_name(*nr)));
            }
            s.push_str("]}");
        }
        s.push_str("\n]}\n");
        s
    }

    /// Read a profile written by [`Self::to_json`]. Fails loudly on another
    /// format, a version this build does not know, or a malformed file.
    pub fn from_json(text: &str) -> Result<Profile, String> {
        let v = Json::parse(text)?;
        if v.get("format").and_then(Json::as_str) != Some(PROFILE_FORMAT) {
            return Err("not a gex profile (format field)".into());
        }
        match v.get("version").and_then(Json::as_u64) {
            Some(PROFILE_VERSION) => {}
            other => return Err(format!("unsupported gex profile version {other:?}")),
        }
        let mut p = Profile {
            isa: v.get("isa").and_then(Json::as_str).unwrap_or("").to_string(),
            runs: v.get("runs").and_then(Json::as_u64).unwrap_or(1),
            objects: BTreeMap::new(),
        };
        let objs = v.get("objects").and_then(Json::as_arr).ok_or("objects")?;
        for o in objs {
            let id = o.get("id").and_then(Json::as_str).ok_or("object id")?.to_string();
            let mut op = ObjectProfile::default();
            for path in o.get("paths").and_then(Json::as_arr).unwrap_or(&[]) {
                op.paths.push(path.as_str().ok_or("path")?.to_string());
            }
            for b in o.get("blocks").and_then(Json::as_arr).unwrap_or(&[]) {
                let f = nums(b, 7).ok_or("block row")?;
                op.blocks.insert(
                    f[0],
                    BlockCounts {
                        end: f[1],
                        n: f[2],
                        call: f[3],
                        jump: f[4],
                        ret: f[5],
                        taken: f[6],
                    },
                );
            }
            for site in o.get("indirect").and_then(Json::as_arr).unwrap_or(&[]) {
                let off = site.get("off").and_then(Json::as_u64).ok_or("indirect off")?;
                let mut s = IndirectSite {
                    call: site.get("call").and_then(Json::as_bool).unwrap_or(false),
                    total: site.get("total").and_then(Json::as_u64).unwrap_or(0),
                    replaced: site.get("replaced").and_then(Json::as_u64).unwrap_or(0),
                    targets: BTreeMap::new(),
                };
                for t in site.get("targets").and_then(Json::as_arr).unwrap_or(&[]) {
                    let row = t.as_arr().ok_or("target row")?;
                    let obj = row.first().and_then(Json::as_str).ok_or("target obj")?;
                    let toff = row.get(1).and_then(Json::as_u64).ok_or("target off")?;
                    let n = row.get(2).and_then(Json::as_u64).ok_or("target n")?;
                    s.targets.insert(
                        Loc {
                            obj: obj.to_string(),
                            off: toff,
                        },
                        n,
                    );
                }
                op.indirect.insert(off, s);
            }
            for r in o.get("returns").and_then(Json::as_arr).unwrap_or(&[]) {
                let f = nums(r, 4).ok_or("return row")?;
                op.returns.insert(
                    f[0],
                    ReturnSite {
                        expected: f[1],
                        unexpected: f[2],
                        unchecked: f[3],
                    },
                );
            }
            for sc in o.get("syscalls").and_then(Json::as_arr).unwrap_or(&[]) {
                let row = sc.as_arr().ok_or("syscall row")?;
                let off = row.first().and_then(Json::as_u64).ok_or("syscall off")?;
                let func = match (row.get(1), row.get(2)) {
                    (Some(Json::Str(o)), Some(f)) => Some(Loc {
                        obj: o.clone(),
                        off: f.as_u64().ok_or("syscall func off")?,
                    }),
                    _ => None,
                };
                let nr = row.get(3).and_then(Json::as_u64).ok_or("syscall nr")?;
                let n = row.get(5).and_then(Json::as_u64).ok_or("syscall n")?;
                *op.syscalls.entry((off, func, nr)).or_insert(0) += n;
            }
            p.objects.insert(id, op);
        }
        Ok(p)
    }

    /// A human summary: the hottest blocks, indirect sites and anomalies.
    pub fn summary(&self, top: usize) -> String {
        let mut s = String::new();
        let name = |id: &str| -> String {
            self.objects
                .get(id)
                .and_then(|o| o.paths.first())
                .map(|p| p.rsplit('/').next().unwrap_or(p).to_string())
                .unwrap_or_else(|| id.chars().take(20).collect())
        };
        let total: u64 = self
            .objects
            .values()
            .flat_map(|o| o.blocks.values())
            .map(|c| c.n)
            .sum();
        let _ = writeln!(
            s,
            "{} profile, {} run(s), {} objects, {} block entries",
            self.isa,
            self.runs,
            self.objects.len(),
            total
        );
        for (id, o) in &self.objects {
            let n: u64 = o.blocks.values().map(|c| c.n).sum();
            let _ = writeln!(
                s,
                "  {:<24} {:>14} entries  {:>7} blocks  {:>5} indirect sites  {:>5} return sites  {}",
                name(id),
                n,
                o.blocks.len(),
                o.indirect.len(),
                o.returns.len(),
                id
            );
        }
        let mut hot: Vec<(u64, &str, u64)> = self
            .objects
            .iter()
            .flat_map(|(id, o)| o.blocks.iter().map(move |(off, c)| (c.n, id.as_str(), *off)))
            .collect();
        hot.sort_by(|a, b| b.0.cmp(&a.0));
        let _ = writeln!(s, "hottest blocks:");
        for (n, id, off) in hot.iter().take(top) {
            let _ = writeln!(s, "  {n:>14}  {}+{off:#x}", name(id));
        }
        let mut sites: Vec<(u64, &str, u64, &IndirectSite)> = self
            .objects
            .iter()
            .flat_map(|(id, o)| o.indirect.iter().map(move |(off, x)| (x.total, id.as_str(), *off, x)))
            .collect();
        sites.sort_by(|a, b| b.0.cmp(&a.0));
        let _ = writeln!(s, "hottest indirect sites:");
        for (n, id, off, site) in sites.iter().take(top) {
            let mut t: Vec<(&Loc, &u64)> = site.targets.iter().collect();
            t.sort_by(|a, b| b.1.cmp(a.1));
            let targets: Vec<String> = t
                .iter()
                .take(4)
                .map(|(l, c)| format!("{}+{:#x}×{c}", name(&l.obj), l.off))
                .collect();
            let _ = writeln!(
                s,
                "  {n:>14}  {}+{off:#x} {}{}",
                name(id),
                if site.call { "call " } else { "" },
                targets.join(" ")
            );
        }
        let unexpected: u64 = self
            .objects
            .values()
            .flat_map(|o| o.returns.values())
            .map(|r| r.unexpected)
            .sum();
        let _ = writeln!(s, "unexpected returns: {unexpected}");
        let interior = self.interior_entries();
        let _ = writeln!(s, "interior entries: {} sites", interior.len());
        for (id, off, within, n) in interior.iter().take(top) {
            let _ = writeln!(s, "  {n:>14}  {}+{off:#x} (inside {within:#x})", name(id));
        }
        s
    }
}

fn nums(v: &Json, n: usize) -> Option<Vec<u64>> {
    let a = v.as_arr()?;
    if a.len() < n {
        return None;
    }
    a.iter().take(n).map(Json::as_u64).collect()
}

static SYSCALL_NAMER: std::sync::OnceLock<fn(u64) -> &'static str> = std::sync::OnceLock::new();

/// Name syscalls with the kernel's full table from now on. The first call wins.
pub fn set_syscall_namer(namer: fn(u64) -> &'static str) {
    let _ = SYSCALL_NAMER.set(namer);
}

/// The canonical (AArch64 generic) name of a syscall number, for readers.
/// Numbers in profiles and manifests are canonical: an x86-64 guest's are
/// translated before counting.
pub fn syscall_name(nr: u64) -> &'static str {
    if let Some(namer) = SYSCALL_NAMER.get() {
        return namer(nr);
    }
    // A small table of the common ones; `gex-kernel` has the full set (and
    // gex's private numbers for x86-only calls), but this crate does not
    // depend on it, so a host installs it with `set_syscall_namer`. Unknown
    // numbers print as "?".
    match nr {
        17 => "getcwd",
        23 => "dup",
        24 => "dup3",
        25 => "fcntl",
        29 => "ioctl",
        34 => "mkdirat",
        35 => "unlinkat",
        48 => "faccessat",
        49 => "chdir",
        56 => "openat",
        57 => "close",
        59 => "pipe2",
        61 => "getdents64",
        62 => "lseek",
        63 => "read",
        64 => "write",
        65 => "readv",
        66 => "writev",
        67 => "pread64",
        68 => "pwrite64",
        72 => "pselect6",
        73 => "ppoll",
        78 => "readlinkat",
        79 => "newfstatat",
        80 => "fstat",
        93 => "exit",
        94 => "exit_group",
        96 => "set_tid_address",
        98 => "futex",
        99 => "set_robust_list",
        101 => "nanosleep",
        113 => "clock_gettime",
        115 => "clock_nanosleep",
        124 => "sched_yield",
        129 => "kill",
        130 => "tkill",
        131 => "tgkill",
        134 => "rt_sigaction",
        135 => "rt_sigprocmask",
        139 => "rt_sigreturn",
        160 => "uname",
        169 => "gettimeofday",
        172 => "getpid",
        173 => "getppid",
        174 => "getuid",
        175 => "geteuid",
        176 => "getgid",
        177 => "getegid",
        178 => "gettid",
        198 => "socket",
        203 => "connect",
        214 => "brk",
        215 => "munmap",
        216 => "mremap",
        220 => "clone",
        221 => "execve",
        222 => "mmap",
        226 => "mprotect",
        233 => "madvise",
        260 => "wait4",
        261 => "prlimit64",
        278 => "getrandom",
        291 => "statx",
        _ => "?",
    }
}

// ---------------------------------------------------------------------------
// The session manifest
// ---------------------------------------------------------------------------

/// One object the session loaded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectEntry {
    /// `sha256:<hex>` of the file.
    pub id: String,
    /// The path the guest named it by.
    pub path: String,
    /// Where gex read it from on the host, when that differs (a sysroot).
    pub host_path: Option<String>,
    pub size: u64,
    /// `exe`, `interp` or `library`.
    pub role: String,
}

/// One file the guest opened (or tried to).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    /// Opened for reading, for writing, or created.
    pub read: bool,
    pub write: bool,
    pub create: bool,
    pub directory: bool,
    /// Looked at without being opened (`stat`, `access`, `readlink`): a file
    /// the program needs to be there even if it never reads it (Qt checks a
    /// QML source's time and loads its cached compilation instead).
    pub stat: bool,
    pub ok: u64,
    pub failed: u64,
    /// The last errno a failed open returned.
    pub errno: Option<i64>,
}

/// One program the session ran (the first, and every `execve`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgramEntry {
    pub pid: i64,
    pub exe: String,
    pub argv: Vec<String>,
    pub env: Vec<String>,
}

/// What a run consisted of: what `gex-eject` needs to rebuild it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    pub isa: String,
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub cwd: Option<String>,
    pub sysroot: Option<String>,
    pub objects: Vec<ObjectEntry>,
    /// Canonical number -> count.
    pub syscalls: BTreeMap<u64, u64>,
    pub files: BTreeMap<String, FileEntry>,
    pub programs: Vec<ProgramEntry>,
    pub processes: u64,
    pub threads: u64,
    /// `status N` or `signal N`.
    pub exit: Option<String>,
}

impl Session {
    pub fn add_object(&mut self, e: ObjectEntry) {
        if !self.objects.iter().any(|o| o.id == e.id && o.path == e.path) {
            self.objects.push(e);
        }
    }

    /// The manifest as JSON. Shape (version 1):
    ///
    /// ```text
    /// { "format": "gex-session", "version": 1, "isa": "aarch64",
    ///   "argv": [...], "env": [...], "cwd": "/"|null, "sysroot": "…"|null,
    ///   "objects":  [{"id": "sha256:…", "path": "/lib/libc.so",
    ///                 "host_path": "…"|null, "size": n, "role": "library"}],
    ///   "programs": [{"pid": 1, "exe": "…", "argv": [...], "env": [...]}],
    ///   "processes": n, "threads": n,
    ///   "syscalls": [{"nr": 64, "name": "write", "n": 12}],
    ///   "files":    [{"path": "/etc/passwd", "read": true, "write": false,
    ///                 "create": false, "directory": false, "stat": false, "ok": 1,
    ///                 "failed": 0, "errno": null}],
    ///   "exit": "status 0" | "signal 11" | null }
    /// ```
    pub fn to_json(&self) -> String {
        let mut s = String::new();
        let strs = |v: &[String]| -> String {
            let parts: Vec<String> = v.iter().map(|x| jstr(x)).collect();
            format!("[{}]", parts.join(","))
        };
        let opt = |v: &Option<String>| v.as_ref().map(|x| jstr(x)).unwrap_or_else(|| "null".into());
        let _ = write!(
            s,
            "{{\"format\":\"{SESSION_FORMAT}\",\"version\":{SESSION_VERSION},\"isa\":{},\n\"argv\":{},\n\"env\":{},\n\"cwd\":{},\"sysroot\":{},\n\"objects\":[",
            jstr(&self.isa),
            strs(&self.argv),
            strs(&self.env),
            opt(&self.cwd),
            opt(&self.sysroot)
        );
        for (i, o) in self.objects.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(
                s,
                "\n{{\"id\":{},\"path\":{},\"host_path\":{},\"size\":{},\"role\":{}}}",
                jstr(&o.id),
                jstr(&o.path),
                opt(&o.host_path),
                o.size,
                jstr(&o.role)
            );
        }
        s.push_str("],\n\"programs\":[");
        for (i, p) in self.programs.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(
                s,
                "\n{{\"pid\":{},\"exe\":{},\"argv\":{},\"env\":{}}}",
                p.pid,
                jstr(&p.exe),
                strs(&p.argv),
                strs(&p.env)
            );
        }
        let _ = write!(
            s,
            "],\n\"processes\":{},\"threads\":{},\n\"syscalls\":[",
            self.processes, self.threads
        );
        for (i, (nr, n)) in self.syscalls.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(s, "\n{{\"nr\":{nr},\"name\":{},\"n\":{n}}}", jstr(syscall_name(*nr)));
        }
        s.push_str("],\n\"files\":[");
        for (i, f) in self.files.values().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(
                s,
                "\n{{\"path\":{},\"read\":{},\"write\":{},\"create\":{},\"directory\":{},\"stat\":{},\"ok\":{},\"failed\":{},\"errno\":{}}}",
                jstr(&f.path),
                f.read,
                f.write,
                f.create,
                f.directory,
                f.stat,
                f.ok,
                f.failed,
                f.errno.map(|e| e.to_string()).unwrap_or_else(|| "null".into())
            );
        }
        let _ = write!(s, "],\n\"exit\":{}", opt(&self.exit));
        s.push_str("}\n");
        s
    }

    /// Read a manifest written by [`Self::to_json`].
    pub fn from_json(text: &str) -> Result<Session, String> {
        let v = Json::parse(text)?;
        if v.get("format").and_then(Json::as_str) != Some(SESSION_FORMAT) {
            return Err("not a gex session manifest (format field)".into());
        }
        match v.get("version").and_then(Json::as_u64) {
            Some(SESSION_VERSION) => {}
            other => return Err(format!("unsupported gex session version {other:?}")),
        }
        let strs = |k: &str, o: &Json| -> Vec<String> {
            o.get(k)
                .and_then(Json::as_arr)
                .unwrap_or(&[])
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        };
        let ostr = |k: &str, o: &Json| o.get(k).and_then(Json::as_str).map(str::to_string);
        let mut s = Session {
            isa: ostr("isa", &v).unwrap_or_default(),
            argv: strs("argv", &v),
            env: strs("env", &v),
            cwd: ostr("cwd", &v),
            sysroot: ostr("sysroot", &v),
            processes: v.get("processes").and_then(Json::as_u64).unwrap_or(0),
            threads: v.get("threads").and_then(Json::as_u64).unwrap_or(0),
            exit: ostr("exit", &v),
            ..Default::default()
        };
        for o in v.get("objects").and_then(Json::as_arr).unwrap_or(&[]) {
            s.objects.push(ObjectEntry {
                id: ostr("id", o).ok_or("object id")?,
                path: ostr("path", o).unwrap_or_default(),
                host_path: ostr("host_path", o),
                size: o.get("size").and_then(Json::as_u64).unwrap_or(0),
                role: ostr("role", o).unwrap_or_default(),
            });
        }
        for p in v.get("programs").and_then(Json::as_arr).unwrap_or(&[]) {
            s.programs.push(ProgramEntry {
                pid: p.get("pid").and_then(Json::as_u64).unwrap_or(0) as i64,
                exe: ostr("exe", p).unwrap_or_default(),
                argv: strs("argv", p),
                env: strs("env", p),
            });
        }
        for c in v.get("syscalls").and_then(Json::as_arr).unwrap_or(&[]) {
            let nr = c.get("nr").and_then(Json::as_u64).ok_or("syscall nr")?;
            s.syscalls.insert(nr, c.get("n").and_then(Json::as_u64).unwrap_or(0));
        }
        for f in v.get("files").and_then(Json::as_arr).unwrap_or(&[]) {
            let path = ostr("path", f).ok_or("file path")?;
            let b = |k: &str| f.get(k).and_then(Json::as_bool).unwrap_or(false);
            s.files.insert(
                path.clone(),
                FileEntry {
                    path,
                    read: b("read"),
                    write: b("write"),
                    create: b("create"),
                    directory: b("directory"),
                    stat: b("stat"),
                    ok: f.get("ok").and_then(Json::as_u64).unwrap_or(0),
                    failed: f.get("failed").and_then(Json::as_u64).unwrap_or(0),
                    errno: f.get("errno").and_then(Json::as_i64),
                },
            );
        }
        Ok(s)
    }
}

// ---------------------------------------------------------------------------
// Minimal JSON: enough to write and read the two formats above without a
// dependency (this crate is compiled into the browser shim too).
// ---------------------------------------------------------------------------

/// A JSON string literal.
pub fn jstr(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A parsed JSON value. Numbers keep their text so a `u64` round-trips
/// exactly.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn parse(text: &str) -> Result<Json, String> {
        let b = text.as_bytes();
        let mut p = 0;
        let v = parse_value(b, &mut p)?;
        skip_ws(b, &mut p);
        if p != b.len() {
            return Err(format!("trailing characters at byte {p}"));
        }
        Ok(v)
    }
    pub fn get(&self, k: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(key, _)| key == k).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(n) => n.parse().ok(),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Num(n) => n.parse().ok(),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }
}

fn skip_ws(b: &[u8], p: &mut usize) {
    while *p < b.len() && b[*p].is_ascii_whitespace() {
        *p += 1;
    }
}

fn parse_value(b: &[u8], p: &mut usize) -> Result<Json, String> {
    skip_ws(b, p);
    let c = *b.get(*p).ok_or("unexpected end of JSON")?;
    match c {
        b'{' => {
            *p += 1;
            let mut kv = Vec::new();
            skip_ws(b, p);
            if b.get(*p) == Some(&b'}') {
                *p += 1;
                return Ok(Json::Obj(kv));
            }
            loop {
                skip_ws(b, p);
                let Json::Str(k) = parse_value(b, p)? else {
                    return Err(format!("object key expected at byte {p}"));
                };
                skip_ws(b, p);
                if b.get(*p) != Some(&b':') {
                    return Err(format!("':' expected at byte {p}"));
                }
                *p += 1;
                let v = parse_value(b, p)?;
                kv.push((k, v));
                skip_ws(b, p);
                match b.get(*p) {
                    Some(b',') => *p += 1,
                    Some(b'}') => {
                        *p += 1;
                        return Ok(Json::Obj(kv));
                    }
                    _ => return Err(format!("',' or '}}' expected at byte {p}")),
                }
            }
        }
        b'[' => {
            *p += 1;
            let mut a = Vec::new();
            skip_ws(b, p);
            if b.get(*p) == Some(&b']') {
                *p += 1;
                return Ok(Json::Arr(a));
            }
            loop {
                a.push(parse_value(b, p)?);
                skip_ws(b, p);
                match b.get(*p) {
                    Some(b',') => *p += 1,
                    Some(b']') => {
                        *p += 1;
                        return Ok(Json::Arr(a));
                    }
                    _ => return Err(format!("',' or ']' expected at byte {p}")),
                }
            }
        }
        b'"' => {
            *p += 1;
            let mut out: Vec<u8> = Vec::new();
            loop {
                let c = *b.get(*p).ok_or("unterminated string")?;
                *p += 1;
                match c {
                    b'"' => break,
                    b'\\' => {
                        let e = *b.get(*p).ok_or("unterminated escape")?;
                        *p += 1;
                        match e {
                            b'n' => out.push(b'\n'),
                            b't' => out.push(b'\t'),
                            b'r' => out.push(b'\r'),
                            b'b' => out.push(8),
                            b'f' => out.push(12),
                            b'u' => {
                                let h = b.get(*p..*p + 4).ok_or("short \\u escape")?;
                                let v = u32::from_str_radix(
                                    std::str::from_utf8(h).map_err(|e| e.to_string())?,
                                    16,
                                )
                                .map_err(|e| e.to_string())?;
                                *p += 4;
                                let ch = char::from_u32(v).unwrap_or('\u{fffd}');
                                let mut buf = [0u8; 4];
                                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            }
                            other => out.push(other),
                        }
                    }
                    c => out.push(c),
                }
            }
            Ok(Json::Str(String::from_utf8(out).map_err(|e| e.to_string())?))
        }
        b't' if b[*p..].starts_with(b"true") => {
            *p += 4;
            Ok(Json::Bool(true))
        }
        b'f' if b[*p..].starts_with(b"false") => {
            *p += 5;
            Ok(Json::Bool(false))
        }
        b'n' if b[*p..].starts_with(b"null") => {
            *p += 4;
            Ok(Json::Null)
        }
        b'-' | b'0'..=b'9' => {
            let s = *p;
            *p += 1;
            while *p < b.len() && matches!(b[*p], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                *p += 1;
            }
            Ok(Json::Num(String::from_utf8_lossy(&b[s..*p]).into_owned()))
        }
        other => Err(format!("unexpected '{}' at byte {p}", other as char)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Profile {
        let mut p = Profile::new("aarch64");
        p.note_path("sha256:aa", "/lib/libc.so");
        let o = p.obj("sha256:aa");
        o.blocks.insert(
            0x100,
            BlockCounts {
                end: 0x120,
                n: 10,
                call: 2,
                jump: 0,
                ret: 0,
                taken: 3,
            },
        );
        o.blocks.insert(
            0x110,
            BlockCounts {
                end: 0x120,
                n: 4,
                call: 0,
                jump: 4,
                ret: 0,
                taken: 0,
            },
        );
        let mut t = BTreeMap::new();
        t.insert(
            Loc {
                obj: "sha256:aa".into(),
                off: 0x110,
            },
            4,
        );
        o.indirect.insert(
            0x200,
            IndirectSite {
                call: false,
                total: 4,
                replaced: 0,
                targets: t,
            },
        );
        o.returns.insert(
            0x300,
            ReturnSite {
                expected: 5,
                unexpected: 1,
                unchecked: 2,
            },
        );
        o.syscalls.insert(
            (
                0x400,
                Some(Loc {
                    obj: "sha256:aa".into(),
                    off: 0x3f0,
                }),
                64,
            ),
            7,
        );
        o.syscalls.insert((0x404, None, 93), 1);
        p
    }

    #[test]
    fn profile_json_round_trips() {
        let p = sample();
        let text = p.to_json();
        let q = Profile::from_json(&text).unwrap();
        assert_eq!(p, q);
    }

    #[test]
    fn merge_adds_and_rejects_other_isa() {
        let mut a = sample();
        let b = sample();
        a.merge(&b).unwrap();
        assert_eq!(a.runs, 2);
        let o = &a.objects["sha256:aa"];
        assert_eq!(o.blocks[&0x100].n, 20);
        assert_eq!(o.indirect[&0x200].total, 8);
        assert_eq!(o.returns[&0x300].unexpected, 2);
        let mut x = Profile::new("x86_64");
        assert!(x.merge(&b).is_err());
    }

    #[test]
    fn interior_entries_are_found() {
        let p = sample();
        let i = p.interior_entries();
        assert_eq!(i, vec![("sha256:aa".to_string(), 0x110, 0x100, 4)]);
    }

    #[test]
    fn rejects_unknown_version() {
        let text = sample().to_json().replace("\"version\":1", "\"version\":99");
        assert!(Profile::from_json(&text).is_err());
    }

    #[test]
    fn session_round_trips() {
        let mut s = Session {
            isa: "x86_64".into(),
            argv: vec!["a\"b".into()],
            env: vec!["K=V".into()],
            cwd: Some("/".into()),
            processes: 1,
            threads: 1,
            exit: Some("status 0".into()),
            ..Default::default()
        };
        s.add_object(ObjectEntry {
            id: "sha256:bb".into(),
            path: "/bin/x".into(),
            host_path: None,
            size: 10,
            role: "exe".into(),
        });
        s.syscalls.insert(64, 3);
        s.files.insert(
            "/etc/x".into(),
            FileEntry {
                path: "/etc/x".into(),
                read: true,
                ok: 1,
                failed: 1,
                errno: Some(2),
                ..Default::default()
            },
        );
        s.programs.push(ProgramEntry {
            pid: 1,
            exe: "/bin/x".into(),
            argv: vec!["x".into()],
            env: vec![],
        });
        assert_eq!(Session::from_json(&s.to_json()).unwrap(), s);
    }

    #[test]
    fn thread_memory_parses_and_zeroes() {
        let b1 = gex_ir::Block {
            start: 0x1000,
            end: 0x1010,
            stmts: vec![],
            term: gex_ir::Term::Indirect {
                target: gex_ir::Value(0),
                is_ret: false,
            },
            n_values: 1,
            call_return: None,
            guard: None,
        };
        let region = layout::layout([&b1].into_iter());
        let mut mem = vec![0u8; layout::REGIONS_START as usize + region.size as usize + 64];
        init_thread_memory(&mut mem);
        let (base, need) = next_region(&mem, region.size);
        assert!(need as usize <= mem.len());
        mem[base as usize..base as usize + region.init.len()].copy_from_slice(&region.init);
        set_region_end(&mut mem, base + region.size);
        let rec = base as usize + region.records[&0x1000] as usize;
        mem[rec..rec + 8].copy_from_slice(&5u64.to_le_bytes());
        mem[rec + 32..rec + 40].copy_from_slice(&5u64.to_le_bytes());
        mem[rec + 48..rec + 56].copy_from_slice(&0x2000u64.to_le_bytes());
        mem[rec + 56..rec + 64].copy_from_slice(&5u64.to_le_bytes());
        let raw = parse_thread_memory(&mut mem, true).unwrap();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].entries, 5);
        assert_eq!(raw[0].indirect, Some((5, 0, vec![(0x2000, 5)])));
        assert!(parse_thread_memory(&mut mem, false).unwrap().is_empty());
    }
}
