//! ELF images: mapping linker64 and the program it starts, reading symbol tables, and booting the guest's dynamic
//! linker. After that the linker loads everything (`dlopen`), with the guest's own `mmap`s.

use crate::{call, cpu, vm};
use gex_abi::ctx;

const PT_LOAD: u32 = 1;
const PT_PHDR: u32 = 6;

/// A parsed ELF64 file.
pub struct Elf<'a> {
    pub b: &'a [u8],
}

pub struct Phdr {
    pub kind: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

impl<'a> Elf<'a> {
    pub fn parse(b: &'a [u8]) -> Result<Elf<'a>, String> {
        if b.len() < 64 || &b[..4] != b"\x7fELF" || b[4] != 2 || b[5] != 1 {
            return Err("not a little-endian ELF64 file".into());
        }
        let e = Elf { b };
        if e.u16(18) != 183 {
            return Err(format!("machine {} is not AArch64", e.u16(18)));
        }
        if e.u16(16) != 3 {
            return Err("not position-independent (ET_DYN)".into());
        }
        Ok(e)
    }

    fn u16(&self, o: usize) -> u16 {
        u16::from_le_bytes(self.b[o..o + 2].try_into().unwrap())
    }
    fn u32(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.b[o..o + 4].try_into().unwrap())
    }
    fn u64(&self, o: usize) -> u64 {
        u64::from_le_bytes(self.b[o..o + 8].try_into().unwrap())
    }

    pub fn entry(&self) -> u64 {
        self.u64(24)
    }

    pub fn phoff(&self) -> u64 {
        self.u64(32)
    }

    pub fn phdrs(&self) -> Vec<Phdr> {
        let (off, n, sz) = (self.phoff() as usize, self.u16(56) as usize, self.u16(54) as usize);
        (0..n)
            .map(|i| {
                let o = off + i * sz;
                Phdr {
                    kind: self.u32(o),
                    flags: self.u32(o + 4),
                    offset: self.u64(o + 8),
                    vaddr: self.u64(o + 16),
                    filesz: self.u64(o + 32),
                    memsz: self.u64(o + 40),
                    align: self.u64(o + 48),
                }
            })
            .collect()
    }

    /// A function's or object's value, from `.symtab` (local symbols too) or `.dynsym` (whose names may differ: linker64's
    /// `.symtab` has every name prefixed `__dl_`).
    pub fn symbol(&self, name: &str) -> Option<u64> {
        self.symbols().into_iter().find(|(n, _)| *n == name.as_bytes()).map(|(_, v)| v)
    }

