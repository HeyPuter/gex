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
pub const HWCAP: u64 = {
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
}

impl Default for Identity {
    fn default() -> Self {
        Identity {
            uid: 1000,
            gid: 1000,
            interp_base: 0,
        }
    }
}

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
    let mut blob: Vec<u8> = Vec::new();
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
    // AT_RANDOM must point at 16 readable bytes.
    while !blob.len().is_multiple_of(8) {
        blob.push(0);
    }
    let random_off = blob.len();
    blob.extend_from_slice(&random);
    // "aarch64" for AT_PLATFORM.
    let platform_off = blob.len();
    blob.extend_from_slice(b"aarch64\0");

    // Place the blob directly below the stack top, 16-byte aligned.
    let blob_addr = (stack_top - blob.len() as u64) & !0xf;

    let auxv_pairs: Vec<(u64, u64)> = vec![
        (auxv::AT_PHDR, image.phdr_vaddr),
        (auxv::AT_PHENT, image.phentsize),
        (auxv::AT_PHNUM, image.phnum),
        (auxv::AT_PAGESZ, 4096),
        (auxv::AT_BASE, id.interp_base),
        (auxv::AT_FLAGS, 0),
        (auxv::AT_ENTRY, image.entry),
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

    // words = argc + argv + NULL + envp + NULL + 2*auxv
    let n_words = 1 + args.len() + 1 + env.len() + 1 + 2 * auxv_pairs.len();
    let vector_bytes = n_words * 8;
    // `sp` must be 16-byte aligned at entry.
    let sp = (blob_addr - vector_bytes as u64) & !0xf;

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
        let off = i * 8;
        bytes[off..off + 8].copy_from_slice(&w.to_le_bytes());
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
