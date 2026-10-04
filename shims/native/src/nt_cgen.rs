//! The function tier for the NT layer's images (S3, M7; docs/function-tier.md
//! §11).
//!
//! Wine's loader maps every image itself, so a Windows program's code
//! arrives through the runtime translator. With `gexc run --cgen` on a PE
//! program, every image the NT layer maps (the hook in
//! `gex_kernel::nt::section`) gets its function-tier unit, if one was built
//! for exactly those bytes at that base ([`prepare_pe`], `gexc cgen-image`,
//! `gexc cgen-prefix` for Wine's): the whole image's recovered functions as
//! C, linked once into a module of its own. Installing it instantiates that
//! module in every store against the process's body table and dispatcher and
//! writes each entry into the dispatch and body tables at its slot
//! (`publish_cgen_unit`, `install_cgen`): calls inside the image are wasm
//! calls, calls into any other installed image go through the body table,
//! and everything else -- code recovery missed, code generated at run time
//! -- stays the runtime translator's.
//!
//! A unit is keyed by the image as placed (its relocated bytes at its base)
//! and the build of gex: [`image_key`]. Before one is installed, the image's
//! executable bytes in guest memory are compared with what the unit was built
//! from; a mismatch stops the run, naming the first differing address. Code
//! rewritten afterwards (Detours) is caught when its page retires: its bit in
//! `gex_abi::PATCHED_PAGES` is set if the bytes differ ([`on_retire`]), and
//! compiled code leaves for a callee on such a page rather than call it. An
//! image without a unit runs on tier-1, said under `GEX_TRACE`; the
//! program's own exe without one stops the run, since `--cgen` was asked for.

use super::*;
use std::collections::HashMap;
use std::sync::OnceLock;

/// What a Windows program does about units (`gexc run`'s flags).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `--no-cgen`: tier-1 only.
    Off,
    /// The default: units where they exist; an image without one runs on
    /// tier-1 and gets one built in the background for the next run (units
    /// are only ever installed at map time, never swapped in mid-run).
    Default,
    /// `--cgen`: every program image (not Wine's) must have a unit.
    Require,
    /// An ejected project (docs/eject-pe.md): every image, Wine's included,
    /// has a unit built for its base ahead of time; nothing is ever built at
    /// run time, and an image without one stops the run.
    Aot,
}

static MODE: OnceLock<Mode> = OnceLock::new();

/// Set the mode before the program starts.
pub fn set_mode(m: Mode) {
    let _ = MODE.set(m);
}

pub(crate) fn mode() -> Mode {
    MODE.get().copied().unwrap_or(Mode::Default)
}

/// One unit build at a time per unit cache, across processes: a lock file
/// holding the builder's pid, taken over when that process is gone.
pub struct BuildLock(std::path::PathBuf);

impl Drop for BuildLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn build_lock() -> Result<BuildLock> {
    let root = cache_root()?;
    std::fs::create_dir_all(&root)?;
    let path = root.join("build.lock");
    let mut said = false;
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                use std::io::Write;
                let _ = write!(f, "{}", std::process::id());
                return Ok(BuildLock(path));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = std::fs::read_to_string(&path).unwrap_or_default();
                let alive = holder.trim().parse::<u32>().is_ok_and(|p| std::path::Path::new(&format!("/proc/{p}")).exists());
                if !alive && !holder.is_empty() {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if !said {
                    eprintln!("[cgen] waiting for another build (pid {}) to finish", holder.trim());
                    said = true;
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            Err(e) => return Err(anyhow!("{}: {e}", path.display())),
        }
    }
}

/// An image the NT layer mapped, waiting for the host.
#[derive(Clone)]
struct Pending {
    path: String,
    base: u64,
    pe: Arc<gex_ld::pe::PeImage>,
}

static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
/// Set after a push to [`PENDING`]: every syscall drains, and looks here
/// first rather than taking the lock.
static HAS_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ON: OnceLock<()> = OnceLock::new();
/// One unit is placed at a time: slot assignment runs from a snapshot of the
/// page table to the publish.
static PLACING: Mutex<()> = Mutex::new(());

fn trace() -> bool {
    std::env::var_os("GEX_TRACE").is_some() || std::env::var_os("GEX_NT_CGEN_TRACE").is_some()
}

/// Where units live: `GEX_CGEN_CACHE`, else `<gex cache>/cgen`.
pub fn cache_root() -> Result<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("GEX_CGEN_CACHE") {
        return Ok(d.into());
    }
    let modules = gex_cache::cache_dir().ok_or_else(|| anyhow!("no cache directory (set GEX_CGEN_CACHE)"))?;
    Ok(modules.parent().map(|p| p.join("cgen")).unwrap_or(modules))
}

/// The identity of an image's recovery, and of a unit: `gex_cgen::image`'s
/// (gex-key's, shared by every host so they cannot disagree).
pub fn recovery_key(image: &gex_ld::Image) -> String {
    gex_cgen::image::recovery_key(image)
}

