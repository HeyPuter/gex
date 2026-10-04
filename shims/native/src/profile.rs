//! The native host's half of profiling (`GEX_PROFILE=1`, `gexc run --record`).
//!
//! Translated code keeps its counters in a second wasm memory, one per store
//! (so one per guest thread); `gex_emit::profile` documents the layout. This
//! file creates those memories, places each module instance's region in
//! them, reads them back ("harvests") when a thread or process ends or a dump
//! is asked for, names every address by `(object hash, offset)`, and writes
//! the profile and the session manifest (`gex_xlate::profile`).
//!
//! One recorder per host process, like the sampler: a process tree is one
//! profile and one session, whatever forks and execs happen inside it.
//!
//! On-demand dumps: set `GEX_PROFILE_EVERY=<seconds>` to rewrite the files
//! periodically, or create `<out>/dump` to ask for one now. Each thread folds
//! its counts in at its next safepoint or syscall; a thread blocked forever in
//! the kernel contributes when it wakes or ends.

use crate::HostState;
use anyhow::{anyhow, Context, Result};
use gex_xlate::profile::{self as gp, FileEntry, ObjectEntry, Placements, Profile, Session};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use wasmtime::{AsContextMut, Memory, MemoryType};

/// The recorder for this run, when recording.
static RECORDER: OnceLock<Arc<Recorder>> = OnceLock::new();

/// Where the first program was read from on the host, as the driver knows
/// it. The guest-visible path the kernel reports for it is derived from
/// argv[0] and may not lead back to the same file.
static EXE_HOST_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Tell the recorder which host file the first program was loaded from.
pub fn set_exe_host_path(path: PathBuf) {
    *EXE_HOST_PATH.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
}

/// Everything folded in so far.
pub struct Recorder {
    out: PathBuf,
    state: Mutex<RecState>,
    /// Bumped to ask every thread to fold its counts in.
    epoch: AtomicU64,
    /// The top-level run has finished and written its files; later harvests
    /// (a child that outlived it) rewrite them.
    finished: AtomicBool,
}

struct RecState {
    profile: Profile,
    session: Session,
    /// Guest path -> identity of the file, read once.
    ids: HashMap<String, Option<FileId>>,
}

#[derive(Clone)]
struct FileId {
    id: String,
    /// The start of the file, enough for its program headers.
    head: Vec<u8>,
}

/// Per process image (between execs): where its objects are, by address.
#[derive(Default)]
pub struct ProcProf {
    placements: Mutex<Placements>,
}

/// Per store: the profile memory, and syscall counts not yet folded in.
pub struct StoreProf {
    mem: Memory,
    seen_epoch: u64,
    proc: Arc<ProcProf>,
    /// `(syscall pc, enclosing function, canonical nr)` -> count.
    syscalls: HashMap<(u64, Option<u64>, u64), u64>,
    /// An open/openat/execve in flight: `(canonical nr, path, flags, dirfd)`.
    pending: Option<(u64, String, u64, u64)>,
}

/// Name syscalls in written profiles and manifests with the kernel's full
/// table, which also names gex's private numbers for x86-only calls.
pub fn use_kernel_syscall_names() {
    gex_xlate::profile::set_syscall_namer(gex_kernel::syscalls::syscall_name);
}

