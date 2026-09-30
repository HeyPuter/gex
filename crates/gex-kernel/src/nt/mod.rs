//! The NT personality: Windows programs running Wine 11.15's PE DLLs.
//!
//! `docs/nt-layer.md` is the design. In short: Wine's own `ntdll.dll`,
//! `kernelbase.dll`, `kernel32.dll` and the rest run unchanged in guest memory,
//! and everything on the other side of Wine's syscall boundary (unix
//! `ntdll.so`, `win32u.so`, `wineserver`) is this module.
//!
//! Entry: the host's `syscall` import calls [`syscall`] for a process that has
//! an [`Process`], *without* holding the Linux kernel's lock. Handlers take
//! that lock only for the VFS and the memory map, and keep the NT-side state
//! behind the process's own locks, so the hot calls (the clock, the futex
//! pair) touch no lock that doesn't concern them.
//!
//! Every syscall this layer does not implement is reported on stderr with its
//! name each time it is called, and returns `STATUS_NOT_IMPLEMENTED`
//! (`GEX_NT_STRICT=1` ends the process instead). A call that is implemented
//! for some arguments and not others reports the unsupported case the same
//! way.

pub mod boot;
pub mod file;
pub mod layout;
pub mod object;
pub mod reg;
pub mod section;
pub mod status;
pub mod sync;
pub mod sysinfo;
pub mod sysno;
pub mod thread;
pub mod unixcall;
pub mod user;
pub mod vm;

use crate::futex::FutexTable;
use crate::{GuestMem, Kernel};
use gex_abi::ctx;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

pub use status as st;

/// Which pointer size the process's Windows code uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abi {
    /// PE32+: x86-64.
    P64,
    /// PE32: i386.
    P32,
}

impl Abi {
    pub fn ptr_size(self) -> u64 {
        match self {
            Abi::P64 => 8,
            Abi::P32 => 4,
        }
    }
}

/// A field offset from the generated layouts, for the process's ABI.
#[macro_export]
macro_rules! nt_off {
    ($abi:expr, $($p:ident)::+) => {
        match $abi {
            $crate::nt::Abi::P64 => $crate::nt::layout::x64::$($p)::+,
            $crate::nt::Abi::P32 => $crate::nt::layout::x86::$($p)::+,
        }
    };
}
pub use nt_off as off;

/// An `NTSTATUS` failure. Success codes travel in `Ok`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status(pub u32);

/// A handler's result: `Ok(status)` for a success or informational status
/// (`STATUS_SUCCESS`, `STATUS_IMAGE_NOT_AT_BASE`, `STATUS_TIMEOUT`, …),
/// `Err` for a failure.
pub type R<T = u32> = Result<T, Status>;

impl From<crate::Errno> for Status {
    fn from(e: crate::Errno) -> Status {
        Status(errno_status(e))
    }
}

/// The status Wine's unix side reports for a failed Unix call.
pub fn errno_status(e: crate::Errno) -> u32 {
    use crate::Errno as E;
    match e {
        E::EFAULT => st::ACCESS_VIOLATION,
        E::ENOENT => st::OBJECT_NAME_NOT_FOUND,
        E::ENOTDIR => st::OBJECT_PATH_NOT_FOUND,
        E::EISDIR => st::FILE_IS_A_DIRECTORY,
        E::EEXIST => st::OBJECT_NAME_COLLISION,
        E::EACCES | E::EPERM => st::ACCESS_DENIED,
        E::ENOMEM => st::NO_MEMORY,
        E::EINVAL => st::INVALID_PARAMETER,
        E::EBADF => st::INVALID_HANDLE,
        E::ENOSPC => 0xc000_007f, // STATUS_DISK_FULL
        E::ENOTEMPTY => st::DIRECTORY_NOT_EMPTY,
        E::EROFS => st::MEDIA_WRITE_PROTECTED,
        E::ENOSYS => st::NOT_IMPLEMENTED,
        E::ENAMETOOLONG => st::OBJECT_NAME_INVALID,
        E::EPIPE => st::PIPE_BROKEN,
        _ => st::UNSUCCESSFUL,
    }
}

/// Guest memory, read with the process's pointer size.
pub struct G<'a> {
    pub m: GuestMem<'a>,
    pub abi: Abi,
}

fn fault(_: crate::Errno) -> Status {
    Status(st::ACCESS_VIOLATION)
}