pub fn image_key(image: &gex_ld::Image) -> String {
    gex_cgen::image::image_key(image)
}

/// The export providers for a PE image at `base` (`gex_cgen::provider`),
/// its exports by name, and the guest addresses they take over.
struct Provided {
    providers: Vec<gex_cgen::provider::Provider>,
    exports: HashMap<String, u32>,
    stubs: Vec<u64>,
}

fn provided(pe: &gex_ld::pe::PeImage, base: u64) -> Provided {
    let providers = gex_cgen::provider::for_image(&pe.exports.dll_name, !pe.is64, &pe.data);
    let mut exports = HashMap::new();
    if !providers.is_empty() {
        for e in pe.exports.entries.iter().filter(|e| e.rva != 0 && e.forwarder.is_none()) {
            for n in &e.names {
                exports.insert(n.clone(), e.rva);
            }
        }
    }
    let stubs = gex_cgen::provider::stubs(&providers, &exports, base);
    Provided { providers, exports, stubs }
}

/// The key of `image`'s unit built around `pv`'s providers and its
/// indirect-call profile (`icall_hints`): the image's own key when there
/// are neither.
fn unit_key(image: &gex_ld::Image, pv: &Provided) -> String {
    use std::hash::{Hash, Hasher};
    let mut key = image_key(image);
    if !pv.stubs.is_empty() {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        pv.stubs.hash(&mut h);
        gex_cgen::provider::fingerprint(&pv.providers).hash(&mut h);
        key = format!("{key}-p{:016x}", h.finish());
    }
    let hints = icall_hints(image);
    if !hints.is_empty() {
        let mut v: Vec<(u64, u64)> = hints.into_iter().collect();
        v.sort_unstable();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        v.hash(&mut h);
        key = format!("{key}-i{:016x}", h.finish());
    }
    key
}

/// Where [`icall_hints`] reads the image's profile.
pub fn icall_path(image: &gex_ld::Image) -> Result<std::path::PathBuf> {
    Ok(cache_root()?.join("icall").join(format!("{}.txt", gex_key::image_identity(image).trim_start_matches("image:"))))
}

/// Say where the image's indirect-call profile was looked for and how many
/// hints it gave (`gexc cgen-image`). A unit built without one has no
/// devirtualised calls, so its hot virtual calls stay table calls that no
/// engine inlines (the browser units of 2026-10-04 were all built so). When
/// it is missing, each cache in `GEX_CGEN_ICALL_FROM` (colon-separated
/// cache roots) is checked for the same identity's file, and one found is
/// named, loudly.
fn report_hints(image: &gex_ld::Image) {
    let Ok(path) = icall_path(image) else { return };
    if path.exists() {
        eprintln!("[cgen] indirect-call hints: {} from {}", icall_hints(image).len(), path.display());
        return;
    }
    eprintln!("[cgen] indirect-call hints: none (no {})", path.display());
    let name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    for root in std::env::var("GEX_CGEN_ICALL_FROM").unwrap_or_default().split(':').filter(|r| !r.is_empty()) {
        let other = std::path::Path::new(root).join("icall").join(&name);
        if other.exists() {
            eprintln!(
                "[cgen] WARNING: building without the indirect-call profile at {}; copy it to {} for devirtualised calls",
                other.display(),
                path.display()
            );
        }
    }
}

/// The image's indirect-call profile, `<cache>/icall/<image identity>.txt`:
/// lines `icall <return address> <target> <count>` (pewasm's
/// `PEWASM_PROFILE_INDIRECT` dump, in this image's addresses; anything
/// before `icall` is ignored). As pewasm's `read_icall_hints`: a call site
/// whose dominant target takes at least 90% of at least 64 calls gets a
/// guarded direct call to it. By return address.
pub fn icall_hints(image: &gex_ld::Image) -> HashMap<u64, u64> {
    let Ok(path) = icall_path(image) else { return HashMap::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return HashMap::new() };
    let num = |w: &str| u64::from_str_radix(w.trim_start_matches("0x"), if w.starts_with("0x") { 16 } else { 10 }).ok();
    let mut sites: HashMap<u64, HashMap<u64, u64>> = HashMap::new();
    for l in text.lines() {
        let Some(k) = l.find("icall ") else { continue };
        let w: Vec<&str> = l[k + 6..].split_whitespace().collect();
        if let [r, t, n, ..] = w[..] {
            if let (Some(r), Some(t), Some(n)) = (num(r), num(t), num(n)) {
                *sites.entry(r).or_default().entry(t).or_default() += n;
            }
        }
    }
    let mut hints = HashMap::new();
    for (ret, targets) in sites {
        let total: u64 = targets.values().sum();
        let (t, best) = targets.into_iter().max_by_key(|&(t, n)| (n, std::cmp::Reverse(t))).unwrap();
        if best >= 64 && best * 10 >= total * 9 {
            hints.insert(ret, t);
        }
    }
    hints
}

