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
}

static MODE: OnceLock<Mode> = OnceLock::new();

/// Set the mode before the program starts.
pub fn set_mode(m: Mode) {
    let _ = MODE.set(m);
}

fn mode() -> Mode {
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
struct Pending {
    path: String,
    base: u64,
    pe: Arc<gex_ld::pe::PeImage>,
}

static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
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

/// The identity of an image's recovery: the sources that decide it
/// (`gex_cgen::image::RECOVERY_INPUTS`) and the image as placed
/// (`aot::image_identity`: its relocated segments at its base).
pub fn recovery_key(image: &gex_ld::Image) -> String {
    let mut h = gex_cache::sha256::Sha256::new();
    h.update(b"gex-cgen-recovery 1");
    h.update(gex_cgen::image::RECOVERY_INPUTS.as_bytes());
    h.update(aot::image_identity(image).as_bytes());
    gex_cache::sha256::hex(&h.finish())
}

/// The identity of a unit: its format, the sources that decide the C and the
/// module (`UNIT_INPUTS`), what it embeds from elsewhere
/// (`UNIT_ABI_VERSION`), and the recovery it was built from. A change to
/// tier-1 alone changes none of these. clang's version and flags key each
/// object inside it.
pub fn image_key(image: &gex_ld::Image) -> String {
    let mut h = gex_cache::sha256::Sha256::new();
    h.update(gex_cgen::image::FORMAT.as_bytes());
    h.update(gex_cgen::image::UNIT_INPUTS.as_bytes());
    h.update(&gex_cgen::image::UNIT_ABI_VERSION.to_le_bytes());
    h.update(recovery_key(image).as_bytes());
    gex_cache::sha256::hex(&h.finish())
}

/// Build (or find) the unit's C module for a PE image placed at `base`:
/// recovery once (cached), then C compiled unit by unit. Returns its path.
pub fn prepare_pe(pe: &gex_ld::pe::PeImage, base: u64, jobs: usize, verbose: bool) -> Result<std::path::PathBuf> {
    let (image, seeds) = gex_xlate::func::pe_input(pe, base).map_err(|e| anyhow!("placing the image at {base:#x}: {e}"))?;
    let key = image_key(&image);
    let root = cache_root()?;
    let dir = root.join(&key);
    let out = dir.join("c.wasm");
    if out.exists() {
        return Ok(out);
    }
    let recovery = root.join("recovery").join(recovery_key(&image));
    if !recovery.join("summaries").exists() {
        let t0 = std::time::Instant::now();
        let prog = gex_xlate::func::recover_pe(&image, &seeds, &Default::default());
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
    let stats = gex_cgen::image::build_c_module(&recovery, &work, &root.join("objects"), &out, jobs)?;
    let _ = std::fs::remove_dir_all(&work);
    if verbose {
        eprintln!(
            "[cgen] {} functions, {} units ({} compiled, {} cached), {} MiB of C, {} MiB wasm, {:.1}s",
            stats.functions,
            stats.units,
            stats.compiled,
            stats.cached,
            stats.c_bytes >> 20,
            stats.wasm_bytes >> 20,
            t0.elapsed().as_secs_f64()
        );
    }
    Ok(out)
}

/// Turn the function tier on for this process's images (`--cgen`).
pub(crate) fn enable() -> Result<()> {
    if mode() == Mode::Off {
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
    });
    Ok(())
}

fn take_pending() -> Vec<Pending> {
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
    let cwasm_path = cache_root()?.join(image_key(&image)).join("c.wasm");
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
    let entries = gex_cgen::image::read_entries(&cwasm_path)?;
    if entries.is_empty() {
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

/// Compile a unit's module. The compiled form is kept beside the unit
/// (`c.<build>.cwasm`), not in the shared module cache: a large image's is
/// hundreds of MiB, which that cache's LRU budget evicts at once, and
/// Cranelift over noita.exe's unit takes a minute.
fn compile(engine: &Engine, cache: Option<&(std::path::PathBuf, String)>, cwasm_path: &std::path::Path, cwasm: &[u8]) -> Result<Module> {
    let Some((_, build)) = cache else {
        return Ok(Module::new(engine, cwasm)?);
    };
    let tag = &gex_cache::key(build, [b"gex-cgen-compiled 1".as_slice()])[..16];
    gex_cache::load_or_compile_at(engine, &cwasm_path.with_extension(format!("{tag}.cwasm")), cwasm)
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
    if sync_build() && !is_wine(p) {
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
        let (shared, engine, cache) = {
            let jit = caller.data().jit.as_ref().ok_or_else(|| anyhow!("runtime translation is not set up"))?;
            (Arc::clone(&jit.shared), jit.engine.clone(), jit.cache.clone())
        };
        let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        let t0 = std::time::Instant::now();
        let Some(pl) = place_or_build(&p, memory.bytes())? else {
            missing(p)?;
            continue;
        };
        let module = compile(&engine, cache.as_ref(), &pl.path, &pl.cwasm).with_context(|| format!("compiling {}'s function tier", p.path))?;
        note_pages(&memory, &pl.pages);
        publish_cgen_unit(&shared, module, &pl.entries)?;
        replay_registry(caller)?;
        report(&p, &pl, t0);
    }
    Ok(())
}

/// The same, at boot (the exe and ntdll, mapped before any guest code runs).
pub(crate) fn drain_in_store(store: &mut Store<HostState>) -> Result<()> {
    let pending = take_pending();
    if pending.is_empty() {
        return Ok(());
    }
    let _one = PLACING.lock().unwrap_or_else(|e| e.into_inner());
    for p in pending {
        let (shared, engine, cache) = {
            let jit = store.data().jit.as_ref().ok_or_else(|| anyhow!("runtime translation is not set up"))?;
            (Arc::clone(&jit.shared), jit.engine.clone(), jit.cache.clone())
        };
        let memory = store.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        let t0 = std::time::Instant::now();
        let Some(pl) = place_or_build(&p, memory.bytes())? else {
            missing(p)?;
            continue;
        };
        let module = compile(&engine, cache.as_ref(), &pl.path, &pl.cwasm).with_context(|| format!("compiling {}'s function tier", p.path))?;
        note_pages(&memory, &pl.pages);
        let unit = publish_cgen_unit(&shared, module, &pl.entries)?;
        install_unit(&mut *store, &unit)?;
        store.data_mut().jit.as_mut().unwrap().replayed += 1;
        report(&p, &pl, t0);
    }
    Ok(())
}

fn report(p: &Pending, pl: &Placed, t0: std::time::Instant) {
    eprintln!(
        "gex-nt: function tier for {} at {:#x}: {} entries, {} MiB wasm, placed in {:.1}s",
        p.path,
        p.base,
        pl.entries.len(),
        pl.cwasm.len() >> 20,
        t0.elapsed().as_secs_f64()
    );
}
