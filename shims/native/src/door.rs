//! Door calls into a wineserver on its own memory (docs/wineserver-kernel.md).
//!
//! The server (Wine's `server/door.c`) is a native wasm32 program, its own
//! process. When it is up it publishes a door area (`gex.door_publish`).
//! From then on any thread of any process makes a server request by calling
//! `gex.door_call`: this host takes the server's door lock, copies the
//! request into the server memory, and runs `__wine_door_dispatch` on its
//! own instance of the server's module (one per host thread, on the server's
//! memory), then copies the reply back. No pipe and no wake.
//!
//! A system call the server code makes inside a door is answered in place
//! when it needs no process (the clock, getpid) and otherwise proxied: posted
//! in the area and made by the server's main thread (the pump) as the server
//! process, which serves proxies from both of its waits.
//!
//! Natively this is the debugging oracle for the page's door glue.

use super::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use wasmtime::{Global, GlobalType, Mutability, SharedMemory, TableType, ValType};

/// The area's layout (server/door.c `struct door_area`).
mod area {
    pub const LOCK: u32 = 0;
    pub const PROXY_STATE: u32 = 4;
    pub const PROXY_NR: u32 = 8;
    pub const PROXY_ARGS: u32 = 12;
    pub const PROXY_RET: u32 = 36;
    pub const VERSION: u32 = 40;
    pub const BUF: u32 = 44;
    pub const BUF_SIZE: u32 = 48;
    pub const STACK_TOP: u32 = 52;
    pub const DISPATCH: u32 = 60;
    pub const KICK_FD: u32 = 68;
    pub const CTX: u32 = 72;
    pub const KICK_NEEDED: u32 = 80;
    pub const STATS: u32 = 96;
    pub const PROXIES: u32 = 100;
}

/// A native wasm32 program as installed, by pid (`wasmprog::install`): what
/// a door instance needs to instantiate it again on the same memory.
#[derive(Clone)]
pub(crate) struct Prog {
    pub module: Module,
    pub memory_base: u32,
    pub table_base: u32,
    pub table_len: u32,
    /// `__gex_thread_start`'s table index (absolute).
    pub thread_start: u32,
}

static PROGS: Mutex<Option<HashMap<u32, Prog>>> = Mutex::new(None);

pub(crate) fn note_program(pid: u32, prog: Prog) {
    PROGS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(pid, prog);
}

struct Server {
    sm: SharedMemory,
    mem: gex_mem::GuestMemory,
    prog: Prog,
    area: u32,
    kick: Arc<dyn gex_kernel::vfs::FileHandle>,
    pid: u32,
    id: u64,
    stats: bool,
}

static SERVER: Mutex<Option<Arc<Server>>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A door call's census (GEX_DOOR_STATS=1): calls, proxies, time.
static CALLS: AtomicU64 = AtomicU64::new(0);
static FALLBACKS: AtomicU64 = AtomicU64::new(0);

impl Server {
    fn at(&self, off: u32) -> u64 {
        (self.area + off) as u64
    }
    fn get(&self, off: u32) -> u32 {
        gex_atomic::load_u32(self.mem.bytes(), self.at(off)).unwrap_or(0)
    }
    fn put(&self, off: u32, v: u32) {
        let _ = gex_atomic::store_u32(self.mem.bytes(), self.at(off), v);
    }
    fn cas(&self, off: u32, old: u32, new: u32) -> Result<u32, u32> {
        gex_atomic::cas_u32(self.mem.bytes(), self.at(off), old, new).unwrap_or(Err(!0))
    }
    fn swap(&self, off: u32, v: u32) -> u32 {
        gex_atomic::swap_u32(self.mem.bytes(), self.at(off), v).unwrap_or(0)
    }
    fn lock(&self) {
        let mut spins = 0;
        loop {
            match self.cas(area::LOCK, 0, 1) {
                Ok(_) => return,
                Err(v) => {
                    if spins < 200 {
                        spins += 1;
                        std::hint::spin_loop();
                        continue;
                    }
                    if v == 1 && self.cas(area::LOCK, 1, 2).is_err() {
                        continue;
                    }
                    let _ = self.sm.atomic_wait32((self.area + area::LOCK) as u64, 2, Some(std::time::Duration::from_millis(1)));
                }
            }
        }
    }
    fn unlock(&self) {
        if self.swap(area::LOCK, 0) == 2 {
            let _ = self.sm.atomic_notify((self.area + area::LOCK) as u64, u32::MAX);
        }
    }
    fn kick(&self) {
        let _ = self.kick.write_at(0, b"k");
    }
}