/// Build (or find) the unit's C module for a PE image placed at `base`:
/// recovery once (cached), then C compiled unit by unit. Returns its path.
pub fn prepare_pe(pe: &gex_ld::pe::PeImage, base: u64, jobs: usize, verbose: bool) -> Result<std::path::PathBuf> {
    let (image, seeds) = gex_xlate::func::pe_input(pe, base).map_err(|e| anyhow!("placing the image at {base:#x}: {e}"))?;
    let pv = provided(pe, base);
    prepare_with(&image, &seeds, jobs, verbose, &pv)
}

/// [`prepare_pe`] with function starts of the caller's besides the image's
/// own (gex-eject: entries a recorded session reached that recovery did not
/// find). The unit has the same key: the caller owns the cache it builds
/// into, and starts from a cache without this image's recovery.
pub fn prepare_pe_seeded(pe: &gex_ld::pe::PeImage, base: u64, extra: &[u64], jobs: usize, verbose: bool) -> Result<std::path::PathBuf> {
    let (image, mut seeds) = gex_xlate::func::pe_input(pe, base).map_err(|e| anyhow!("placing the image at {base:#x}: {e}"))?;
    for &a in extra {
        if !seeds.starts.iter().any(|&(s, _)| s == a) {
            seeds.starts.push((a, gex_xlate::func::Source::Entry));
        }
    }
    let pv = provided(pe, base);
    prepare_with(&image, &seeds, jobs, verbose, &pv)
}

/// The recovery directory of a PE image at `base` (what [`prepare_pe`]
/// caches), for a caller that must recover it again.
pub fn recovery_dir(pe: &gex_ld::pe::PeImage, base: u64) -> Result<std::path::PathBuf> {
    let (image, _) = gex_xlate::func::pe_input(pe, base).map_err(|e| anyhow!("placing the image at {base:#x}: {e}"))?;
    Ok(cache_root()?.join("recovery").join(recovery_key(&image)))
}

/// The directory under the unit cache that holds the unit for a PE image
/// placed at `base` (with its providers and hints), built or not.
pub fn unit_dir(pe: &gex_ld::pe::PeImage, base: u64) -> Result<std::path::PathBuf> {
    let (image, _) = gex_xlate::func::pe_input(pe, base).map_err(|e| anyhow!("placing the image at {base:#x}: {e}"))?;
    let pv = provided(pe, base);
    Ok(cache_root()?.join(unit_key(&image, &pv)))
}

/// Build every export provider for a PE image (`gex_cgen::provider`) into the
/// unit cache, as mapping it would; their names.
pub fn prepare_providers(pe: &gex_ld::pe::PeImage) -> Result<Vec<String>> {
    let mut v = Vec::new();
    for prov in gex_cgen::provider::for_image(&pe.exports.dll_name, !pe.is64, &pe.data) {
        gex_cgen::provider::build(&prov, &cache_root()?)?;
        v.push(prov.name.to_string());
    }
    Ok(v)
}

/// [`prepare_pe`] for any recovery input (no providers).
pub fn prepare_image(image: &gex_ld::Image, seeds: &gex_xlate::func::PeSeeds, jobs: usize, verbose: bool) -> Result<std::path::PathBuf> {
    let none = Provided { providers: Vec::new(), exports: HashMap::new(), stubs: Vec::new() };
    prepare_with(image, seeds, jobs, verbose, &none)
}