/// Start recording into `out`. Idempotent: the first call wins.
pub fn start(out: PathBuf, opts: &crate::RunOptions) -> Result<()> {
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    gex_xlate::profile::set_enabled(true);
    use_kernel_syscall_names();
    let session = Session {
        isa: gex_abi::ARCH_NAME.to_string(),
        argv: opts.args.clone(),
        env: opts.env.clone(),
        // Where the guest starts, as `mount_filesystems` decides it: a run
        // without `--cwd` starts in the host's working directory, and a
        // replay (gex-eject) must start there too for relative paths to mean
        // the same files.
        cwd: Some(match &opts.cwd {
            Some(d) => d.clone(),
            None if opts.sysroot.is_some() => "/".to_string(),
            None => std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".into()),
        }),
        sysroot: opts.sysroot.as_ref().map(|p| p.to_string_lossy().into_owned()),
        processes: 1,
        threads: 1,
        ..Default::default()
    };
    let rec = Arc::new(Recorder {
        out: out.clone(),
        state: Mutex::new(RecState {
            profile: Profile::new(gex_abi::ARCH_NAME),
            session,
            ids: HashMap::new(),
        }),
        epoch: AtomicU64::new(0),
        finished: AtomicBool::new(false),
    });
    if RECORDER.set(Arc::clone(&rec)).is_err() {
        return Ok(());
    }
    // The dump clock and the trigger file.
    let every = std::env::var("GEX_PROFILE_EVERY")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|s| *s > 0.0);
    std::thread::Builder::new()
        .name("gex-profile".into())
        .spawn(move || {
            let trigger = rec.out.join("dump");
            let mut last = std::time::Instant::now();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                let mut bump = false;
                if trigger.exists() {
                    let _ = std::fs::remove_file(&trigger);
                    bump = true;
                }
                if let Some(s) = every {
                    if last.elapsed().as_secs_f64() >= s {
                        last = std::time::Instant::now();
                        bump = true;
                    }
                }
                if bump {
                    rec.epoch.fetch_add(1, Ordering::AcqRel);
                }
            }
        })
        .context("starting the profile dump thread")?;
    Ok(())
}

/// Whether a recorder is running.
pub fn active() -> bool {
    RECORDER.get().is_some()
}

fn recorder() -> Option<&'static Arc<Recorder>> {
    RECORDER.get()
}

/// A new process image (a fresh run or an `execve`): record the program and
/// where its executable and interpreter sit.
pub fn new_process(
    kernel: &gex_kernel::Kernel,
    program: &gex_ld::Program,
    pid: i32,
    argv: &[String],
    env: &[String],
) -> Option<Arc<ProcProf>> {
    let rec = recorder()?;
    let proc = Arc::new(ProcProf::default());
    let mut st = rec.state.lock().unwrap_or_else(|e| e.into_inner());
    st.session.programs.push(gp::ProgramEntry {
        pid: pid as i64,
        exe: kernel.exe_path.clone(),
        argv: argv.to_vec(),
        env: env.to_vec(),
    });
    let mut images = vec![(kernel.exe_path.clone(), &program.exe, "exe")];
    if let (Some(interp), Some(path)) = (&program.interp, &program.exe.interp) {
        images.push((path.clone(), interp, "interp"));
    }
    let mut pl = proc.placements.lock().unwrap_or_else(|e| e.into_inner());
    for (path, image, role) in images {
        // The file must be the one the image came from: its entry point,
        // biased, is the image's. Otherwise try the driver's host path.
        let matches = |fid: &FileId| {
            elf_entry(&fid.head).map(|e| e.wrapping_add(image.bias)) == Some(image.entry)
        };
        let mut fid = file_id(&mut st, kernel, &path, role).filter(|f| matches(f));
        if fid.is_none() && role == "exe" {
            let host = EXE_HOST_PATH.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(bytes) = host.as_ref().and_then(|h| std::fs::read(h).ok()) {
                let host = host.map(|h| h.to_string_lossy().into_owned());
                fid = Some(register(&mut st, &path, &bytes, role, host)).filter(|f| matches(f));
            }
        }
        let Some(fid) = fid else {
            eprintln!("gex: profile: cannot identify {role} {path}; its code is recorded as anonymous");
            continue;
        };
        st.profile.note_path(&fid.id, &path);
        for seg in image.segments.iter().filter(|s| s.executable) {
            pl.add(seg.vaddr, seg.vaddr + seg.mem_size, &fid.id, image.bias);
        }
    }
    drop(pl);
    Some(proc)
}

