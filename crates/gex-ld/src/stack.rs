//! Building the initial process stack.
//!
//! The layout is the one the aarch64 Linux kernel hands to a fresh process,
//! because that is what every libc's `_start` expects to find at `sp`:
//!
//! ```text
//!   sp -> argc
//!         argv[0] .. argv[argc-1], NULL
//!         envp[0] .. envp[n-1],    NULL
//!         auxv pairs .., AT_NULL pair
//!         (strings and the AT_RANDOM block, above)
//! ```

use crate::elf::Image;

/// Auxiliary-vector tags, named as in `<elf.h>`.
pub mod auxv {
    pub const AT_NULL: u64 = 0;
    pub const AT_IGNORE: u64 = 1;
    pub const AT_EXECFD: u64 = 2;
    pub const AT_PHDR: u64 = 3;
    pub const AT_PHENT: u64 = 4;
    pub const AT_PHNUM: u64 = 5;
    pub const AT_PAGESZ: u64 = 6;
    pub const AT_BASE: u64 = 7;
    pub const AT_FLAGS: u64 = 8;
    pub const AT_ENTRY: u64 = 9;
    pub const AT_NOTELF: u64 = 10;
    pub const AT_UID: u64 = 11;
    pub const AT_EUID: u64 = 12;
    pub const AT_GID: u64 = 13;
    pub const AT_EGID: u64 = 14;
    pub const AT_PLATFORM: u64 = 15;
    pub const AT_HWCAP: u64 = 16;
    pub const AT_CLKTCK: u64 = 17;
    /// PowerPC's cache-block sizes, which glibc's `memset` reads to use `dcbz`.
    pub const AT_DCACHEBSIZE: u64 = 19;
    pub const AT_ICACHEBSIZE: u64 = 20;
    pub const AT_UCACHEBSIZE: u64 = 21;
    pub const AT_SECURE: u64 = 23;
    pub const AT_RANDOM: u64 = 25;
    pub const AT_HWCAP2: u64 = 26;
    pub const AT_EXECFN: u64 = 31;
    pub const AT_SYSINFO_EHDR: u64 = 33;
    pub const AT_MINSIGSTKSZ: u64 = 51;
}

/// The hardware capabilities gex advertises.
///
/// Deterministic by design: because every gex process sees the same value,
/// `gexc` can pre-run ifunc resolvers at build time and prebind the winner
/// (PLAN §6.4). The set is ARMv8.0 plus the extensions the lifter implements.
pub const HWCAP: u64 = if gex_abi::PPC {
    // PPC_FEATURE_32 | PPC_FEATURE_HAS_FPU | PPC_FEATURE_HAS_MMU: a 750-class
    // CPU, and no ALTIVEC, so a libc that dispatches on it never picks a
    // vector routine (docs/ppc.md §5).
    0x8000_0000 | 0x0800_0000 | 0x0400_0000
} else if gex_abi::X86 {
    // x86-64: the CPUID.1:EDX feature word. FPU, TSC, CX8, CMOV, CLFSH,
    // FXSR, SSE, SSE2 -- the x86-64 baseline less MMX, which the lifter does
    // not implement; what the `cpuid` host handler reports too (they must
    // agree: glibc's resolvers read both).
    (1 << 0) | (1 << 4) | (1 << 8) | (1 << 15) | (1 << 19) | (1 << 24) | (1 << 25) | (1 << 26)
} else {
    const HWCAP_FP: u64 = 1 << 0;
    const HWCAP_ASIMD: u64 = 1 << 1;
    const HWCAP_CPUID: u64 = 1 << 11;
    const HWCAP_ATOMICS: u64 = 1 << 8;
    HWCAP_FP | HWCAP_ASIMD | HWCAP_CPUID | HWCAP_ATOMICS
};