/// `gex.door_publish(area)`: the calling process is a wineserver whose door
/// area is at `area` in its memory.
pub(crate) fn publish(caller: &mut Caller<'_, HostState>, area_at: u32) -> Result<()> {
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let pid = caller.data_mut().with_kernel(|k| k.pid) as u32;
    let prog = PROGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(&pid).cloned())
        .ok_or_else(|| anyhow!("gex.door_publish from a process that is not a wasm32 program"))?;
    let kick_fd = i32::from_le_bytes(memory.bytes()[(area_at + area::KICK_FD) as usize..][..4].try_into().unwrap());
    let kick = caller
        .data_mut()
        .with_kernel(|k| k.fds.get(kick_fd).map(|f| Arc::clone(&f.handle)))
        .map_err(|e| anyhow!("door kick fd {kick_fd}: {e:?}"))?;
    let version = u32::from_le_bytes(memory.bytes()[(area_at + area::VERSION) as usize..][..4].try_into().unwrap());
    if version != 1 {
        bail!("door area version {version} (want 1)");
    }
    let server = Server {
        sm: memory.shared().clone(),
        mem: memory.clone(),
        prog,
        area: area_at,
        kick,
        pid,
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        stats: std::env::var_os("GEX_DOOR_STATS").is_some(),
    };
    if std::env::var_os("GEX_DOOR_TRACE").is_some() || server.stats {
        eprintln!("gex-door: wineserver pid {pid} published its door (area {area_at:#x})");
    }
    *SERVER.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(server));
    if server_stats_on() {
        std::thread::spawn(stats_loop);
    }
    Ok(())
}

fn server_stats_on() -> bool {
    std::env::var_os("GEX_DOOR_STATS").is_some()
}

/// Every 10 s: door calls, fallbacks, and the proxied system calls by number.
fn stats_loop() {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(10));
        let Some(s) = SERVER.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
        let stats = s.get(area::STATS) as usize;
        let b = s.mem.bytes();
        let mut line = format!(
            "gex-door: calls {} fallbacks {} proxies {}:",
            CALLS.load(Ordering::Relaxed),
            FALLBACKS.load(Ordering::Relaxed),
            s.get(area::PROXIES)
        );
        for nr in 0..512usize {
            let n = u32::from_le_bytes(b[stats + nr * 4..][..4].try_into().unwrap());
            if n != 0 {
                line += &format!(" {nr}:{n}");
            }
        }
        eprintln!("{line}");
    }
}

#[derive(Debug)]
struct DoorExit;
impl std::fmt::Display for DoorExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("door instance ready")
    }
}
impl std::error::Error for DoorExit {}

struct Inst {
    id: u64,
    store: Store<()>,
    dispatch: wasmtime::TypedFunc<(i32, i32), i32>,
    sp: Global,
}

thread_local! {
    static INST: RefCell<Option<Inst>> = const { RefCell::new(None) };
}

/// The i386 numbers a door answers in place.
const NR_EXIT: u32 = 1;
const NR_GETPID: u32 = 20;
const NR_GETTID: u32 = 224;
const NR_EXIT_GROUP: u32 = 252;

/// A system call from server code inside a door.
fn door_syscall(s: &Server, ctx: u32) -> wasmtime::Result<()> {
    let b = s.mem.bytes();
    let rd = |slot: usize| u32::from_le_bytes(b[ctx as usize + slot * 8..][..4].try_into().unwrap());
    let nr = rd(0);
    let set = |v: u32| {
        let b = s.mem.bytes();
        b[ctx as usize..ctx as usize + 8].copy_from_slice(&(v as u64).to_le_bytes());
    };
    match nr {
        NR_EXIT | NR_EXIT_GROUP => return Err(wasmtime::Error::new(DoorExit)),
        NR_GETPID | NR_GETTID => {
            set(s.pid);
            return Ok(());
        }
        _ => {}
    }
    {
        let mut gm = GuestMem::new(s.mem.bytes());
        if gex_kernel::syscalls::clock_call(&mut gm, ctx as u64, true, gex_kernel::system_clock, 0) {
            return Ok(());
        }
    }
    // Proxied: the pump makes it as the server process. Arguments in the
    // i386 order: ebx, ecx, edx, esi, edi, ebp.
    let args = [rd(3), rd(1), rd(2), rd(6), rd(7), rd(5)];
    let w = |off: u32, v: u32| s.put(off, v);
    w(area::PROXY_NR, nr);
    for (k, a) in args.iter().enumerate() {
        w(area::PROXY_ARGS + 4 * k as u32, *a);
    }
    let stats = s.get(area::STATS);
    if stats != 0 && nr < 512 {
        let _ = gex_atomic::add_u32(s.mem.bytes(), (stats + nr * 4) as u64, 1);
    }
    let _ = gex_atomic::add_u32(s.mem.bytes(), s.at(area::PROXIES), 1);
    s.put(area::PROXY_STATE, 1);
    // The pump may be waiting for the lock (this notify) or in epoll_wait
    // (the kick).
    let _ = s.sm.atomic_notify((s.area + area::LOCK) as u64, u32::MAX);
    s.kick();
    loop {
        if s.get(area::PROXY_STATE) == 2 {
            break;
        }
        let _ = s.sm.atomic_wait32((s.area + area::PROXY_STATE) as u64, 1, Some(std::time::Duration::from_millis(5)));
    }
    let ret = s.get(area::PROXY_RET);
    s.put(area::PROXY_STATE, 0);
    set(ret);
    Ok(())
}