fn prepare_with(image: &gex_ld::Image, seeds: &gex_xlate::func::PeSeeds, jobs: usize, verbose: bool, pv: &Provided) -> Result<std::path::PathBuf> {
    let (image, seeds) = (image, seeds);
    if verbose {
        report_hints(image);
    }
    let key = unit_key(image, pv);
    let root = cache_root()?;
    let dir = root.join(&key);
    let out = dir.join("c.wasm");
    if out.exists() {
        return Ok(out);
    }
    if mode() == Mode::Aot {
        bail!("no function-tier unit {key} in {} (the ejected project was not built for this image; run ./build.sh)", root.display());
    }
    let recovery = root.join("recovery").join(recovery_key(image));
    if !recovery.join("summaries").exists() {
        let t0 = std::time::Instant::now();
        let prog = gex_xlate::func::recover_pe(image, seeds, &Default::default());
        let summaries = gex_xlate::summary::analyze(&prog).map;
        let n = prog.functions.len();
        let funcs = gex_cgen::select(
            prog.functions.into_values().collect(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
        )?;
        gex_cgen::image::write_recovery(&recovery, &funcs, &summaries, image.class32)?;
        if verbose {
            eprintln!(
                "[cgen] recovered {n} functions ({} kept) in {:.1}s",
                funcs.len(),
                t0.elapsed().as_secs_f64()
            );
        }
    }
    let t0 = std::time::Instant::now();
    let work = dir.join("work");
    let byte = |a: u64| {
        image.segments.iter().find_map(|s| s.byte(a))
    };
    let stubs: std::collections::HashSet<u64> = pv.stubs.iter().copied().collect();
    let hints = icall_hints(image);
    let stats = gex_cgen::image::build_c_module(&recovery, &work, &root.join("objects"), &out, jobs, &byte, &stubs, &hints)?;
    let _ = std::fs::remove_dir_all(&work);
    if verbose {
        eprintln!(
            "[cgen] {} functions, {} units ({} compiled, {} cached), {} MiB of C, {} MiB wasm, {:.1}s; {} devirtualised calls ({} hints), {} calls to leaf copies",
            stats.functions,
            stats.units,
            stats.compiled,
            stats.cached,
            stats.c_bytes >> 20,
            stats.wasm_bytes >> 20,
            t0.elapsed().as_secs_f64(),
            stats.devirt,
            hints.len(),
            stats.leaf_copies
        );
    }
    Ok(out)
}

/// Turn the function tier on for this process's images (`--cgen`).
/// `record`: a profile is being recorded (`--record`), which names each
/// image as it maps (`profile::note_pe`), on every tier.
pub(crate) fn enable(record: bool) -> Result<()> {
    // Export providers bind on every tier, units or not.
    if mode() == Mode::Off && !gex_cgen::provider::enabled() && !record {
        return Ok(());
    }
    if ON.set(()).is_err() {
        return Ok(());
    }
    gex_kernel::nt::section::set_image_mapped_hook(|m| {
        PENDING.lock().unwrap_or_else(|e| e.into_inner()).push(Pending {
            path: m.path.to_string(),
            base: m.base,
            pe: Arc::clone(m.pe),
        });
        HAS_PENDING.store(true, std::sync::atomic::Ordering::Release);
    });
    Ok(())
}

fn take_pending() -> Vec<Pending> {
    // A load first: every syscall of every thread comes here, and a swap
    // would write the shared line each time.
    if !HAS_PENDING.load(std::sync::atomic::Ordering::Relaxed) || !HAS_PENDING.swap(false, std::sync::atomic::Ordering::Acquire) {
        return Vec::new();
    }
    std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner()))
}

/// A unit found for an image, checked against what is mapped.
struct Placed {
    path: std::path::PathBuf,
    cwasm: Vec<u8>,
    entries: Vec<u64>,
    /// A hash of each executable page as the unit was built from it.
    pages: Vec<(u64, u64)>,
}

/// A unit for `p`, if one was built, with the byte check.
fn place(p: &Pending, memory: &[u8]) -> Result<Option<Placed>> {
    let (image, _) = gex_xlate::func::pe_input(&p.pe, p.base)
        .map_err(|e| anyhow!("{}: placing the image at {:#x}: {e}", p.path, p.base))?;
    let pv = provided(&p.pe, p.base);
    let cwasm_path = cache_root()?.join(unit_key(&image, &pv)).join("c.wasm");
    let Ok(cwasm) = std::fs::read(&cwasm_path) else {
        return Ok(None);
    };
    let mut pages = Vec::new();
    // What the unit was built from must be what is mapped.
    for seg in image.segments.iter().filter(|s| s.executable) {
        let (lo, n) = (seg.vaddr as usize, seg.bytes.len());
        let live = memory
            .get(lo..lo + n)
            .ok_or_else(|| anyhow!("{}: {:#x}+{n:#x} is outside guest memory", p.path, seg.vaddr))?;
        if let Some(i) = (0..n).find(|&i| live[i] != seg.bytes[i]) {
            bail!(
                "{}: the mapped code at {:#x} differs from the image its function-tier unit was \
                 built from (guest {:#04x}, unit {:#04x}); refusing the unit",
                p.path,
                seg.vaddr + i as u64,
                live[i],
                seg.bytes[i]
            );
        }
        let first = seg.vaddr / gex_abi::DISPATCH_PAGE;
        let last = (seg.vaddr + n as u64 - 1) / gex_abi::DISPATCH_PAGE;
        for page in first..=last {
            pages.push((page, page_hash(memory, page)));
        }
    }
    let mut entries = gex_cgen::image::read_entries(&cwasm_path)?;
    // A provider's addresses are its stubs: the provider is bound there.
    for a in entries.iter_mut() {
        if pv.stubs.binary_search(a).is_ok() {
            *a = 0;
        }
    }
    if !entries.iter().any(|&a| a != 0) {
        bail!("{}: its function-tier unit exports no entries", p.path);
    }
    Ok(Some(Placed { path: cwasm_path, cwasm, entries, pages }))
}