/// Process attributes the auxiliary vector reports.
///
/// These must agree with what the corresponding syscalls return: musl reads
/// `AT_UID`/`AT_EUID` at startup to decide whether the process is running
/// set-uid, and would reach a different conclusion from `getuid` if the two
/// disagreed.
#[derive(Clone, Copy, Debug)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
    /// Load bias of the dynamic linker, or zero for a static program.
    ///
    /// The interpreter reads `AT_BASE` to locate its own relocations; a
    /// static image must see zero, which is how libc decides no interpreter
    /// ran.
    pub interp_base: u64,
    /// `AT_ENTRY` override, or zero for the real entry. glibc's
    /// `_dl_start_user` branches here once linking is done, so pointing it at a
    /// stub is how the host gets control before any program code runs (see
    /// `SNAP_STUB` in the browser shim).
    pub entry_hook: u64,
}

impl Default for Identity {
    fn default() -> Self {
        Identity {
            uid: 1000,
            gid: 1000,
            interp_base: 0,
            entry_hook: 0,
        }
    }
}

/// How much room the strings are given below the stack top.
///
/// The layout is anchored, not packed: the blob starts at a fixed address so
/// that `argc`, the vectors and `AT_RANDOM` land at the same addresses
/// regardless of argument length. That stability is what lets a snapshot of a
/// linked process (whose libc stored those addresses) be reused. Falls back to
/// packing when the blob does not fit.
pub const BLOB_RESERVE: u64 = 32 * 1024;

/// Room for `argc`, the two pointer arrays and the auxiliary vector: 1024
/// words, which is more entries than any real command line has.
pub const VECTOR_RESERVE: u64 = 8 * 1024;

/// Where the initial stack ended up.
#[derive(Clone, Debug)]
pub struct StackLayout {
    /// Value the guest's `sp` must hold at entry.
    pub sp: u64,
    /// Lowest address written.
    pub low: u64,
    /// The bytes to place, starting at [`StackLayout::low`].
    pub bytes: Vec<u8>,
}