fn instantiate(engine: &Engine, s: &Arc<Server>) -> Result<Inst> {
    let mut store = Store::new(engine, ());
    let mut l: Linker<()> = Linker::new(engine);
    let p = &s.prog;
    l.define(&mut store, "env", "memory", s.sm.clone())?;
    let table = Table::new(&mut store, TableType::new(wasmtime::RefType::FUNCREF, p.table_base + p.table_len, None), Ref::Func(None))?;
    l.define(&mut store, "env", "__indirect_function_table", table)?;
    let stack_top = s.get(area::STACK_TOP);
    let sp = Global::new(&mut store, GlobalType::new(ValType::I32, Mutability::Var), Val::I32(stack_top as i32))?;
    l.define(&mut store, "env", "__stack_pointer", sp)?;
    let mb = Global::new(&mut store, GlobalType::new(ValType::I32, Mutability::Const), Val::I32(p.memory_base as i32))?;
    l.define(&mut store, "env", "__memory_base", mb)?;
    let tb = Global::new(&mut store, GlobalType::new(ValType::I32, Mutability::Const), Val::I32(p.table_base as i32))?;
    l.define(&mut store, "env", "__table_base", tb)?;
    let sv = Arc::clone(s);
    l.func_wrap("gex", "syscall", move |_c: Caller<'_, ()>, ctx: i32, _pc: i64| -> wasmtime::Result<()> { door_syscall(&sv, ctx as u32) })?;
    // musl's fork driver asks asyncify's state around every thread's
    // function: never unwinding here (the server is not fork-instrumented).
    l.func_wrap("gex", "asyncify", |_c: Caller<'_, ()>, op: i32, _arg: i32| -> wasmtime::Result<i32> {
        if op == 0 { Ok(0) } else { Err(wasmtime::Error::msg("gex.asyncify: no fork inside a door call")) }
    })?;
    // Anything else the module imports: not available inside a door.
    for imp in p.module.imports() {
        if l.get(&mut store, imp.module(), imp.name()).is_ok() {
            continue;
        }
        if let wasmtime::ExternType::Func(ft) = imp.ty() {
            let label = format!("{}.{}", imp.module(), imp.name());
            let f = wasmtime::Func::new(&mut store, ft, move |_, _, _| Err(wasmtime::Error::msg(format!("{label}: not available inside a door call"))));
            l.define(&mut store, imp.module(), imp.name(), f)?;
        }
    }
    let _inst = l.instantiate(&mut store, &p.module).context("instantiating the wineserver for doors")?;
    // The instance's first call sets its context, thread pointer and TLS
    // (`__gex_thread_start` on the area's frame) and ends at the exit call.
    let start = match table.get(&mut store, p.thread_start as u64) {
        Some(Ref::Func(Some(f))) => f,
        _ => bail!("door: no __gex_thread_start at {}", p.thread_start),
    };
    let ctx = s.get(area::CTX) as i32;
    // ESP at the frame: a door's system calls move it (musl's stub
    // publishes its C stack pointer there).
    let frame_at = s.get(76);
    s.mem.bytes()[ctx as usize + 32..ctx as usize + 40].copy_from_slice(&(frame_at as u64).to_le_bytes());
    if std::env::var_os("GEX_DOOR_TRACE").is_some() {
        let b = s.mem.bytes();
        let w = |a: u32| u32::from_le_bytes(b[a as usize..][..4].try_into().unwrap());
        let frame = s.get(76);
        eprintln!(
            "gex-door: instance: ctx {ctx:#x} esp {:#x} frame {frame:#x} [{:#x} {:#x} {:#x} {:#x}] start {} stack_top {:#x} tp {:#x}",
            w(ctx as u32 + 32), w(frame), w(frame + 4), w(frame + 8), w(frame + 12), p.thread_start, stack_top, s.get(56)
        );
    }
    let start = start.typed::<(i32, i32, i32, i64), i32>(&store)?;
    match start.call(&mut store, (ctx, 0, 0, 0)) {
        Err(e) if e.downcast_ref::<DoorExit>().is_some() => {}
        Err(e) => bail!("starting a door instance: {e:?} {:?}", e.downcast_ref::<wasmtime::WasmBacktrace>()),
        Ok(_) => bail!("door: __gex_thread_start returned"),
    }
    let dispatch = match table.get(&mut store, s.get(area::DISPATCH) as u64) {
        Some(Ref::Func(Some(f))) => f.typed::<(i32, i32), i32>(&store)?,
        _ => bail!("door: no dispatch function"),
    };
    Ok(Inst { id: s.id, store, dispatch, sp })
}