/// A forked child runs the same image: it starts from the parent's map.
pub fn fork_process(parent: &Option<Arc<ProcProf>>) -> Option<Arc<ProcProf>> {
    let parent = parent.as_ref()?;
    let rec = recorder()?;
    rec.state.lock().unwrap_or_else(|e| e.into_inner()).session.processes += 1;
    let pl = parent.placements.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Some(Arc::new(ProcProf {
        placements: Mutex::new(pl),
    }))
}

/// A new thread in an existing process.
pub fn new_thread() {
    if let Some(rec) = recorder() {
        rec.state.lock().unwrap_or_else(|e| e.into_inner()).session.threads += 1;
    }
}

/// The identity of a guest file, hashing it the first time it is seen, and
/// recording it in the session's object list.
fn file_id(st: &mut RecState, kernel: &gex_kernel::Kernel, path: &str, role: &str) -> Option<FileId> {
    if let Some(v) = st.ids.get(path) {
        return v.clone();
    }
    let got = kernel.read_file(path).map(|bytes| {
        let host_path = st
            .session
            .sysroot
            .as_ref()
            .map(|r| format!("{}/{}", r.trim_end_matches('/'), path.trim_start_matches('/')));
        register(st, path, &bytes, role, host_path)
    });
    st.ids.insert(path.to_string(), got.clone());
    got
}

/// Hash a file's bytes and add it to the session's objects.
fn register(st: &mut RecState, path: &str, bytes: &[u8], role: &str, host_path: Option<String>) -> FileId {
    let mut h = gex_cache::sha256::Sha256::new();
    h.update(bytes);
    let id = format!("sha256:{}", gex_cache::sha256::hex(&h.finish()));
    st.session.add_object(ObjectEntry {
        id: id.clone(),
        path: path.to_string(),
        host_path,
        size: bytes.len() as u64,
        role: role.to_string(),
    });
    let fid = FileId {
        id,
        head: bytes[..bytes.len().min(1 << 16)].to_vec(),
    };
    st.ids.insert(path.to_string(), Some(fid.clone()));
    fid
}

/// `e_entry` of an ELF file.
fn elf_entry(head: &[u8]) -> Option<u64> {
    if head.get(..4)? != b"\x7fELF" {
        return None;
    }
    if *head.get(4)? == 2 {
        Some(u64::from_le_bytes(head.get(0x18..0x20)?.try_into().ok()?))
    } else {
        Some(u32::from_le_bytes(head.get(0x18..0x1c)?.try_into().ok()?) as u64)
    }
}

/// Name every file-backed executable mapping the kernel has now.
fn note_mappings(st: &mut RecState, kernel: &gex_kernel::Kernel, proc: &ProcProf) {
    let mut pl = proc.placements.lock().unwrap_or_else(|e| e.into_inner());
    for v in kernel.mem.vmas() {
        if v.prot & gex_kernel::mem::prot::EXEC == 0 {
            continue;
        }
        let Some((path, off)) = &v.code else {
            continue;
        };
        let Some(fid) = file_id(st, kernel, path, "library") else {
            continue;
        };
        let Some(bias) = gp::elf_bias(&fid.head, *off, v.start) else {
            continue;
        };
        st.profile.note_path(&fid.id, path);
        pl.add(v.start, v.end, &fid.id, bias);
    }
    for (s, e, id, bias) in PE_PLACED.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        pl.add(*s, *e, id, *bias);
    }
}

/// PE images the NT layer mapped (`note_pe`), as placements `(start, end,
/// object, bias)`: added to the process's at every harvest (an NT run is one
/// process).
static PE_PLACED: Mutex<Vec<(u64, u64, String, u64)>> = Mutex::new(Vec::new());