/// Construct the initial stack for a process.
///
/// `stack_top` is the highest address the stack may occupy; the result grows
/// downward from just below it.
pub fn build_initial_stack(
    image: &Image,
    stack_top: u64,
    args: &[String],
    env: &[String],
    random: [u8; 16],
    id: Identity,
) -> StackLayout {
    // Lay the variable-length data out first, from the top down, so that the
    // fixed-size vectors below can point at it.
    //
    // `AT_RANDOM` and `AT_PLATFORM` come first, at fixed offsets, because the
    // linker keeps their addresses; the strings follow and may land wherever
    // their own lengths put them, because nothing keeps those.
    let mut blob: Vec<u8> = Vec::new();
    let random_off = blob.len();
    blob.extend_from_slice(&random);
    let platform_off = blob.len();
    // The word size of the vectors below, and the name a 32-bit program
    // expects to find in AT_PLATFORM.
    let ws: u64 = if image.class32 { 4 } else { 8 };
    let platform: &str = if gex_abi::PPC {
        "ppc750"
    } else if image.class32 {
        "i686"
    } else {
        gex_abi::ARCH_NAME
    };
    blob.extend_from_slice(platform.as_bytes());
    blob.push(0);
    let mut offsets_argv = Vec::with_capacity(args.len());
    let mut offsets_env = Vec::with_capacity(env.len());

    for a in args {
        offsets_argv.push(blob.len());
        blob.extend_from_slice(a.as_bytes());
        blob.push(0);
    }
    for e in env {
        offsets_env.push(blob.len());
        blob.extend_from_slice(e.as_bytes());
        blob.push(0);
    }
    let execfn_off = offsets_argv.first().copied().unwrap_or(0);
    while !blob.len().is_multiple_of(8) {
        blob.push(0);
    }

    // Anchored when it fits, packed when it does not. See `BLOB_RESERVE`.
    let fits = blob.len() as u64 <= BLOB_RESERVE;
    let blob_addr = if fits {
        stack_top - BLOB_RESERVE
    } else {
        (stack_top - blob.len() as u64) & !0xf
    };

    let mut auxv_pairs: Vec<(u64, u64)> = vec![
        (auxv::AT_PHDR, image.phdr_vaddr),
        (auxv::AT_PHENT, image.phentsize),
        (auxv::AT_PHNUM, image.phnum),
        (auxv::AT_PAGESZ, 4096),
        (auxv::AT_BASE, id.interp_base),
        (auxv::AT_FLAGS, 0),
        (
            auxv::AT_ENTRY,
            if id.entry_hook != 0 { id.entry_hook } else { image.entry },
        ),
        (auxv::AT_UID, id.uid as u64),
        (auxv::AT_EUID, id.uid as u64),
        (auxv::AT_GID, id.gid as u64),
        (auxv::AT_EGID, id.gid as u64),
        (auxv::AT_HWCAP, HWCAP),
        (auxv::AT_HWCAP2, 0),
        (auxv::AT_CLKTCK, 100),
        (auxv::AT_SECURE, 0),
        (auxv::AT_RANDOM, blob_addr + random_off as u64),
        (auxv::AT_PLATFORM, blob_addr + platform_off as u64),
        (auxv::AT_EXECFN, blob_addr + execfn_off as u64),
        (auxv::AT_MINSIGSTKSZ, 4096),
        (auxv::AT_NULL, 0),
    ];
    if gex_abi::PPC {
        // 32-byte cache blocks, as a 750's; `dcbz` zeroes this many.
        let at = auxv_pairs.len() - 1;
        for tag in [auxv::AT_DCACHEBSIZE, auxv::AT_ICACHEBSIZE, auxv::AT_UCACHEBSIZE] {
            auxv_pairs.insert(at, (tag, 32));
        }
    }

    // words = argc + argv + NULL + envp + NULL + 2*auxv
    let n_words = 1 + args.len() + 1 + env.len() + 1 + 2 * auxv_pairs.len();
    let vector_bytes = n_words as u64 * ws;
    // `sp` must be 16-byte aligned at entry, and stays put when it can.
    let sp = if fits && vector_bytes <= VECTOR_RESERVE {
        blob_addr - VECTOR_RESERVE
    } else {
        (blob_addr - vector_bytes) & !0xf
    };

    let mut words: Vec<u64> = Vec::with_capacity(n_words);
    words.push(args.len() as u64);
    for off in &offsets_argv {
        words.push(blob_addr + *off as u64);
    }
    words.push(0);
    for off in &offsets_env {
        words.push(blob_addr + *off as u64);
    }
    words.push(0);
    for (k, v) in &auxv_pairs {
        words.push(*k);
        words.push(*v);
    }

    // Emit one contiguous buffer covering [sp, stack_top).
    let low = sp;
    let mut bytes = vec![0u8; (blob_addr + blob.len() as u64 - low) as usize];
    for (i, w) in words.iter().enumerate() {
        let off = i * ws as usize;
        // Guest byte order: big-endian words on PowerPC.
        use gex_abi::GuestEndian;
        if image.class32 {
            bytes[off..off + 4].copy_from_slice(&(*w as u32).to_ge_bytes());
        } else {
            bytes[off..off + 8].copy_from_slice(&w.to_ge_bytes());
        }
    }
    let blob_off = (blob_addr - low) as usize;
    bytes[blob_off..blob_off + blob.len()].copy_from_slice(&blob);

    StackLayout { sp, low, bytes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::{Image, Segment};

    fn dummy_image() -> Image {
        Image {
            class32: false,
            soname: None,
            needed: Vec::new(),
            runpath: Vec::new(),
            bias: 0,
            entry: 0x400000,
            code: vec![(0x400000, 0x400010)],
            segments: vec![Segment {
                vaddr: 0x400000,
                bytes: vec![0; 16],
                mem_size: 16,
                readable: true,
                writable: false,
                executable: true,
            }],
            phdr_vaddr: 0x400040,
            phentsize: 56,
            phnum: 4,
            brk: 0x401000,
            text_range: (0x400000, 0x400010),
            tls: None,
            symbols: Vec::new(),
            named_relocs: Vec::new(),
            irelative: Vec::new(),
            relocs: Vec::new(),
            init_array: Vec::new(),
            dynsyms: Vec::new(),
            pie: false,
            interp: None,
        }
    }

    fn read_u64(l: &StackLayout, addr: u64) -> u64 {
        let off = (addr - l.low) as usize;
        u64::from_le_bytes(l.bytes[off..off + 8].try_into().unwrap())
    }

    #[test]
    fn stack_is_sixteen_byte_aligned() {
        let l = build_initial_stack(
            &dummy_image(),
            0x8000_0000,
            &["prog".into(), "arg".into()],
            &["PATH=/bin".into()],
            [7; 16],
            Identity::default(),
        );
        assert_eq!(l.sp % 16, 0);
    }

    #[test]
    fn argc_argv_envp_and_auxv_are_reachable() {
        // An AArch64/x86-64 fixture (64-bit little-endian ELF or A64 code).
        if gex_abi::PPC {
            return;
        }
        let args = vec!["prog".to_string(), "one".to_string()];
        let env = vec!["A=1".to_string()];
        let l = build_initial_stack(
            &dummy_image(),
            0x8000_0000,
            &args,
            &env,
            [3; 16],
            Identity::default(),
        );

        assert_eq!(read_u64(&l, l.sp), 2, "argc");
        // argv[0] points at "prog\0".
        let argv0 = read_u64(&l, l.sp + 8);
        let off = (argv0 - l.low) as usize;
        assert_eq!(&l.bytes[off..off + 5], b"prog\0");
        // argv is NULL-terminated, then envp, then NULL.
        assert_eq!(read_u64(&l, l.sp + 8 + 2 * 8), 0);
        let envp0 = read_u64(&l, l.sp + 8 + 3 * 8);
        let eoff = (envp0 - l.low) as usize;
        assert_eq!(&l.bytes[eoff..eoff + 4], b"A=1\0");
        assert_eq!(read_u64(&l, l.sp + 8 + 4 * 8), 0);

        // The auxv follows and must end with AT_NULL.
        let auxv_start = l.sp + 8 + 5 * 8;
        let mut at = auxv_start;
        let mut seen_random = None;
        loop {
            let key = read_u64(&l, at);
            let val = read_u64(&l, at + 8);
            if key == auxv::AT_RANDOM {
                seen_random = Some(val);
            }
            if key == auxv::AT_NULL {
                break;
            }
            at += 16;
            assert!(at < l.low + l.bytes.len() as u64, "auxv not terminated");
        }
        let random = seen_random.expect("AT_RANDOM present");
        let roff = (random - l.low) as usize;
        assert_eq!(&l.bytes[roff..roff + 16], &[3u8; 16]);
    }

    #[test]
    fn two_command_lines_put_argv_and_at_random_in_the_same_place() {
        let at = |args: &[String], env: &[String]| {
            let l = build_initial_stack(&dummy_image(), 0x8000_0000, args, env, [3; 16], Identity::default());
            let mut random = 0;
            let mut a = l.sp + 8 + (args.len() as u64 + 1 + env.len() as u64 + 1) * 8;
            loop {
                let (k, v) = (read_u64(&l, a), read_u64(&l, a + 8));
                if k == auxv::AT_RANDOM {
                    random = v;
                }
                if k == auxv::AT_NULL {
                    break;
                }
                a += 16;
            }
            (l.sp, random)
        };
        let short = at(&["ls".into()], &["PATH=/bin".into()]);
        let long = at(
            &["ls".into(), "-la".into(), "/usr/share/doc".into()],
            &["PATH=/bin".into(), "HOME=/root".into()],
        );
        assert_eq!(short.0, long.0, "sp");
        assert_eq!(short.1, long.1, "AT_RANDOM");
    }

    #[test]
    fn everything_lies_within_the_emitted_buffer() {
        let l = build_initial_stack(
            &dummy_image(),
            0x8000_0000,
            &["x".repeat(300)],
            &["Y=".to_string() + &"z".repeat(500)],
            [0; 16],
            Identity::default(),
        );
        assert!(l.low + l.bytes.len() as u64 <= 0x8000_0000);
        assert!(l.sp >= l.low);
    }
}