/// `gex.door_call(req, unix_pid, unix_tid)`: 0 done (reply in `req`), 1 use
/// the pipes, 2 the thread died in the request.
pub(crate) fn call(caller: &mut Caller<'_, HostState>, req: u32, pid: i32, tid: i32) -> Result<i32> {
    let Some(s) = SERVER.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return Ok(1) };
    let cm = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let rd = |at: u32| u32::from_le_bytes(cm.bytes()[at as usize..][..4].try_into().unwrap());
    let req_size = rd(req + 4);
    let reply_max = rd(req + 8);
    let data_count = rd(req + 64).min(16);
    let reply_data = rd(req + 68);
    let reply_off = (64 + req_size + 7) & !7;
    let buf = s.get(area::BUF);
    let buf_size = s.get(area::BUF_SIZE);
    if reply_off as u64 + 64 + reply_max as u64 > buf_size as u64 {
        FALLBACKS.fetch_add(1, Ordering::Relaxed);
        return Ok(1);
    }
    let engine = caller.engine().clone();
    INST.with(|cell| -> Result<i32> {
        let mut slot = cell.borrow_mut();
        s.lock();
        // Started under the lock: an instance's start runs on the one door
        // stack and context.
        if slot.as_ref().is_none_or(|i| i.id != s.id) {
            match instantiate(&engine, &s) {
                Ok(i) => *slot = Some(i),
                Err(e) => {
                    s.unlock();
                    return Err(e);
                }
            }
        }
        let inst = slot.as_mut().unwrap();
        let sb = s.mem.bytes();
        let b0 = buf as usize;
        sb[b0..b0 + 64].copy_from_slice(&cm.bytes()[req as usize..req as usize + 64]);
        let mut at = b0 + 64;
        for k in 0..data_count {
            let ptr = rd(req + 72 + 8 * k) as usize;
            let size = rd(req + 76 + 8 * k) as usize;
            sb[at..at + size].copy_from_slice(&cm.bytes()[ptr..ptr + size]);
            at += size;
        }
        let _ = inst.sp.set(&mut inst.store, Val::I32(s.get(area::STACK_TOP) as i32));
        let r = inst.dispatch.call(&mut inst.store, (pid, tid));
        let r = match r {
            Ok(r) => r as u32,
            Err(e) => {
                s.unlock();
                *slot = None;
                eprintln!("gex-door: the wineserver trapped in a door call: {e:?}");
                bail!("wineserver door call: {e:?}");
            }
        };
        let ret = if r == !0u32 {
            FALLBACKS.fetch_add(1, Ordering::Relaxed);
            1
        } else if r == !1u32 {
            2
        } else {
            let ro = b0 + r as usize;
            cm.bytes()[req as usize..req as usize + 64].copy_from_slice(&sb[ro..ro + 64]);
            let size = (u32::from_le_bytes(sb[ro + 4..ro + 8].try_into().unwrap())).min(reply_max) as usize;
            if size != 0 && reply_data != 0 {
                cm.bytes()[reply_data as usize..reply_data as usize + size].copy_from_slice(&sb[ro + 64..ro + 64 + size]);
            }
            CALLS.fetch_add(1, Ordering::Relaxed);
            0
        };
        let kick = s.swap(area::KICK_NEEDED, 0) != 0;
        s.unlock();
        if kick {
            s.kick();
        }
        Ok(ret)
    })
}