/// A PE image the NT layer mapped at `base`, from the host file `path`: its
/// code is named by the file and its link-time addresses (the address less
/// `base - preferred base`), as an ELF object's are.
pub fn note_pe(path: &str, base: u64, pe: &gex_ld::pe::PeImage) {
    let Some(rec) = recorder() else {
        return;
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gex: profile: cannot read {path} ({e}); its code is recorded as anonymous");
            return;
        }
    };
    let mut st = rec.state.lock().unwrap_or_else(|e| e.into_inner());
    let fid = register(&mut st, path, &bytes, "library", Some(path.to_string()));
    st.profile.note_path(&fid.id, path);
    let bias = base.wrapping_sub(pe.preferred_base);
    PE_PLACED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((base, base + pe.image_size as u64, fid.id, bias));
}

/// Code was just mapped: name it now, while the mapping is there (a later
/// `dlclose` would leave its counts with no object).
pub fn note_mapped(state: &mut HostState, mapped: &[gex_kernel::CodeMap]) {
    if mapped.is_empty() {
        return;
    }
    let (Some(rec), Some(sp)) = (recorder(), state.prof.as_ref()) else {
        return;
    };
    let proc = Arc::clone(&sp.proc);
    state.with_kernel(|k| {
        let mut st = rec.state.lock().unwrap_or_else(|e| e.into_inner());
        note_mappings(&mut st, k, &proc);
    });
}

/// Before a store instantiates anything: define its profile memory, creating
/// it the first time.
pub fn link(
    linker: &mut wasmtime::Linker<HostState>,
    mut store: impl AsContextMut<Data = HostState>,
    proc: &Option<Arc<ProcProf>>,
) -> Result<()> {
    let Some(rec) = recorder() else {
        return Ok(());
    };
    if store.as_context().data().prof.is_none() {
        let proc = proc
            .clone()
            .ok_or_else(|| anyhow!("profiling: a store with no process profile"))?;
        let first = (gex_emit_regions_start() as u64).div_ceil(65536) + 64;
        let mem = Memory::new(&mut store, MemoryType::new(first as u32, Some(65536)))
            .context("creating the profile memory")?;
        gp::init_thread_memory(mem.data_mut(&mut store));
        let seen = rec.epoch.load(Ordering::Acquire);
        store.as_context_mut().data_mut().prof = Some(StoreProf {
            mem,
            seen_epoch: seen,
            proc,
            syscalls: HashMap::new(),
            pending: None,
        });
    }
    let mem = store.as_context().data().prof.as_ref().unwrap().mem;
    linker.allow_shadowing(true);
    linker.define(&mut store, gex_abi::IMPORT_MODULE, gp_memory_import(), mem)?;
    Ok(())
}

fn gp_memory_import() -> &'static str {
    gp::MEMORY_IMPORT
}

fn gex_emit_regions_start() -> u32 {
    gp::REGIONS_START
}

/// After a store instantiates a module: give the instance its region.
pub fn attach(mut store: impl AsContextMut<Data = HostState>, instance: &wasmtime::Instance) -> Result<()> {
    let Some(mem) = store.as_context().data().prof.as_ref().map(|p| p.mem) else {
        return Ok(());
    };
    let Ok(size) = instance.get_typed_func::<(), i32>(&mut store, gex_xlate::profile::EXPORT_SIZE) else {
        // Built without profiling while this run records: fail loudly, since
        // its counts would silently be missing.
        anyhow::bail!("profiling: a module without profile counters was instantiated");
    };
    let init = instance.get_typed_func::<i32, ()>(&mut store, gex_xlate::profile::EXPORT_INIT)?;
    let size = size.call(&mut store, ())? as u32;
    let (base, need) = gp::next_region(mem.data(&store), size);
    let have = mem.data_size(&store) as u64;
    if need > have {
        let pages = (need - have).div_ceil(65536);
        mem.grow(&mut store, pages)
            .context("growing the profile memory (more than 4 GiB of counters?)")?;
    }
    init.call(&mut store, base as i32)?;
    gp::set_region_end(mem.data_mut(&mut store), base + size);
    Ok(())
}