    /// Every defined symbol, `.symtab`'s then `.dynsym`'s: name and value.
    pub fn symbols(&self) -> Vec<(&'a [u8], u64)> {
        let (shoff, shn, shsz) = (self.u64(40) as usize, self.u16(60) as usize, self.u16(58) as usize);
        let sec = |i: usize| shoff + i * shsz;
        let mut out = Vec::new();
        for want in [2u32, 11] {
            // SHT_SYMTAB, then SHT_DYNSYM
            for i in 0..shn {
                let s = sec(i);
                if s + shsz > self.b.len() || self.u32(s + 4) != want {
                    continue;
                }
                let (off, size, link) = (self.u64(s + 24) as usize, self.u64(s + 32) as usize, self.u32(s + 40) as usize);
                let strtab = self.u64(sec(link) + 24) as usize;
                for k in 0..size / 24 {
                    let e = off + k * 24;
                    let (name, shndx, value) = (self.u32(e) as usize, self.u16(e + 6), self.u64(e + 8));
                    if shndx == 0 || name == 0 {
                        continue;
                    }
                    let n = &self.b[strtab + name..];
                    let n = &n[..n.iter().position(|&c| c == 0).unwrap_or(0)];
                    out.push((n, value));
                }
            }
        }
        out
    }
}

/// Where an image went.
pub struct Mapped {
    pub bias: u64,
    pub entry: u64,
    pub phdr: u64,
    pub phnum: u64,
}

/// Map an image's loadable segments into a block of their own, with their protections.
pub fn map(b: &[u8]) -> Result<Mapped, String> {
    let e = Elf::parse(b)?;
    let ph = e.phdrs();
    let loads: Vec<&Phdr> = ph.iter().filter(|p| p.kind == PT_LOAD).collect();
    let lo = loads.iter().map(|p| p.vaddr & !(vm::PAGE - 1)).min().ok_or("no PT_LOAD")?;
    let hi = loads.iter().map(|p| p.vaddr + p.memsz).max().unwrap();
    let align = loads.iter().map(|p| p.align).max().unwrap().max(vm::PAGE);
    let base = vm::reserve(hi - lo, align).ok_or("out of memory for an image")?;
    let bias = base - lo;
    for p in &loads {
        let src = b.get(p.offset as usize..(p.offset + p.filesz) as usize).ok_or("a segment past the file's end")?;
        // SAFETY: inside the block just reserved.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), (bias + p.vaddr) as usize as *mut u8, src.len()) };
    }
    for p in &loads {
        let (s, t) = ((bias + p.vaddr) & !(vm::PAGE - 1), bias + p.vaddr + p.memsz);
        // PF_X 1, PF_W 2, PF_R 4 -> PROT_EXEC 4, PROT_WRITE 2, PROT_READ 1.
        let prot = (p.flags & 1) << 2 | (p.flags & 2) | (p.flags & 4) >> 2;
        vm::mprotect(s, t - s, prot);
    }
    let phdr = match ph.iter().find(|p| p.kind == PT_PHDR) {
        Some(p) => bias + p.vaddr,
        None => {
            let l = loads.iter().find(|p| p.offset <= e.phoff() && e.phoff() < p.offset + p.filesz).ok_or("no PT_PHDR")?;
            bias + l.vaddr + (e.phoff() - l.offset)
        }
    };
    Ok(Mapped { bias, entry: bias + e.entry(), phdr, phnum: ph.len() as u64 })
}

/// A whole file, by the host's libc.
pub fn read_file(path: &str) -> Result<Vec<u8>, String> {
    #[cfg(target_arch = "wasm32")]
    {
        extern "C" {
            fn open(path: *const u8, flags: i32, ...) -> i32;
            fn read(fd: i32, p: *mut u8, n: usize) -> isize;
            fn close(fd: i32) -> i32;
        }
        let c = std::ffi::CString::new(path).map_err(|_| "a NUL in the path")?;
        // SAFETY: plain libc calls into a buffer of ours.
        let fd = unsafe { open(c.as_ptr() as *const u8, 0) };
        if fd < 0 {
            return Err(format!("can't open {path}"));
        }
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = unsafe { read(fd, buf.as_mut_ptr(), buf.len()) };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        unsafe { close(fd) };
        Ok(out)
    }
    #[cfg(not(target_arch = "wasm32"))]
    std::fs::read(path).map_err(|e| format!("{path}: {e}"))
}

/// The linker's entry points the host calls, once booted.
pub struct Loader {
    pub dlopen: u64,
    pub dlsym: u64,
    pub dlerror: u64,
}

pub static LOADER: std::sync::OnceLock<Loader> = std::sync::OnceLock::new();

mod at {
    pub const NULL: u64 = 0;
    pub const PHDR: u64 = 3;
    pub const PHENT: u64 = 4;
    pub const PHNUM: u64 = 5;
    pub const PAGESZ: u64 = 6;
    pub const BASE: u64 = 7;
    pub const FLAGS: u64 = 8;
    pub const ENTRY: u64 = 9;
    pub const UID: u64 = 11;
    pub const EUID: u64 = 12;
    pub const GID: u64 = 13;
    pub const EGID: u64 = 14;
    pub const PLATFORM: u64 = 15;
    pub const HWCAP: u64 = 16;
    pub const SECURE: u64 = 23;
    pub const RANDOM: u64 = 25;
    pub const HWCAP2: u64 = 26;
    pub const EXECFN: u64 = 31;
}

/// The executable's path, for `/proc/self/exe`.
pub static EXE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Start the guest: map `linker` (bionic's linker64) and `exe` (a program whose only purpose is to have libc and its
/// dependencies loaded), and run the linker on this thread, the way the kernel starts a program, until it jumps to
/// the program's entry -- which is a return pad, so that is where the call returns. This thread is the guest's main
/// thread after, with libc initialized.
pub fn boot(linker: &str, exe: &str) -> Result<(), String> {
    let lb = read_file(linker)?;
    let le = map(&lb)?;
    let eb = read_file(exe)?;
    let ee = map(&eb)?;
    let _ = EXE.set(exe.to_string());
    crate::host::log(&format!("gex-embed: {linker} at {:#x}, {exe} at {:#x}\n", le.bias, ee.bias));
    crate::thread::become_main();
    let l = Elf::parse(&lb)?;
    let sym = |n: &str| l.symbol(n).map(|v| le.bias + v).ok_or_else(|| format!("{linker} has no {n}"));
    let loader = Loader { dlopen: sym("__loader_dlopen")?, dlsym: sym("__loader_dlsym")?, dlerror: sym("__loader_dlerror")? };
    let mut sp_at_entry = 0;
    call::run(
        le.entry,
        |_, pad, top| {
            // The initial stack, from the top down: strings, AT_RANDOM's bytes, then auxv, envp, argv and argc.
            let mut sp = top;
            let mut put = |bytes: &[u8]| {
                sp = (sp - bytes.len() as u64) & !15;
                // SAFETY: this thread's guest stack.
                unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), sp as usize as *mut u8, bytes.len()) };
                sp
            };
            let argv0 = put(format!("{exe}\0").as_bytes());
            let platform = put(b"aarch64\0");
            let mut random = [0u8; 16];
            crate::host::random(&mut random);
            let random = put(&random);
            let auxv = [
                (at::PHDR, ee.phdr),
                (at::PHENT, 56),
                (at::PHNUM, ee.phnum),
                (at::PAGESZ, vm::PAGE),
                (at::BASE, le.bias),
                (at::FLAGS, 0),
                (at::ENTRY, pad),
                (at::UID, crate::sys::UID),
                (at::EUID, crate::sys::UID),
                (at::GID, crate::sys::UID),
                (at::EGID, crate::sys::UID),
                (at::SECURE, 0),
                (at::RANDOM, random),
                (at::HWCAP, gex_ld::stack::HWCAP),
                (at::HWCAP2, 0),
                (at::EXECFN, argv0),
                (at::PLATFORM, platform),
                (at::NULL, 0),
            ];
            let mut words: Vec<u64> = vec![1, argv0, 0, 0];
            for (k, v) in auxv {
                words.extend([k, v]);
            }
            let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            let sp = put(&bytes);
            sp_at_entry = sp;
            sp
        },
        |_| {},
    );
    // libc keeps the auxv and argv where they are (getauxval reads them), so this thread's calls start below them.
    crate::thread::current().stack_top = (sp_at_entry - 256) & !15;
    if cpu::get_u64(crate::thread::current().ctx, ctx::TPIDR) == 0 {
        return Err("the linker returned without a thread pointer".into());
    }
    LOADER.set(loader).map_err(|_| "booted twice")?;
    Ok(())
}