/// A hash of guest page `page` as it is now.
fn page_hash(memory: &[u8], page: u64) -> u64 {
    let lo = (page * gex_abi::DISPATCH_PAGE) as usize;
    let hi = (lo + gex_abi::DISPATCH_PAGE as usize).min(memory.len());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in memory.get(lo..hi).unwrap_or(&[]).chunks(8) {
        let mut b = [0u8; 8];
        b[..c.len()].copy_from_slice(c);
        h = (h ^ u64::from_le_bytes(b)).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The pages installed units cover, with their hash as built.
static UNIT_PAGES: Mutex<Option<HashMap<u64, u64>>> = Mutex::new(None);

/// Pages that have retired (`retire_translations`): any an installed unit
/// covers whose bytes are no longer the ones it was built from gets its bit
/// in `gex_abi::PATCHED_PAGES`, and compiled code stops calling into it.
pub(crate) fn on_retire(memory: &gex_mem::GuestMemory, ranges: &[(u64, u64)]) {
    let memory = memory.bytes();
    let guard = UNIT_PAGES.lock().unwrap_or_else(|e| e.into_inner());
    let Some(pages) = guard.as_ref() else { return };
    for &(start, end) in ranges {
        for page in start / gex_abi::DISPATCH_PAGE..end.div_ceil(gex_abi::DISPATCH_PAGE) {
            let Some(&built) = pages.get(&page) else { continue };
            if page_hash(memory, page) != built {
                let off = (gex_abi::PATCHED_PAGES + page / 8) as usize;
                if let Some(b) = memory.get_mut(off) {
                    *b |= 1 << (page % 8);
                }
                if trace() {
                    eprintln!("gex-nt: page {:#x} of a compiled image was rewritten: its functions leave for tier-1", page * gex_abi::DISPATCH_PAGE);
                }
            }
        }
    }
}

/// Provided exports by guest address: a label (`kernel32.dll!OutputDebugStringA`),
/// the first bytes of the library's own code there when the provider was
/// bound, and whether a rewrite of them was reported.
static PROVIDED: Mutex<Option<std::collections::BTreeMap<u64, (String, [u8; PROVIDED_BYTES], bool)>>> = Mutex::new(None);
const PROVIDED_BYTES: usize = 8;

fn provided_bytes(memory: &[u8], addr: u64) -> [u8; PROVIDED_BYTES] {
    let mut b = [0; PROVIDED_BYTES];
    if let Some(m) = memory.get(addr as usize..addr as usize + PROVIDED_BYTES) {
        b.copy_from_slice(m);
    }
    b
}

fn note_provided(memory: &gex_mem::GuestMemory, addr: u64, label: String) {
    let bytes = provided_bytes(memory.bytes(), addr);
    PROVIDED.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(addr, (label, bytes, false));
}

/// Report, once per export, a guest that rewrote the start of an export a
/// provider serves (a Detours-style hook, a patch). The provider stays
/// bound (`ftab::Shared::providers`), so the guest's new code there does
/// not run; on Windows it would. `GEX_PROVIDER_SKIP=<export>` leaves the
/// export to the library's code, hooks included.
pub(crate) fn provided_rewrites(memory: &gex_mem::GuestMemory, ranges: &[(u64, u64)]) {
    let memory = memory.bytes();
    let mut guard = PROVIDED.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = guard.as_mut() else { return };
    for &(start, end) in ranges {
        for (&addr, (label, bytes, told)) in map.range_mut(start.saturating_sub(PROVIDED_BYTES as u64 - 1)..end) {
            if !*told && provided_bytes(memory, addr) != *bytes {
                *told = true;
                let e = label.rsplit('!').next().unwrap_or(label);
                eprintln!(
                    "gex-nt: the guest rewrote provided export {label} at {addr:#x} (a hook?); the provider stays bound, so its new code does not run (GEX_PROVIDER_SKIP={e} runs the library's own code)"
                );
            }
        }
    }
}

/// Record a placed unit's pages as built, and clear their patched bits.
fn note_pages(memory: &gex_mem::GuestMemory, pages: &[(u64, u64)]) {
    let memory = memory.bytes();
    let mut guard = UNIT_PAGES.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(Default::default);
    for &(page, h) in pages {
        map.insert(page, h);
        let off = (gex_abi::PATCHED_PAGES + page / 8) as usize;
        if let Some(b) = memory.get_mut(off) {
            *b &= !(1 << (page % 8));
        }
    }
}

/// The NT layer's trampoline page (`gex_kernel::nt::boot::trampoline_input`:
/// the unix-call and syscall dispatchers every Wine DLL calls through, Wine's
/// opengl32 once per GL call) as a recovery input. Fixed bytes at a fixed
/// address, so its unit is built once per gex build and installed at boot
/// like an image's: without it every such call leaves the function tier.
fn trampoline_input(class32: bool) -> (gex_ld::Image, gex_xlate::func::PeSeeds) {
    let (image, starts) = gex_kernel::nt::boot::trampoline_input(class32);
    let seeds = gex_xlate::func::PeSeeds {
        starts: starts.into_iter().map(|a| (a, gex_xlate::func::Source::Entry)).collect(),
        fdes: Vec::new(),
    };
    (image, seeds)
}

/// The NT trampoline page's unit, built into the cache (`gexc cgen-image
/// --nt-trampolines`): for a host that cannot build it at boot, the browser.
pub fn prepare_trampolines(class32: bool, jobs: usize) -> Result<std::path::PathBuf> {
    let (image, seeds) = trampoline_input(class32);
    prepare_image(&image, &seeds, jobs, true)
}

static TRAMPOLINES_DONE: OnceLock<()> = OnceLock::new();

/// Build (once per gex build: cached) and install the trampoline page's
/// unit, at boot.
fn install_trampolines(store: &mut Store<HostState>, class32: bool) -> Result<()> {
    if TRAMPOLINES_DONE.set(()).is_err() {
        return Ok(());
    }
    let (image, seeds) = trampoline_input(class32);
    let t0 = std::time::Instant::now();
    let path = {
        let _lock = build_lock()?;
        prepare_image(&image, &seeds, 2, false)?
    };
    let cwasm = std::fs::read(&path)?;
    let memory = store.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    for seg in &image.segments {
        let (lo, n) = (seg.vaddr as usize, seg.bytes.len());
        if memory.bytes().get(lo..lo + n) != Some(&seg.bytes[..]) {
            bail!("the NT trampoline page in guest memory is not the one its unit was built from");
        }
    }
    let entries = gex_cgen::image::read_entries(&path)?;
    let (shared, engine, cache) = {
        let jit = store.data().jit.as_ref().ok_or_else(|| anyhow!("runtime translation is not set up"))?;
        (Arc::clone(&jit.shared), jit.engine.clone(), jit.cache.clone())
    };
    let module = compile(&engine, cache.as_ref(), &path, &cwasm)?;
    crate::ftab::publish(&shared, &memory, module, &entries, gex_kernel::ftab::Tier::Unit, Vec::new())?;
    crate::ftab::catch_up(&mut *store)?;
    catch_up_table(store)?;
    if trace() {
        eprintln!("gex-nt: function tier for the NT trampolines: {} entries, {:.1}s", entries.len(), t0.elapsed().as_secs_f64());
    }
    Ok(())
}

/// Compile a unit's module. The compiled form is kept beside the unit
/// (`c.cwasm`), not in the shared module cache: a large image's is hundreds
/// of MiB, which that cache's LRU budget evicts at once, and Cranelift over
/// noita.exe's unit takes about a minute. Not keyed on the build of gex: the
/// unit directory is keyed on the module's content already, and wasmtime
/// refuses an artifact from an incompatible engine, which recompiles it.
fn compile(engine: &Engine, cache: Option<&(std::path::PathBuf, String)>, cwasm_path: &std::path::Path, cwasm: &[u8]) -> Result<Module> {
    if cache.is_none() {
        return Ok(Module::new(engine, cwasm)?);
    }
    gex_cache::load_or_compile_at(engine, &cwasm_path.with_extension("cwasm"), cwasm)
}

/// Build a unit for `p` in a background child for the next run: `gexc
/// cgen-image`, low priority, under a memory cap when `systemd-run` is there
/// (`GEX_CGEN_MEMORY_MAX`, default 12G). Builders take [`build_lock`], so
/// they run one at a time, and outlive this process.
fn spawn_build(p: &Pending) -> Result<std::process::Child> {
    // A process group of its own: the run ending -- a Ctrl-C at the terminal,
    // `timeout` signalling its group -- must not take the build with it.
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let max = std::env::var("GEX_CGEN_MEMORY_MAX").unwrap_or_else(|_| "12G".into());
    let base = format!("{:#x}", p.base);
    let args = ["cgen-image", p.path.as_str(), "--pe-base", base.as_str()];
    let capped = std::process::Command::new("systemd-run")
        .args(["--user", "--scope", "-q", "-p", &format!("MemoryMax={max}")])
        .arg("nice")
        .arg("-n")
        .arg("10")
        .arg(&exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .process_group(0)
        .spawn();
    match capped {
        Ok(c) => Ok(c),
        Err(_) => Ok(std::process::Command::new(&exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .process_group(0)
            .spawn()?),
    }
}

/// `GEX_NT_CGEN_SYNC=1` (tests/nt under --cgen): a program image mapped
/// without a unit has one built there and then, and installed, rather than
/// running on tier-1 or failing.
fn sync_build() -> bool {
    std::env::var_os("GEX_NT_CGEN_SYNC").is_some()
}

/// Build `p`'s unit now (the child `gexc cgen-image`), waiting for it.
fn build_now(p: &Pending) -> Result<()> {
    let status = spawn_build(p)?.wait()?;
    if !status.success() {
        bail!("building the function tier for {} failed ({status})", p.path);
    }
    Ok(())
}

/// A unit for `p`: placed if built; built first under `GEX_NT_CGEN_SYNC`.
fn place_or_build(p: &Pending, memory: &[u8]) -> Result<Option<Placed>> {
    if let Some(pl) = place(p, memory)? {
        return Ok(Some(pl));
    }
    if sync_build() && !is_wine(p) && mode() != Mode::Aot {
        build_now(p)?;
        return place(p, memory);
    }
    Ok(None)
}

/// An image Wine's prefix provides, not the program.
fn is_wine(p: &Pending) -> bool {
    p.path.to_ascii_lowercase().contains("/drive_c/windows/")
}

/// An image mapped with no unit for its base. `--cgen` refuses a program
/// image; otherwise it runs on tier-1 and, unless `GEX_NT_CGEN_BUILD` says
/// no (`0`, or a list of file names that leaves it out), gets a unit built
/// for the next run.
fn missing(p: Pending) -> Result<()> {
    if mode() == Mode::Aot {
        bail!(
            "gex-eject: {} was mapped at {:#x}, and the ejected project has no code built for it there \
             (the recorded session never mapped it, or mapped it elsewhere). Record a session that loads it \
             and re-eject",
            p.path,
            p.base
        );
    }
    // Always said, and kept for GEX_NT_STATS: an image on tier-1 because its
    // unit is missing (a base its unit was not built for, a stale key) is
    // otherwise only visible as a lower AOT share.
    let key = gex_xlate::func::pe_input(&p.pe, p.base)
        .map(|(image, _)| unit_key(&image, &provided(&p.pe, p.base)))
        .unwrap_or_else(|e| format!("(not placeable: {e})"));
    eprintln!("gex-nt: warning: {} at {:#x}: no function-tier unit (key {key}); it runs on tier-1", p.path, p.base);
    NO_UNIT.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{} at {:#x} (key {key})", p.path, p.base));
    if mode() == Mode::Require && !is_wine(&p) {
        bail!(
            "--cgen: {} has no function-tier unit for base {:#x}; build one with \
             `gexc cgen-image {} --pe-base {:#x}`, or run once without --cgen (it is then built \
             for the next run)",
            p.path,
            p.base,
            p.path,
            p.base
        );
    }
    // Wine's DLLs get theirs at staging, at their layout bases (`gexc
    // cgen-prefix`); one placed anywhere else is not built for.
    let build = !is_wine(&p)
        && match std::env::var("GEX_NT_CGEN_BUILD").as_deref() {
        Err(_) | Ok("1") => true,
        Ok("0") => false,
        Ok(list) => {
            let name = p.path.rsplit(['/', '\\']).next().unwrap_or(&p.path);
            list.split(',').any(|n| n.trim().eq_ignore_ascii_case(name))
        }
        };
    if !build {
        if trace() {
            eprintln!("gex-nt: {} at {:#x}: no function-tier unit, tier-1", p.path, p.base);
        }
        return Ok(());
    }
    eprintln!("gex-nt: no function-tier unit for {} at {:#x}: tier-1 this run, building it for the next", p.path, p.base);
    match spawn_build(&p) {
        // Not waited for: it outlives the run, and the next one finds it.
        Ok(child) => drop(child),
        Err(e) => eprintln!("gex-nt: could not start building the function tier for {}: {e}", p.path),
    }
    Ok(())
}

/// Images mapped with no unit for their base, as `missing` reported them.
static NO_UNIT: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// GEX_NT_STATS's "no unit" line: every image running on tier-1 for want of
/// a unit, or `None` when there are none.
pub(crate) fn no_unit_line() -> Option<String> {
    let v = NO_UNIT.lock().unwrap_or_else(|e| e.into_inner());
    (!v.is_empty()).then(|| format!("gex-nt: no unit ({}): {}", v.len(), v.join(" | ")))
}

/// Place, compile, publish and install, for a caller inside the guest
/// (after an NT syscall that mapped an image).
pub(crate) fn drain_in_caller(caller: &mut Caller<'_, HostState>) -> Result<()> {
    if ON.get().is_none() {
        return Ok(());
    }
    let pending = take_pending();
    if pending.is_empty() {
        return Ok(());
    }
    let _one = PLACING.lock().unwrap_or_else(|e| e.into_inner());
    for p in pending {
        crate::profile::note_pe(&p.path, p.base, &p.pe);
        let (shared, engine, cache) = {
            let jit = caller.data().jit.as_ref().ok_or_else(|| anyhow!("runtime translation is not set up"))?;
            (Arc::clone(&jit.shared), jit.engine.clone(), jit.cache.clone())
        };
        let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        let t0 = std::time::Instant::now();
        if mode() != Mode::Off {
            match place_or_build(&p, memory.bytes())? {
                Some(pl) => {
                    let module = compile(&engine, cache.as_ref(), &pl.path, &pl.cwasm).with_context(|| format!("compiling {}'s function tier", p.path))?;
                    note_pages(&memory, &pl.pages);
                    crate::ftab::publish(&shared, &memory, module, &pl.entries, gex_kernel::ftab::Tier::Unit, Vec::new())?;
                    report(&p, &pl, t0);
                }
                None => missing(p.clone())?,
            }
        }
        install_providers(&p, caller.data_mut(), &shared, &engine, &memory)?;
        crate::ftab::catch_up(&mut *caller)?;
        replay_registry(caller)?;
    }
    Ok(())
}

/// The same, at boot (the exe and ntdll, mapped before any guest code runs).
pub(crate) fn drain_in_store(store: &mut Store<HostState>) -> Result<()> {
    let pending = take_pending();
    if pending.is_empty() {
        return Ok(());
    }
    // The trampolines with the first images (the exe says the ABI).
    if let Some(p) = pending.first().filter(|_| mode() != Mode::Off) {
        install_trampolines(store, !p.pe.is64)?;
    }
    let _one = PLACING.lock().unwrap_or_else(|e| e.into_inner());
    for p in pending {
        crate::profile::note_pe(&p.path, p.base, &p.pe);
        let (shared, engine, cache) = {
            let jit = store.data().jit.as_ref().ok_or_else(|| anyhow!("runtime translation is not set up"))?;
            (Arc::clone(&jit.shared), jit.engine.clone(), jit.cache.clone())
        };
        let memory = store.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        let t0 = std::time::Instant::now();
        if mode() != Mode::Off {
            match place_or_build(&p, memory.bytes())? {
                Some(pl) => {
                    let module = compile(&engine, cache.as_ref(), &pl.path, &pl.cwasm).with_context(|| format!("compiling {}'s function tier", p.path))?;
                    note_pages(&memory, &pl.pages);
                    crate::ftab::publish(&shared, &memory, module, &pl.entries, gex_kernel::ftab::Tier::Unit, Vec::new())?;
                    report(&p, &pl, t0);
                }
                None => missing(p.clone())?,
            }
        }
        install_providers(&p, store.data_mut(), &shared, &engine, &memory)?;
        crate::ftab::catch_up(&mut *store)?;
        catch_up_table(store)?;
    }
    Ok(())
}

/// Bind `p`'s export providers (`gex_cgen::provider`) at their exports'
/// addresses, after its unit (whose bodies there are stubs into the table).
fn install_providers(p: &Pending, state: &mut HostState, shared: &Arc<Mutex<JitShared>>, engine: &Engine, memory: &gex_mem::GuestMemory) -> Result<()> {
    let pv = provided(&p.pe, p.base);
    for prov in &pv.providers {
        // Small, and built in a private directory then renamed into place:
        // no need for the cache's build lock (which a unit build holds for
        // an hour).
        let (path, slots) = gex_cgen::provider::build(prov, &cache_root()?)?;
        let pairs = gex_cgen::provider::bindings(prov, &slots, &pv.exports, p.base, trace());
        if pairs.is_empty() {
            continue;
        }
        let module = Module::from_file(engine, &path).with_context(|| format!("compiling provider {}", prov.name))?;
        crate::aot_share::note(&module, crate::aot_share::Kind::Provider);
        if prov.stateful {
            // Its data in guest pages, as an ejected project's stateful
            // providers have it (`libprov`); the stack comes per thread.
            let wasm = std::fs::read(&path)?;
            let (size, align) = crate::libprov::dylink_memory(&wasm)?;
            let a = 1u64 << align;
            let data = crate::libprov::alloc_top(state, (size as u64).max(16) + a, &format!("[provider {} data]", prov.name))?;
            let data = (data + a - 1) & !(a - 1);
            crate::ftab::publish_stateful(shared, memory, module, slots.len() as u32, &pairs, crate::libprov::Extras { memory_base: data as u32 })?;
        } else {
            crate::ftab::publish_pairs(shared, memory, module, slots.len() as u32, &pairs, gex_kernel::ftab::Tier::Unit)?;
        }
        let dll = p.path.rsplit(['/', '\\']).next().unwrap_or(&p.path);
        for (_, e) in &prov.binds {
            if let Some(&rva) = pv.exports.get(e) {
                note_provided(memory, p.base + rva as u64, format!("{dll}!{e}"));
            }
        }
        eprintln!("gex-nt: provider {} for {}: {} exports", prov.name, p.path, pairs.len());
    }
    // A native wasm32 Wine DLL (stream W): its module, at its skeleton's
    // exports (`wasmdll`).
    if let Some(n) = crate::wasmdll::install(&p.pe, &p.path, p.base, state, shared, engine, memory)? {
        eprintln!("gex-nt: native wasm32 module for {}: {n} entries", p.path);
    }
    // Stateful providers (`libprov`: an ejected project's), after the
    // built-in ones, so they win where both bind an export.
    let libs = crate::libprov::for_dll(&p.pe.exports.dll_name);
    if !libs.is_empty() {
        let mut exports = HashMap::new();
        for e in p.pe.exports.entries.iter().filter(|e| e.rva != 0 && e.forwarder.is_none()) {
            for n in &e.names {
                exports.insert(n.clone(), e.rva);
            }
        }
        for lp in libs {
            let n = crate::libprov::install(lp, state, shared, engine, memory, &exports, p.base, p.pe.entry_rva)?;
            eprintln!("gex-nt: provider {} for {}: {n} exports", lp.name, p.path);
        }
    }
    Ok(())
}

fn report(p: &Pending, pl: &Placed, t0: std::time::Instant) {
    eprintln!(
        "gex-nt: function tier for {} at {:#x}: {} entries, {} MiB wasm, placed in {:.1}s",
        p.path,
        p.base,
        pl.entries.iter().filter(|&&a| a != 0).count(),
        pl.cwasm.len() >> 20,
        t0.elapsed().as_secs_f64()
    );
}