/// Fold this store's counts into the recorder, zeroing them. Called when a
/// thread or process ends, and for an on-demand dump.
pub fn harvest(mut store: impl AsContextMut<Data = HostState>) -> Result<()> {
    let Some(rec) = recorder() else {
        return Ok(());
    };
    let Some(mem) = store.as_context().data().prof.as_ref().map(|p| p.mem) else {
        return Ok(());
    };
    let raw = gp::parse_thread_memory(mem.data_mut(&mut store), true)?;
    let mut cx = store.as_context_mut();
    let state = cx.data_mut();
    let sp = state.prof.as_mut().unwrap();
    let syscalls = std::mem::take(&mut sp.syscalls);
    let proc = Arc::clone(&sp.proc);
    state.with_kernel(|k| {
        let mut st = rec.state.lock().unwrap_or_else(|e| e.into_inner());
        note_mappings(&mut st, k, &proc);
        let pl = proc.placements.lock().unwrap_or_else(|e| e.into_inner()).clone();
        st.profile.add_raw(&raw, &pl);
        for ((pc, func, nr), n) in syscalls {
            st.profile.add_syscall(pc, func, nr, n, &pl);
        }
    });
    if rec.finished.load(Ordering::Acquire) {
        write(rec)?;
    }
    Ok(())
}

/// At a safepoint or syscall: fold in and rewrite the files if a dump was
/// asked for since this store last did.
pub fn poll(mut store: impl AsContextMut<Data = HostState>) -> Result<()> {
    let Some(rec) = recorder() else {
        return Ok(());
    };
    let now = rec.epoch.load(Ordering::Acquire);
    let behind = store
        .as_context()
        .data()
        .prof
        .as_ref()
        .is_some_and(|p| p.seen_epoch < now);
    if behind {
        store.as_context_mut().data_mut().prof.as_mut().unwrap().seen_epoch = now;
        harvest(&mut store)?;
        write(rec)?;
    }
    Ok(())
}

/// Before a syscall: count it against the function it is in, and remember
/// the path of an open or exec so its result can be recorded.
pub fn syscall_enter(
    mut store: impl AsContextMut<Data = HostState>,
    gm: &gex_kernel::GuestMem<'_>,
    pc: u64,
) {
    let Some(mem) = store.as_context().data().prof.as_ref().map(|p| p.mem) else {
        return;
    };
    let func = gp::current_function(mem.data(&store));
    let mut cx = store.as_context_mut();
    let state = cx.data_mut();
    let ctx_addr = state.thread.ctx_addr;
    let reg = |off: usize| gm.read_u64(ctx_addr + off as u64).unwrap_or(0);
    let raw = reg(gex_abi::ctx::SYSCALL_NR);
    let nr = if gex_abi::X86 {
        gex_kernel::x86::canonical_number(raw)
    } else {
        raw
    };
    let sp = state.prof.as_mut().unwrap();
    *sp.syscalls.entry((pc, func, nr)).or_insert(0) += 1;
    let a = |i: usize| reg(gex_abi::ctx::SYSCALL_ARGS[i]);
    // (path pointer, flags, dirfd) of the calls that name a file.
    const AT_FDCWD: u64 = (-100i64) as u64;
    let named = if gex_abi::X86 {
        match raw {
            2 => Some((a(0), a(1), AT_FDCWD)),            // open
            85 => Some((a(0), 0o1101, AT_FDCWD)),         // creat
            257 => Some((a(1), a(2), a(0))),              // openat
            59 => Some((a(0), 0, AT_FDCWD)),              // execve
            _ => None,
        }
    } else {
        match raw {
            56 => Some((a(1), a(2), a(0))), // openat
            221 => Some((a(0), 0, AT_FDCWD)), // execve
            _ => None,
        }
    };
    sp.pending = named.and_then(|(ptr, flags, dirfd)| Some((nr, read_cstr(gm, ptr)?, flags, dirfd)));
}