impl<'a> G<'a> {
    pub fn new(bytes: &'a mut [u8], abi: Abi) -> G<'a> {
        G { m: GuestMem::new(bytes), abi }
    }
    pub fn p64(&self) -> bool {
        self.abi == Abi::P64
    }
    pub fn psize(&self) -> u64 {
        self.abi.ptr_size()
    }
    pub fn u8(&self, a: u64) -> R<u8> {
        Ok(self.m.read(a, 1).map_err(fault)?[0])
    }
    pub fn u16(&self, a: u64) -> R<u16> {
        let b = self.m.read(a, 2).map_err(fault)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    pub fn u32(&self, a: u64) -> R<u32> {
        self.m.read_u32(a).map_err(fault)
    }
    pub fn u64(&self, a: u64) -> R<u64> {
        self.m.read_u64(a).map_err(fault)
    }
    pub fn w8(&mut self, a: u64, v: u8) -> R<()> {
        self.m.write(a, &[v]).map_err(fault)
    }
    pub fn w16(&mut self, a: u64, v: u16) -> R<()> {
        self.m.write(a, &v.to_le_bytes()).map_err(fault)
    }
    pub fn w32(&mut self, a: u64, v: u32) -> R<()> {
        self.m.write_u32(a, v).map_err(fault)
    }
    pub fn w64(&mut self, a: u64, v: u64) -> R<()> {
        self.m.write_u64(a, v).map_err(fault)
    }
    /// A pointer-sized value.
    pub fn ptr(&self, a: u64) -> R<u64> {
        match self.abi {
            Abi::P64 => self.u64(a),
            Abi::P32 => self.u32(a).map(u64::from),
        }
    }
    pub fn wptr(&mut self, a: u64, v: u64) -> R<()> {
        match self.abi {
            Abi::P64 => self.w64(a, v),
            Abi::P32 => self.w32(a, v as u32),
        }
    }
    /// Write through an optional out-pointer (NULL means "not wanted").
    pub fn out_ptr(&mut self, a: u64, v: u64) -> R<()> {
        if a != 0 {
            self.wptr(a, v)?;
        }
        Ok(())
    }
    pub fn out32(&mut self, a: u64, v: u32) -> R<()> {
        if a != 0 {
            self.w32(a, v)?;
        }
        Ok(())
    }
    pub fn bytes(&self, a: u64, n: usize) -> R<Vec<u8>> {
        Ok(self.m.read(a, n).map_err(fault)?.to_vec())
    }
    pub fn write(&mut self, a: u64, b: &[u8]) -> R<()> {
        self.m.write(a, b).map_err(fault)
    }
    pub fn zero(&mut self, a: u64, n: usize) -> R<()> {
        self.m.zero(a, n).map_err(fault)
    }
    /// UTF-16 units at `a`.
    pub fn wchars(&self, a: u64, n: usize) -> R<Vec<u16>> {
        let b = self.m.read(a, n * 2).map_err(fault)?;
        Ok(b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect())
    }
    pub fn write_wchars(&mut self, a: u64, s: &[u16]) -> R<()> {
        let b: Vec<u8> = s.iter().flat_map(|c| c.to_le_bytes()).collect();
        self.write(a, &b)
    }
    /// A `UNICODE_STRING` at `a`, or `None` for a NULL pointer.
    pub fn ustr(&self, a: u64) -> R<Option<Vec<u16>>> {
        if a == 0 {
            return Ok(None);
        }
        let len = self.u16(a)? as usize;
        let buf = self.ptr(a + off!(self.abi, unicode_string::BUFFER))?;
        if len == 0 {
            return Ok(Some(Vec::new()));
        }
        if buf == 0 {
            return Err(Status(st::ACCESS_VIOLATION));
        }
        Ok(Some(self.wchars(buf, len / 2)?))
    }
    /// Write a `UNICODE_STRING` header at `a` naming `buf`.
    pub fn w_ustr_hdr(&mut self, a: u64, len_bytes: u16, max_bytes: u16, buf: u64) -> R<()> {
        self.w16(a, len_bytes)?;
        self.w16(a + 2, max_bytes)?;
        if self.p64() {
            self.w32(a + 4, 0)?;
        }
        self.wptr(a + off!(self.abi, unicode_string::BUFFER), buf)
    }
    pub fn us_size(&self) -> u64 {
        off!(self.abi, unicode_string::SIZEOF)
    }
    /// An `OBJECT_ATTRIBUTES`: (root directory handle, name, attributes).
    pub fn obj_attr(&self, a: u64) -> R<Option<ObjAttr>> {
        if a == 0 {
            return Ok(None);
        }
        let root = self.ptr(a + off!(self.abi, object_attributes::ROOTDIRECTORY))?;
        let name_p = self.ptr(a + off!(self.abi, object_attributes::OBJECTNAME))?;
        let attrs = self.u32(a + off!(self.abi, object_attributes::ATTRIBUTES))?;
        let name = self.ustr(name_p)?.unwrap_or_default();
        Ok(Some(ObjAttr { root: root as u32, name, attrs }))
    }
    /// `IO_STATUS_BLOCK`: status and information.
    pub fn iosb(&mut self, a: u64, status: u32, info: u64) -> R<()> {
        if a == 0 {
            return Ok(());
        }
        self.w32(a, status)?;
        if self.p64() {
            self.w32(a + 4, 0)?;
        }
        self.wptr(a + off!(self.abi, io_status_block::INFORMATION), info)
    }
}

/// A decoded `OBJECT_ATTRIBUTES`.
#[derive(Clone, Debug, Default)]
pub struct ObjAttr {
    pub root: u32,
    pub name: Vec<u16>,
    pub attrs: u32,
}

/// `OBJ_CASE_INSENSITIVE`, `OBJ_OPENIF`, …
pub mod obj_flags {
    pub const INHERIT: u32 = 0x02;
    pub const PERMANENT: u32 = 0x10;
    pub const EXCLUSIVE: u32 = 0x20;
    pub const CASE_INSENSITIVE: u32 = 0x40;
    pub const OPENIF: u32 = 0x80;
    pub const OPENLINK: u32 = 0x100;
}

pub fn wstr(s: &[u16]) -> String {
    String::from_utf16_lossy(s)
}

pub fn to_w(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// The guest's clocks, supplied by the host (the browser has no `Instant`).
#[derive(Clone, Copy)]
pub struct Clocks {
    /// Monotonic nanoseconds since some fixed point.
    pub mono_ns: fn() -> u64,
    /// Wall clock: seconds and nanoseconds since the Unix epoch.
    pub wall: fn() -> (u64, u32),
}

impl Clocks {
    /// Wall-clock time as Windows counts it: 100 ns units since 1601.
    pub fn filetime(&self) -> u64 {
        let (s, ns) = (self.wall)();
        (s + 11_644_473_600) * 10_000_000 + ns as u64 / 100
    }
}

/// One NT thread.
#[derive(Debug)]
pub struct Thread {
    /// The Windows thread id (`ClientId.UniqueThread`), a multiple of 4.
    pub tid: u32,
    /// gex's thread id for the same thread.
    pub gex_tid: i32,
    /// Where its gex register context lives.
    pub ctx_addr: u64,
    pub teb: u64,
    /// Its stack's reservation, released with the thread.
    pub stack: (u64, u64),
    /// Its futex word (docs/nt-layer.md §3.3).
    pub word: u64,
    /// Set by `NtAlertThreadByThreadId`, consumed by
    /// `NtWaitForAlertByThreadId`.
    pub alerted: AtomicBool,
    /// Pending user APCs.
    pub apcs: Mutex<std::collections::VecDeque<thread::Apc>>,
    /// Set once the thread has exited, with its exit code.
    pub exit_code: Mutex<Option<u32>>,
    /// The sync-object id waiters on the thread handle use.
    pub sync_id: u64,
    /// Suspend count; a thread created suspended starts at 1.
    pub suspend: AtomicU32,
    /// Nesting depth of user-mode callbacks (`KeUserModeCallback`).
    pub callback_depth: AtomicU32,
    /// What the innermost callback returned (`NtCallbackReturn`'s status and
    /// data), until [`thread::user_callback`] takes it.
    pub callback_result: Mutex<Option<(u32, Vec<u8>)>>,
}

/// Where the process's files come from.
#[derive(Clone, Debug)]
pub struct Config {
    /// Guest path of the staged prefix (`drive_c`, `nls`).
    pub prefix: String,
    /// The program's own path, as a Windows path (`Z:\…`).
    pub image_path: String,
    /// The command line, as Windows sees it.
    pub cmdline: String,
    /// `KEY=VALUE` strings for the Windows environment block.
    pub env: Vec<(String, String)>,
    /// The current directory, as a Windows path ending in `\`.
    pub cwd: String,
    /// Report every syscall on stderr.
    pub trace: bool,
    /// End the process on an unimplemented call instead of returning
    /// `STATUS_NOT_IMPLEMENTED`.
    pub strict: bool,
}

/// NT-side process state. Shared by all the process's threads.
pub struct Process {
    pub abi: Abi,
    /// Windows process id.
    pub pid: u32,
    pub cfg: Config,
    pub clocks: Clocks,
    pub futexes: Arc<FutexTable>,
    /// Handles, virtual memory, sections: one lock, taken briefly.
    pub st: Mutex<State>,
    /// Threads by Windows tid, and gex ctx address → tid.
    pub threads: RwLock<Threads>,
    /// Dispatcher-object state for waits.
    pub sync: Mutex<sync::SyncCore>,
    pub reg: reg::Registry,
    pub peb: AtomicU64,
    /// Addresses in ntdll the kernel needs (`LdrInitializeThunk`,
    /// `KiUserExceptionDispatcher`, …), filled at boot.
    pub ntdll: Mutex<boot::NtdllEntries>,
    pub exit_code: AtomicU32,
    pub terminating: AtomicBool,
    next_sync_id: AtomicU64,
    /// Where the NT-kernel-data page hands out thread wait words.
    next_word: AtomicU32,
    /// The process's own sync id, signalled when it ends.
    pub sync_id: u64,
    pub unix: unixcall::Registry,
    pub atoms: Mutex<sysinfo::Atoms>,
    /// Threads blocked on keyed events.
    pub keyed: Mutex<sync::Keyed>,
    /// USER: windows, classes, message queues (win32u).
    pub user: Mutex<user::User>,
    /// Threads `NtTerminateProcess(0)` told to stop; each exits at its next
    /// syscall.
    pub kill_list: Mutex<Vec<u32>>,
}

#[derive(Default)]
pub struct Threads {
    pub by_tid: HashMap<u32, Arc<Thread>>,
    pub by_ctx: HashMap<u64, Arc<Thread>>,
}

pub struct State {
    pub handles: object::Handles,
    pub vm: vm::Vm,
    /// Images mapped so far, by base, for `MemoryWineLoadUnixLib` and
    /// `NtQueryVirtualMemory(MemoryImageInformation)`.
    pub images: std::collections::BTreeMap<u64, section::ImageView>,
}

/// Where per-thread wait words live (docs/nt-layer.md §1.2).
pub const WAIT_WORDS: u64 = 0x003e_0000;
pub const WAIT_WORDS_END: u64 = 0x003f_0000;
/// The page of real code gex writes at boot.
pub const TRAMPOLINES: u64 = 0x003f_0000;
pub const TRAMPOLINES_END: u64 = 0x0040_0000;
/// `KUSER_SHARED_DATA`.
pub const USD: u64 = 0x7ffe_0000;

/// gex-private syscall numbers, issued only by gex's own trampolines.
pub mod gex_nr {
    /// `__wine_unix_call_dispatcher(handle, code, args)`.
    pub const UNIX_CALL: u32 = 0x7f00_0001;
    /// The x86-64 `__wine_syscall_dispatcher` was called, which Wine's
    /// x86-64 stubs never do while `USD.SystemCall` is 0.
    pub const BAD_DISPATCH: u32 = 0x7f00_0002;
}

impl Process {
    pub fn new(abi: Abi, pid: u32, cfg: Config, clocks: Clocks, futexes: Arc<FutexTable>) -> Process {
        Process {
            abi,
            pid,
            cfg,
            clocks,
            futexes,
            st: Mutex::new(State {
                handles: object::Handles::default(),
                vm: vm::Vm::new(),
                images: Default::default(),
            }),
            threads: RwLock::new(Threads::default()),
            sync: Mutex::new(sync::SyncCore::default()),
            reg: reg::Registry::new(),
            peb: AtomicU64::new(0),
            ntdll: Mutex::new(boot::NtdllEntries::default()),
            exit_code: AtomicU32::new(0),
            terminating: AtomicBool::new(false),
            next_sync_id: AtomicU64::new(2),
            next_word: AtomicU32::new(0),
            sync_id: 1,
            unix: unixcall::Registry::default(),
            atoms: Mutex::new(Default::default()),
            keyed: Mutex::new(Default::default()),
            user: Mutex::new(user::User::new()),
            kill_list: Mutex::new(Vec::new()),
        }
    }

    pub fn new_sync_id(&self) -> u64 {
        self.next_sync_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn alloc_wait_word(&self) -> Option<u64> {
        let i = self.next_word.fetch_add(1, Ordering::Relaxed) as u64;
        let a = WAIT_WORDS + 4 * i;
        (a < WAIT_WORDS_END).then_some(a)
    }

    pub fn thread_by_ctx(&self, ctx_addr: u64) -> Option<Arc<Thread>> {
        self.threads.read().unwrap().by_ctx.get(&ctx_addr).cloned()
    }

    pub fn thread_by_tid(&self, tid: u32) -> Option<Arc<Thread>> {
        self.threads.read().unwrap().by_tid.get(&tid).cloned()
    }

    pub fn add_thread(&self, t: Arc<Thread>) {
        let mut th = self.threads.write().unwrap();
        th.by_ctx.insert(t.ctx_addr, Arc::clone(&t));
        th.by_tid.insert(t.tid, t);
    }

    pub fn remove_thread(&self, t: &Thread) {
        let mut th = self.threads.write().unwrap();
        th.by_ctx.remove(&t.ctx_addr);
        th.by_tid.remove(&t.tid);
    }

    /// Report an unimplemented call (or an unimplemented case of one).
    /// Printed every time, never deduplicated.
    pub fn unimplemented(&self, what: &str) -> Status {
        eprintln!("gex-nt: unimplemented: {what}");
        if self.cfg.strict {
            eprintln!("gex-nt: GEX_NT_STRICT is set; ending the process");
            UNIMPLEMENTED_STRICT.store(true, Ordering::Relaxed);
        }
        Status(st::NOT_IMPLEMENTED)
    }
}

/// Set when a strict run hit an unimplemented call; the dispatcher turns it
/// into a process exit.
static UNIMPLEMENTED_STRICT: AtomicBool = AtomicBool::new(false);

/// What the host must do once a syscall has been handled.
#[derive(Debug)]
pub enum Outcome {
    /// Continue after the syscall instruction (the result is in `rax`).
    Continue,
    /// Continue at this pc, with the whole register context already set
    /// (`NtContinue`, exception dispatch, APC delivery).
    Resume(u64),
    /// The process has ended with this exit code.
    Exit(u32),
    /// This thread has ended.
    ThreadExit,
    /// Start a gex thread at the context at `ctx_addr` (already filled), then
    /// continue this one after the syscall.
    SpawnThread { ctx_addr: u64, gex_tid: i32 },
    /// `NtCallbackReturn`: end the innermost nested guest loop.
    CallbackReturn,
}

/// What the host provides the NT layer beyond memory and the kernel: running
/// guest code on the calling thread, for user-mode callbacks
/// (docs/nt-layer.md §5).
pub trait NtHost {
    /// Run the calling thread's guest code from its current context (the
    /// pc in its gex context) until it issues `NtCallbackReturn`. `Err` means
    /// the thread or the process is ending, or the run failed; the host has
    /// recorded why.
    fn run_callback(&mut self) -> Result<(), ()>;
}

/// A host that cannot run guest code (processor exceptions, tests).
pub struct NoHost;

impl NtHost for NoHost {
    fn run_callback(&mut self) -> Result<(), ()> {
        eprintln!("gex-nt: a user-mode callback where the host cannot run one");
        Err(())
    }
}

/// One syscall being handled.
pub struct Call<'a> {
    pub p: &'a Arc<Process>,
    pub t: Arc<Thread>,
    pub kernel: &'a Mutex<Kernel>,
    pub g: G<'a>,
    pub nr: u32,
    pub pc: u64,
    /// Set by a handler that wants something other than "return a status".
    pub outcome: Option<Outcome>,
    /// A full-width result for `rax` (win32u calls return handles and
    /// LRESULTs, not NTSTATUS).
    pub ret64: Option<u64>,
    pub host: &'a mut dyn NtHost,
}

impl<'a> Call<'a> {
    fn creg(&self, off: usize) -> u64 {
        self.g.u64(self.t.ctx_addr + off as u64).unwrap_or(0)
    }

    /// Syscall argument `i` (0-based).
    pub fn a(&self, i: usize) -> u64 {
        match self.p.abi {
            Abi::P64 => match i {
                0 => self.creg(ctx::x(10)),
                1 => self.creg(ctx::x(2)),
                2 => self.creg(ctx::x(8)),
                3 => self.creg(ctx::x(9)),
                _ => {
                    let sp = self.creg(ctx::x(4));
                    self.g.u64(sp + 0x28 + 8 * (i as u64 - 4)).unwrap_or(0)
                }
            },
            // The i386 dispatcher trampoline is `call`ed from the stub, which
            // was `call`ed by the caller: two return addresses, then the
            // stdcall arguments.
            Abi::P32 => {
                let sp = self.creg(ctx::x(4)) & 0xffff_ffff;
                self.g.u32(sp + 8 + 4 * i as u64).map(u64::from).unwrap_or(0)
            }
        }
    }

    /// Argument `i` as a 32-bit value (ULONG, HANDLE, NTSTATUS, BOOLEAN, …).
    pub fn a32(&self, i: usize) -> u32 {
        self.a(i) as u32
    }

    pub fn kernel(&self) -> std::sync::MutexGuard<'a, Kernel> {
        self.kernel.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn unimplemented(&self, what: &str) -> Status {
        self.p.unimplemented(what)
    }

    /// The name of this call, for messages.
    pub fn name(&self) -> String {
        syscall_name(self.nr)
    }
}

pub fn syscall_name(nr: u32) -> String {
    match nr {
        gex_nr::UNIX_CALL => return "__wine_unix_call".into(),
        gex_nr::BAD_DISPATCH => return "__wine_syscall_dispatcher".into(),
        _ => {}
    }
    let (table, base, label) = if nr < 0x1000 {
        (&sysno::NTDLL_TABLE[..], 0, "ntdll")
    } else {
        (&sysno::WIN32U_TABLE[..], 0x1000, "win32u")
    };
    match table.get((nr.wrapping_sub(base)) as usize) {
        Some((n, _, _)) if nr < 0x2000 => n.to_string(),
        _ => format!("<{label}? syscall {nr:#x}>"),
    }
}

/// A processor exception in translated code (`int3`, an instruction gex
/// cannot run, a branch to memory that is not executable), delivered as an
/// NT exception the way Wine's signal handlers do: a CONTEXT of the faulting
/// state and a record, then `KiUserExceptionDispatcher`. `rip` is the pc the
/// context reports (after the `int3` for a breakpoint, which the dispatch
/// adjusts); `info` fills `ExceptionInformation`.
pub fn hardware_exception(
    p: &Arc<Process>,
    kernel: &Mutex<Kernel>,
    data: &mut [u8],
    ctx_addr: u64,
    code: u32,
    rip: u64,
    info: &[u64],
) -> Outcome {
    let g = G::new(data, p.abi);
    let Some(t) = p.thread_by_ctx(ctx_addr) else {
        eprintln!("gex-nt: exception {code:#x} in a thread the NT layer does not know (ctx {ctx_addr:#x})");
        return Outcome::Exit(code);
    };
    let abi = p.abi;
    if p.cfg.trace {
        let r = |o: usize| g.u64(ctx_addr + o as u64).unwrap_or(0);
        eprintln!(
            "[nt:{:x}] exception {code:#010x} at {rip:#x} (sp {:#x}, last indirect branch {:#x} from block {:#x})",
            t.tid,
            r(ctx::x(4)),
            r(ctx::LAST_TARGET),
            r(ctx::LAST_SOURCE)
        );
    }
    let mut c = Call { p, t, kernel, g, nr: 0, pc: rip, outcome: None, ret64: None, host: &mut NoHost };
    let r = (|| -> R<()> {
        let sp = c.g.u64(ctx_addr + ctx::x(4) as u64)?;
        let context = ((sp - 0x5c0) & !63) - 0x600;
        thread::save_context(&mut c.g, ctx_addr, context, rip)?;
        // EXCEPTION_RECORD: code, flags, record, address, parameter count,
        // parameters, with pointer-sized fields for the ABI.
        let ps = abi.ptr_size() as usize;
        let mut rec = vec![0u8; thread::record_size(abi) as usize];
        let mut put = |at: usize, v: u64, n: usize| rec[at..at + n].copy_from_slice(&v.to_le_bytes()[..n]);
        put(0, code as u64, 4);
        let addr = if code == st::BREAKPOINT { rip - 1 } else { rip };
        put(8 + ps, addr, ps);
        put(8 + 2 * ps, info.len() as u64, 4);
        let params = if abi == Abi::P64 { 32 } else { 0x14 };
        for (i, v) in info.iter().enumerate() {
            put(params + ps * i, *v, ps);
        }
        thread::dispatch_exception(&mut c, &rec, context)
    })();
    match (r, c.outcome.take()) {
        (Ok(()), Some(o)) => o,
        (r, _) => {
            eprintln!("gex-nt: could not dispatch exception {code:#x} at {rip:#x}: {r:?}");
            Outcome::Exit(code)
        }
    }
}

/// Handle one syscall from an NT process. `data` is the whole guest memory.
pub fn syscall(
    p: &Arc<Process>,
    kernel: &Mutex<Kernel>,
    data: &mut [u8],
    ctx_addr: u64,
    pc: u64,
    host: &mut dyn NtHost,
) -> Outcome {
    let abi = p.abi;
    let g = G::new(data, abi);
    let nr = g.u64(ctx_addr + ctx::x(0) as u64).unwrap_or(!0) as u32;
    let Some(t) = p.thread_by_ctx(ctx_addr) else {
        eprintln!("gex-nt: syscall {nr:#x} from a thread the NT layer does not know (ctx {ctx_addr:#x})");
        return Outcome::Exit(st::INVALID_CID);
    };
    if p.terminating.load(Ordering::Acquire) {
        return Outcome::Exit(p.exit_code.load(Ordering::Acquire));
    }
    if p.kill_list.lock().unwrap().contains(&t.tid) {
        let mut c = Call { p, t, kernel, g, nr, pc, outcome: None, ret64: None, host };
        thread::exit_thread(&mut c, p.exit_code.load(Ordering::Acquire));
        return Outcome::ThreadExit;
    }
    let mut c = Call { p, t, kernel, g, nr, pc, outcome: None, ret64: None, host };
    let r = dispatch(&mut c);
    let status = match r {
        Ok(s) => s,
        Err(Status(s)) => s,
    };
    if p.cfg.trace {
        eprintln!("[nt:{:x}] {} -> {status:#010x}", c.t.tid, c.name());
    }
    if UNIMPLEMENTED_STRICT.load(Ordering::Relaxed) {
        // The whole process ends, not just this thread: the others notice at
        // their next syscall, or are woken from a wait to notice.
        let mut c = c;
        thread::terminate_process(&mut c, st::NOT_IMPLEMENTED);
        return Outcome::Exit(st::NOT_IMPLEMENTED);
    }
    // Another thread ended the process while this call ran (a wait woken
    // for the purpose returns here).
    if p.terminating.load(Ordering::Acquire) && !matches!(c.outcome, Some(Outcome::Exit(_))) {
        return Outcome::Exit(p.exit_code.load(Ordering::Acquire));
    }
    match c.outcome.take() {
        Some(o @ (Outcome::Resume(_) | Outcome::Exit(_) | Outcome::ThreadExit | Outcome::CallbackReturn)) => o,
        other => {
            // NTSTATUS is 32 bits; the upper half of rax is zero, as after
            // Wine's `movl %eax` return. win32u calls return their own value.
            let v = c.ret64.unwrap_or(status as u64);
            let _ = c.g.w64(ctx_addr + ctx::x(0) as u64, v);
            other.unwrap_or(Outcome::Continue)
        }
    }
}

fn dispatch(c: &mut Call<'_>) -> R {
    use sysno::ntdll as n;
    match c.nr {
        // -- virtual memory ------------------------------------------------
        n::NtAllocateVirtualMemory => vm::nt_allocate_virtual_memory(c),
        n::NtAllocateVirtualMemoryEx => vm::nt_allocate_virtual_memory_ex(c),
        n::NtFreeVirtualMemory => vm::nt_free_virtual_memory(c),
        n::NtProtectVirtualMemory => vm::nt_protect_virtual_memory(c),
        n::NtQueryVirtualMemory => vm::nt_query_virtual_memory(c),
        n::NtFlushInstructionCache => vm::nt_flush_instruction_cache(c),
        n::NtReadVirtualMemory => vm::nt_read_virtual_memory(c),
        // -- sections --------------------------------------------------------
        n::NtCreateSection => section::nt_create_section(c),
        n::NtOpenSection => section::nt_open_section(c),
        n::NtMapViewOfSection => section::nt_map_view_of_section(c),
        n::NtMapViewOfSectionEx => section::nt_map_view_of_section_ex(c),
        n::NtUnmapViewOfSection => section::nt_unmap_view_of_section(c),
        n::NtUnmapViewOfSectionEx => section::nt_unmap_view_of_section_ex(c),
        n::NtQuerySection => section::nt_query_section(c),
        // -- objects ---------------------------------------------------------
        n::NtClose => object::nt_close(c),
        n::NtDuplicateObject => object::nt_duplicate_object(c),
        n::NtOpenDirectoryObject => object::nt_open_directory_object(c),
        n::NtQueryObject => object::nt_query_object(c),
        n::NtSetInformationObject => object::nt_set_information_object(c),
        n::NtOpenSymbolicLinkObject => object::nt_open_symbolic_link_object(c),
        // -- files -----------------------------------------------------------
        n::NtCreateFile => file::nt_create_file(c),
        n::NtOpenFile => file::nt_open_file(c),
        n::NtReadFile => file::nt_read_file(c),
        n::NtWriteFile => file::nt_write_file(c),
        n::NtQueryAttributesFile => file::nt_query_attributes_file(c),
        n::NtQueryFullAttributesFile => file::nt_query_full_attributes_file(c),
        n::NtQueryInformationFile => file::nt_query_information_file(c),
        n::NtSetInformationFile => file::nt_set_information_file(c),
        n::NtQueryVolumeInformationFile => file::nt_query_volume_information_file(c),
        n::NtQueryDirectoryFile => file::nt_query_directory_file(c),
        n::NtFsControlFile => file::nt_fs_control_file(c),
        n::NtDeviceIoControlFile => file::nt_device_io_control_file(c),
        n::NtFlushBuffersFile => file::nt_flush_buffers_file(c),
        // -- registry --------------------------------------------------------
        n::NtCreateKey => reg::nt_create_key(c),
        n::NtOpenKey => reg::nt_open_key(c),
        n::NtOpenKeyEx => reg::nt_open_key_ex(c),
        n::NtQueryValueKey => reg::nt_query_value_key(c),
        n::NtSetValueKey => reg::nt_set_value_key(c),
        n::NtEnumerateKey => reg::nt_enumerate_key(c),
        n::NtEnumerateValueKey => reg::nt_enumerate_value_key(c),
        n::NtQueryKey => reg::nt_query_key(c),
        n::NtDeleteValueKey => reg::nt_delete_value_key(c),
        // -- sync --------------------------------------------------------------
        n::NtCreateEvent => sync::nt_create_event(c),
        n::NtOpenEvent => sync::nt_open_event(c),
        n::NtSetEvent => sync::nt_set_event(c),
        n::NtResetEvent => sync::nt_reset_event(c),
        n::NtClearEvent => sync::nt_clear_event(c),
        n::NtPulseEvent => sync::nt_pulse_event(c),
        n::NtCreateMutant => sync::nt_create_mutant(c),
        n::NtOpenMutant => sync::nt_open_mutant(c),
        n::NtReleaseMutant => sync::nt_release_mutant(c),
        n::NtCreateSemaphore => sync::nt_create_semaphore(c),
        n::NtOpenSemaphore => sync::nt_open_semaphore(c),
        n::NtReleaseSemaphore => sync::nt_release_semaphore(c),
        n::NtCreateKeyedEvent => sync::nt_create_keyed_event(c),
        n::NtCreateTimer => sync::nt_create_timer(c),
        n::NtOpenTimer => sync::nt_open_timer(c),
        n::NtSetTimer => sync::nt_set_timer(c),
        n::NtCancelTimer => sync::nt_cancel_timer(c),
        n::NtQueryTimer => sync::nt_query_timer(c),
        n::NtWaitForKeyedEvent => sync::nt_wait_for_keyed_event(c),
        n::NtReleaseKeyedEvent => sync::nt_release_keyed_event(c),
        n::NtWaitForSingleObject => sync::nt_wait_for_single_object(c),
        n::NtWaitForMultipleObjects => sync::nt_wait_for_multiple_objects(c),
        n::NtSignalAndWaitForSingleObject => sync::nt_signal_and_wait_for_single_object(c),
        n::NtWaitForAlertByThreadId => sync::nt_wait_for_alert_by_thread_id(c),
        n::NtAlertThreadByThreadId => sync::nt_alert_thread_by_thread_id(c),
        n::NtAlertMultipleThreadByThreadId => sync::nt_alert_multiple_thread_by_thread_id(c),
        n::NtDelayExecution => sync::nt_delay_execution(c),
        n::NtYieldExecution => sync::nt_yield_execution(c),
        n::NtTestAlert => thread::nt_test_alert(c),
        // -- threads, processes, exceptions ------------------------------------
        n::NtContinue => thread::nt_continue(c),
        n::NtContinueEx => thread::nt_continue_ex(c),
        n::NtRaiseException => thread::nt_raise_exception(c),
        n::NtCreateThreadEx => thread::nt_create_thread_ex(c),
        n::NtCreateThread => Err(c.unimplemented("NtCreateThread (only NtCreateThreadEx is)")),
        n::NtTerminateThread => thread::nt_terminate_thread(c),
        n::NtTerminateProcess => thread::nt_terminate_process(c),
        n::NtResumeThread => thread::nt_resume_thread(c),
        n::NtGetNextThread => thread::nt_get_next_thread(c),
        n::NtQueryInformationThread => thread::nt_query_information_thread(c),
        n::NtSetInformationThread => thread::nt_set_information_thread(c),
        n::NtQueryInformationProcess => thread::nt_query_information_process(c),
        n::NtSetInformationProcess => thread::nt_set_information_process(c),
        n::NtOpenThread => thread::nt_open_thread(c),
        n::NtOpenProcess => thread::nt_open_process(c),
        n::NtGetContextThread => thread::nt_get_context_thread(c),
        n::NtSetContextThread => thread::nt_set_context_thread(c),
        n::NtQueueApcThread => thread::nt_queue_apc_thread(c),
        n::NtCallbackReturn => thread::nt_callback_return(c),
        n::NtOpenProcessToken | n::NtOpenProcessTokenEx => sysinfo::nt_open_process_token(c),
        n::NtOpenThreadToken | n::NtOpenThreadTokenEx => sysinfo::nt_open_thread_token(c),
        n::NtQueryInformationToken => sysinfo::nt_query_information_token(c),
        // -- system information, time, locale ---------------------------------
        n::NtQuerySystemInformation => sysinfo::nt_query_system_information(c),
        n::NtQuerySystemInformationEx => sysinfo::nt_query_system_information_ex(c),
        n::NtQueryPerformanceCounter => sysinfo::nt_query_performance_counter(c),
        n::NtQuerySystemTime => sysinfo::nt_query_system_time(c),
        n::NtQueryDefaultLocale => sysinfo::nt_query_default_locale(c),
        n::NtQueryDefaultUILanguage => sysinfo::nt_query_default_ui_language(c),
        n::NtQueryInstallUILanguage => sysinfo::nt_query_install_ui_language(c),
        n::NtInitializeNlsFiles => sysinfo::nt_initialize_nls_files(c),
        n::NtGetNlsSectionPtr => sysinfo::nt_get_nls_section_ptr(c),
        n::NtQueryTimerResolution => sysinfo::nt_query_timer_resolution(c),
        n::NtSetTimerResolution => sysinfo::nt_set_timer_resolution(c),
        n::NtAddAtom => sysinfo::nt_add_atom(c),
        n::NtFindAtom => sysinfo::nt_find_atom(c),
        n::NtDeleteAtom => sysinfo::nt_delete_atom(c),
        n::NtQueryInformationAtom => sysinfo::nt_query_information_atom(c),
        n::NtAllocateLocallyUniqueId => sysinfo::nt_allocate_locally_unique_id(c),
        n::NtQueryLicenseValue => sysinfo::nt_query_license_value(c),
        n::NtWow64IsProcessorFeaturePresent => sysinfo::nt_wow64_is_processor_feature_present(c),
        n::NtTraceControl => Err(Status(st::NOT_SUPPORTED)).inspect_err(|_| {
            c.p.unimplemented("NtTraceControl (event tracing)");
        }),
        gex_nr::UNIX_CALL => unixcall::dispatch(c),
        gex_nr::BAD_DISPATCH => Err(c.unimplemented("a call through __wine_syscall_dispatcher on x86-64")),
        nr if (0x1000..0x2000).contains(&nr) => {
            // A failed win32u call returns 0, with any last error already set.
            let v = user::dispatch(c).unwrap_or(0);
            c.ret64 = Some(v);
            Ok(v as u32)
        }
        nr => Err(c.unimplemented(&format!("{} ({nr:#x})", syscall_name(nr)))),
    }
}