/// After a syscall: record the outcome of a pending open.
pub fn syscall_exit(state: &mut HostState, data: &[u8]) {
    let (Some(rec), Some(sp)) = (recorder(), state.prof.as_mut()) else {
        return;
    };
    let Some((nr, path, flags, dirfd)) = sp.pending.take() else {
        return;
    };
    let ctx_addr = state.thread.ctx_addr as usize + gex_abi::ctx::SYSCALL_RET;
    let ret = data
        .get(ctx_addr..ctx_addr + 8)
        .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
        .unwrap_or(0);
    // Relative paths are made absolute against the directory they were
    // relative to, as the kernel saw it.
    let path = if path.starts_with('/') {
        path
    } else {
        const AT_FDCWD: u64 = (-100i64) as u64;
        let dir = state.with_kernel(|k| {
            if dirfd == AT_FDCWD {
                Some(k.vfs.cwd.clone())
            } else {
                k.fds.get(dirfd as i32).ok().map(|f| f.path.clone())
            }
        });
        match dir {
            Some(d) => format!("{}/{path}", d.trim_end_matches('/')),
            None => format!("<fd {}>/{path}", dirfd as i64),
        }
    };
    let mut st = rec.state.lock().unwrap_or_else(|e| e.into_inner());
    let f = st.session.files.entry(path.clone()).or_insert_with(|| FileEntry {
        path,
        ..Default::default()
    });
    const O_ACCMODE: u64 = 3;
    const O_CREAT: u64 = 0o100;
    const O_DIRECTORY: u64 = if gex_abi::X86 { 0o200000 } else { 0o40000 };
    if nr == 221 {
        f.read = true;
    } else {
        match flags & O_ACCMODE {
            0 => f.read = true,
            1 => f.write = true,
            _ => {
                f.read = true;
                f.write = true;
            }
        }
        f.create |= flags & O_CREAT != 0;
        f.directory |= flags & O_DIRECTORY != 0;
    }
    // An execve that succeeds does not return here, so it counts as ok only
    // when it fails; the exec itself is in `programs`.
    if ret < 0 && ret > -4096 {
        f.failed += 1;
        f.errno = Some(-ret);
    } else if nr != 221 {
        f.ok += 1;
    }
}

fn read_cstr(gm: &gex_kernel::GuestMem<'_>, ptr: u64) -> Option<String> {
    let start = ptr as usize;
    let bytes = gm.bytes.get(start..)?;
    let end = bytes.iter().take(4096).position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

/// Write the profile and the manifest.
fn write(rec: &Recorder) -> Result<()> {
    // One writer at a time, snapshot included: every thread folds its
    // counts in after a dump is asked for and may write; two renames of one
    // temporary file raced (the second found it gone, and the run ended on
    // the error: Peggle under GEX_PROFILE_EVERY), and an older snapshot
    // must not land after a newer one.
    static WRITING: Mutex<()> = Mutex::new(());
    let _one = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let (profile, session) = {
        let st = rec.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut session = st.session.clone();
        // The manifest's syscall list is the per-site counts, summed.
        session.syscalls.clear();
        for o in st.profile.objects.values() {
            for ((_, _, nr), n) in &o.syscalls {
                *session.syscalls.entry(*nr).or_insert(0) += n;
            }
        }
        (st.profile.to_json(), session.to_json())
    };
    // Write-then-rename, so a reader never sees half a file.
    for (name, text) in [("profile.json", profile), ("session.json", session)] {
        let path = rec.out.join(name);
        let tmp = rec.out.join(format!(".{name}.tmp"));
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// The top-level run is over: record how it ended and write the files.
pub fn finish(exit: &crate::Exit) -> Result<Option<PathBuf>> {
    let Some(rec) = recorder() else {
        return Ok(None);
    };
    {
        let mut st = rec.state.lock().unwrap_or_else(|e| e.into_inner());
        st.session.exit = Some(match exit {
            crate::Exit::Status(c) => format!("status {c}"),
            crate::Exit::Signal { signal, .. } => format!("signal {signal}"),
        });
    }
    write(rec)?;
    rec.finished.store(true, Ordering::Release);
    Ok(Some(rec.out.clone()))
}

#[cfg(test)]
mod tests {
    use gex_ld::elf::{Image, Segment};

    /// Two calls to a leaf, then `exit_group(0)`: the leaf is entered twice,
    /// both times by a call, and both returns go where their call said.
    #[test]
    fn records_calls_returns_and_syscalls() {
        // An AArch64/x86-64 fixture (64-bit little-endian ELF or A64 code).
        if gex_abi::PPC {
            return;
        }
        let base = 0x40_0000u64;
        let (bytes, f_off): (Vec<u8>, u64) = if gex_abi::X86 {
            (
                vec![
                    0xe8, 14, 0, 0, 0, // call f
                    0xe8, 9, 0, 0, 0, // call f
                    0xb8, 231, 0, 0, 0, // mov eax, 231 (exit_group)
                    0x31, 0xff, // xor edi, edi
                    0x0f, 0x05, // syscall
                    0xc3, // f: ret
                ],
                19,
            )
        } else {
            let code: [u32; 6] = [
                0xd2800000, // movz x0, #0
                0x94000004, // bl f
                0x94000003, // bl f
                0xd2800bc8, // movz x8, #94
                0xd4000001, // svc #0
                0xd65f03c0, // f: ret
            ];
            (code.iter().flat_map(|w| w.to_le_bytes()).collect(), 20)
        };
        let len = bytes.len() as u64;
        let image = Image {
            soname: None,
            needed: Vec::new(),
            runpath: Vec::new(),
            bias: 0,
            entry: base,
            code: vec![(base, base + len)],
            segments: vec![Segment {
                vaddr: base,
                bytes,
                mem_size: len,
                readable: true,
                writable: false,
                executable: true,
            }],
            phdr_vaddr: base,
            phentsize: 56,
            phnum: 1,
            brk: base + 0x1000,
            text_range: (base, base + len),
            tls: None,
            class32: false,
            symbols: Vec::new(),
            named_relocs: Vec::new(),
            relocs: Vec::new(),
            init_array: Vec::new(),
            dynsyms: Vec::new(),
            irelative: Vec::new(),
            pie: false,
            interp: None,
        };
        let dir = std::env::temp_dir().join(format!("gex-profile-test-{}", std::process::id()));
        let opts = crate::RunOptions {
            args: vec!["/nonexistent/prog".into()],
            record: Some(dir.clone()),
            jit: true,
            cache: false,
            ..Default::default()
        };
        let (exit, _) = crate::run_image(&image, &opts, &gex_xlate::Options::default()).unwrap();
        assert_eq!(exit, crate::Exit::Status(0));
        let out = super::finish(&exit).unwrap().unwrap();
        let text = std::fs::read_to_string(out.join("profile.json")).unwrap();
        let p = gex_xlate::profile::Profile::from_json(&text).unwrap();
        // No file behind the image, so its code is anonymous, by address.
        let o = &p.objects[gex_xlate::profile::ANON];
        let f = &o.blocks[&(base + f_off)];
        assert_eq!((f.n, f.call), (2, 2), "{text}");
        let r = &o.returns[&(base + f_off)];
        assert_eq!((r.expected, r.unexpected), (2, 0), "{text}");
        let sys: u64 = o.syscalls.iter().filter(|((_, _, nr), _)| *nr == 94).map(|(_, n)| n).sum();
        assert_eq!(sys, 1, "{text}");
        let session = std::fs::read_to_string(out.join("session.json")).unwrap();
        let s = gex_xlate::profile::Session::from_json(&session).unwrap();
        assert_eq!(s.syscalls.get(&94), Some(&1));
        assert_eq!(s.exit.as_deref(), Some("status 0"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
