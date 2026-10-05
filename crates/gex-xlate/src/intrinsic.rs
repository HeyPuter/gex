//! Library functions the backend serves itself, instead of translating them.
//!
//! glibc's `memcpy` is a hand-written NEON loop; lifting it is correct but
//! slow, where wasm's `memory.copy` does the same work in one instruction.
//! Decompression and hashing (`apt update`, `xz`) spend most of their time
//! here. So, as box64 and FEX do, the function is recognised by name -- from
//! the symbol table discovery already reads -- and bound to a host builtin.
//!
//! Safety against substituting the wrong function: a symbol here is `STT_FUNC`
//! (`elf::load` keeps no other kind), naming a definition in this object's
//! text. aarch64 glibc resolves `memcpy` through an IFUNC, whose
//! `STT_GNU_IFUNC` resolver symbol never reaches this table; what reaches it
//! is the real `__memcpy_generic` and siblings the resolver returns.

use gex_ir::*;

/// A library function with a wasm instruction behind it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Intrinsic {
    /// `void *memcpy(void *dst, const void *src, size_t n)`.
    ///
    /// `memory.copy` is defined as if the bytes went through a temporary, a
    /// superset of `memcpy`'s (non-overlapping) contract.
    MemCpy,
    /// `void *memmove(void *dst, const void *src, size_t n)`.
    ///
    /// Emitted identically to `MemCpy` (`memory.copy` is already memmove-safe),
    /// kept a distinct variant only so the two can be enabled/disabled apart
    /// when bisecting a corruption to one or the other.
    MemMove,
    /// `void *mempcpy(void *dst, const void *src, size_t n)`, which differs
    /// from `memcpy` only in returning the *end* of the copy.
    MemPCpy,
    /// `void *memset(void *dst, int c, size_t n)`.
    MemSet,
    /// `int memcmp(const void *a, const void *b, size_t n)`.
    ///
    /// `bcmp` and `__memcmpeq` only ask whether the result is zero, so this
    /// stronger contract satisfies them too. The odd one out: no wasm
    /// instruction backs it, so the backend emits a loop (see `recognise`).
    MemCmp,
    /// A user-supplied replacement (`gex-eject`'s library replacement): the
    /// function becomes one host call, `sysreg_write(pc, REPLACE_SYSREG, id)`,
    /// and a return. The host runs replacement `id` (a wasm function the
    /// ejected project was built with) against the context, which reads its
    /// arguments from the guest's argument registers and writes its result
    /// back, and then the block returns to the caller as the function would.
    ///
    /// Never produced by [`recognise`]: an eject build puts it in
    /// [`crate::Options::intrinsics`] at the address of the symbol it
    /// replaces, so every way of reaching that address -- a direct call, a
    /// PLT stub through its resolved GOT slot, a function pointer -- runs the
    /// replacement.
    Replace(u32),
    /// A user-supplied override of the whole function (`gex-eject`'s "entry"
    /// ABI, which `gexc eject decompile` writes): the host runs it, and it
    /// does everything the function would, its return included, leaving the
    /// context's PC where the guest goes next. The block is the host call and
    /// a dispatch on that PC.
    Override(u32),
}

/// The `sysreg_write` register number a [`Intrinsic::Replace`] block passes.
/// Negative like gex's other private numbers (`-1`..`-10` on AArch64, `-100`
/// and `-101` on x86-64), and far from them. A host that does not know it
/// must fail loudly rather than ignore it: the function it stands for did not
/// run.
pub const REPLACE_SYSREG: i32 = -200;

/// The memcpy/memmove/memset/mempcpy definitions in `image`'s own text, by
/// address (`Symbol::value` is already biased), for `Options::intrinsics`:
/// a plain build and `gexc run` serve them with the host builtins, as closure
/// builds and the browser's runtime translator do, so every path agrees on
/// what those functions are.
///
/// Not for 32-bit (i386) images: `intrinsic::block` reads the 64-bit
/// register arguments and pops an 8-byte return address, while i386 cdecl
/// passes them on the stack.
pub fn image_intrinsics(image: &gex_ld::Image) -> std::collections::HashMap<u64, Intrinsic> {
    if image.class32 {
        return Default::default();
    }
    let (tlo, thi) = image.text_range;
    image
        .symbols
        .iter()
        .filter(|s| !s.ifunc && s.value >= tlo && s.value < thi)
        .filter_map(|s| recognise(&s.name).map(|k| (s.value, k)))
        .collect()
}

/// [`image_intrinsics`] for a program: its executable and its dynamic linker.
pub fn program_intrinsics(program: &gex_ld::Program) -> std::collections::HashMap<u64, Intrinsic> {
    let mut m = image_intrinsics(&program.exe);
    if let Some(i) = &program.interp {
        m.extend(image_intrinsics(i));
    }
    m
}

/// The intrinsic a symbol names, if it names one. Matches both the public name
/// and glibc's private `__memcpy_*` implementations, since on aarch64 the
/// public name is an IFUNC and never appears as a `STT_FUNC` symbol.
pub fn recognise(name: &str) -> Option<Intrinsic> {
    // Two glibc helper families share a stem with the function they help but
    // not its signature, so the shape match below must not reach them:
    // `__memcpy_chk` takes a fourth bounds argument, and `__mempcpy_small`
    // takes the bytes to store rather than a pointer and length. Both are real
    // symbols in Debian's libc.
    if name.ends_with("_chk") || name.ends_with("_small") {
        return None;
    }
    // `__memcpy_generic`, `__memcpy_simd`, ... -- the variant suffix differs
    // by glibc version and target core, so match it by shape, but only under a
    // leading `__` (glibc's private namespace, not some `memcpy_from_page`).
    let stem = match name.strip_prefix("__") {
        Some(rest) => rest.split_once('_').map_or(rest, |(s, _)| s),
        None => name,
    };
    match stem {
        "memcpy" => Some(Intrinsic::MemCpy),
        "memmove" => Some(Intrinsic::MemMove),
        "mempcpy" => Some(Intrinsic::MemPCpy),
        "memset" => Some(Intrinsic::MemSet),
        // `memcmp` is deliberately absent though `Intrinsic::MemCmp` exists
        // and works: measured on perl (a pure key-compare workload) it showed
        // no benefit, because the loop this emits runs a byte at a time while
        // glibc's aarch64 `memcmp` is SIMD -- unlike `memcpy`, there is no
        // single wasm instruction to beat it. Enabling it is adding
        // `"memcmp" | "bcmp" | "memcmpeq"` here, worth doing only with a
        // wider, word-at-a-time lowering.
        _ => None,
    }
}

/// The whole function as one block: read the AAPCS64 arguments (`x0`-`x2`),
/// do it, and return through `x30`. Registers a real implementation would
/// clobber are left alone, which the procedure call standard permits.
pub fn block(addr: u64, which: Intrinsic) -> Block {
    if gex_abi::PPC {
        return block_ppc(addr, which);
    }
    let mut b = Builder::new(addr);
    // The C calling convention's first three argument registers: x0-x2, or
    // rdi/rsi/rdx. `ctx::CALL_ARGS` gives their context offsets.
    let arg = |i: usize| GuestReg::X((gex_abi::ctx::CALL_ARGS[i] / 8) as u8);
    let dst = b.read(arg(0));
    let src = b.read(arg(1));
    let len = b.read(arg(2));
    match which {
        Intrinsic::MemCpy | Intrinsic::MemMove => b.stmt(Stmt::MemCopy { dst, src, len }),
        Intrinsic::MemPCpy => {
            b.stmt(Stmt::MemCopy { dst, src, len });
            // The one difference: it returns where the copy ended.
            let end = b.bin(BinOp::Add, Ty::I64, dst, len);
            b.write(GuestReg::X(0), end);
        }
        // `val` is an `int` whose low byte is the fill; `memory.fill` takes
        // the same and ignores the rest, so no masking is needed here.
        Intrinsic::MemSet => b.stmt(Stmt::MemFill { dst, val: src, len }),
        Intrinsic::MemCmp => {
            // `dst`/`src` here are just this builder's names for x0 and x1,
            // the two buffers.
            let r = b.fresh();
            b.stmt(Stmt::MemCmp {
                dst: r,
                a: dst,
                b: src,
                len,
            });
            // Result is an `int` in w0; sign-extend so a caller reading x0 as
            // a signed 64-bit value sees the sign.
            let wide = b.extend(r, true);
            b.write(GuestReg::X(0), wide);
        }
        Intrinsic::Replace(id) | Intrinsic::Override(id) => {
            let pc = b.const_i64(addr);
            let reg = b.const_i32(REPLACE_SYSREG as u32);
            let id = b.const_i64(u64::from(id));
            b.stmt(Stmt::HostCall {
                func: HostFunc::SysRegWrite,
                args: vec![pc, reg, id],
            });
        }
    }
    // `memcpy`, `memmove` and `memset` return their first argument. On
    // aarch64 that is x0 already; on x86-64 it arrived in rdi and must be
    // placed in rax, or every `strcpy`-shaped caller that uses the return
    // value (`g_strdup`, `stpcpy`, ...) reads whatever rax held.
    if gex_abi::X86 && matches!(which, Intrinsic::MemCpy | Intrinsic::MemMove | Intrinsic::MemSet) {
        b.write(GuestReg::X(0), dst);
    }
    // An override returned (or left) itself: go where it says.
    if let Intrinsic::Override(_) = which {
        let target = b.read(GuestReg::Pc);
        return b.finish(Term::Indirect {
            target,
            is_ret: true,
        });
    }
    // Return: through the link register, or by popping the return address.
    let target = if gex_abi::X86 {
        let sp = b.read(GuestReg::X(4));
        let ret = b.def(gex_ir::Expr::Load {
            addr: sp,
            offset: 0,
            sz: gex_ir::MemSz::B8,
            signed: false,
            ty: Ty::I64,
            order: gex_ir::Order::Relaxed,
        });
        let eight = b.const_i64(8);
        let nsp = b.bin(BinOp::Add, Ty::I64, sp, eight);
        b.write(GuestReg::X(4), nsp);
        ret
    } else {
        b.read(GuestReg::X(30))
    };
    b.finish(Term::Indirect {
        target,
        is_ret: true,
    })
}

/// [`block`] for 32-bit PowerPC: the SysV arguments in r3-r5 (32-bit), the
/// result in r3, the return through LR. `memcpy`, `memmove` and `memset`
/// return their first argument, which r3 already holds.
fn block_ppc(addr: u64, which: Intrinsic) -> Block {
    let mut b = Builder::new(addr);
    let dst = b.read(GuestReg::R(3));
    let src = b.read(GuestReg::R(4));
    let len32 = b.read(GuestReg::R(5));
    // The bulk statements take their length and fill value as `I64`s.
    let len = b.extend(len32, false);
    match which {
        Intrinsic::MemCpy | Intrinsic::MemMove => b.stmt(Stmt::MemCopy { dst, src, len }),
        Intrinsic::MemPCpy => {
            b.stmt(Stmt::MemCopy { dst, src, len });
            let end = b.bin(BinOp::Add, Ty::I32, dst, len32);
            b.write(GuestReg::R(3), end);
        }
        Intrinsic::MemSet => {
            let val = b.extend(src, false);
            b.stmt(Stmt::MemFill { dst, val, len })
        }
        Intrinsic::MemCmp => {
            let r = b.fresh();
            b.stmt(Stmt::MemCmp {
                dst: r,
                a: dst,
                b: src,
                len,
            });
            b.write(GuestReg::R(3), r);
        }
        // gex-eject's replacements speak the AArch64 and x86-64 calling
        // conventions only; on PowerPC the function did not run, so say so.
        Intrinsic::Replace(_) | Intrinsic::Override(_) => {
            return b.finish(Term::Illegal { raw: 0, pc: addr })
        }
    }
    let lr = b.read(GuestReg::Lr);
    let mask = b.const_i32(!3);
    let lr = b.bin(BinOp::And, Ty::I32, lr, mask);
    let target = b.extend(lr, false);
    b.finish(Term::Indirect {
        target,
        is_ret: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_glibc_private_names() {
        assert_eq!(recognise("memcpy"), Some(Intrinsic::MemCpy));
        assert_eq!(recognise("memmove"), Some(Intrinsic::MemMove));
        assert_eq!(recognise("__memcpy_generic"), Some(Intrinsic::MemCpy));
        assert_eq!(recognise("__memcpy_simd"), Some(Intrinsic::MemCpy));
        assert_eq!(recognise("__memmove_thunderx2"), Some(Intrinsic::MemMove));
        assert_eq!(recognise("__memset_a64fx"), Some(Intrinsic::MemSet));
        assert_eq!(recognise("mempcpy"), Some(Intrinsic::MemPCpy));
    }

    #[test]
    fn leaves_everything_else_alone() {
        // A variant suffix counts only inside glibc's `__` namespace, so
        // another library's similarly-named helper is left to be translated.
        assert_eq!(recognise("memcpy_from_page"), None);
        assert_eq!(recognise("__memcpy_chk"), None);
        assert_eq!(recognise("__memset_chk"), None);
        // Both of these are in Debian's libc and neither takes a pointer and
        // a length.
        assert_eq!(recognise("__mempcpy_small"), None);
        assert_eq!(recognise("memchr"), None);
        assert_eq!(recognise("memory_pool_init"), None);
        assert_eq!(recognise("my_memcpy"), None);
        assert_eq!(recognise(""), None);
    }

    #[test]
    fn memcpy_is_one_copy_and_a_return() {
        let b = block(0x1000, Intrinsic::MemCpy);
        assert_eq!(b.start, 0x1000);
        assert!(matches!(b.term, Term::Indirect { is_ret: true, .. }));
        assert_eq!(
            b.stmts
                .iter()
                .filter(|s| matches!(s, Stmt::MemCopy { .. }))
                .count(),
            1
        );
        // Nothing is written but the values it reads: a `memcpy` that
        // returned its first argument in x0 already has it there. (On x86-64
        // the return register differs from the argument's, so it is written.)
        if !gex_abi::X86 {
            assert!(!b.stmts.iter().any(|s| matches!(s, Stmt::WriteReg { .. })));
        }
    }

    #[test]
    fn memcmp_is_built_but_not_recognised() {
        // Measured and not enabled -- see the note in `recognise`.
        assert_eq!(recognise("memcmp"), None);
        assert_eq!(recognise("bcmp"), None);
    }

    #[test]
    fn memcmp_writes_its_answer_to_x0() {
        // An AArch64/x86-64 fixture (64-bit little-endian ELF or A64 code).
        if gex_abi::PPC {
            return;
        }
        let b = block(0x1000, Intrinsic::MemCmp);
        assert!(b.stmts.iter().any(|s| matches!(s, Stmt::MemCmp { .. })));
        assert!(b
            .stmts
            .iter()
            .any(|s| matches!(s, Stmt::WriteReg { reg: GuestReg::X(0), .. })));
    }

    #[test]
    fn a_replacement_is_one_host_call_and_a_return() {
        // An AArch64/x86-64 fixture (64-bit little-endian ELF or A64 code).
        if gex_abi::PPC {
            return;
        }
        let b = block(0x1000, Intrinsic::Replace(7));
        let calls: Vec<_> = b
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::HostCall { func, args } => Some((*func, args.len())),
                _ => None,
            })
            .collect();
        assert_eq!(calls, vec![(HostFunc::SysRegWrite, 3)]);
        assert!(matches!(b.term, Term::Indirect { is_ret: true, .. }));
        // The replacement writes the result itself; the block writes no
        // register but the stack pointer x86-64's return pops.
        let writes: Vec<_> = b
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::WriteReg { reg, .. } => Some(*reg),
                _ => None,
            })
            .collect();
        if gex_abi::X86 {
            assert_eq!(writes, vec![GuestReg::X(4)]);
        } else {
            assert!(writes.is_empty());
        }
    }

    #[test]
    fn an_override_dispatches_where_it_left_the_pc() {
        // On PowerPC an eject override is illegal (no replacement ABI there).
        if gex_abi::PPC {
            return;
        }
        let b = block(0x1000, Intrinsic::Override(3));
        assert!(b.stmts.iter().any(|s| matches!(s, Stmt::HostCall { func: HostFunc::SysRegWrite, .. })));
        // No register is written: the override did the return itself.
        assert!(!b.stmts.iter().any(|s| matches!(s, Stmt::WriteReg { .. })));
        assert!(b.stmts.iter().any(|s| matches!(s, Stmt::Def { expr: Expr::ReadReg(GuestReg::Pc), .. })));
        assert!(matches!(b.term, Term::Indirect { is_ret: true, .. }));
    }

    #[test]
    fn mempcpy_returns_the_end() {
        // An AArch64/x86-64 fixture (64-bit little-endian ELF or A64 code).
        if gex_abi::PPC {
            return;
        }
        let b = block(0x1000, Intrinsic::MemPCpy);
        assert!(b
            .stmts
            .iter()
            .any(|s| matches!(s, Stmt::WriteReg { reg: GuestReg::X(0), .. })));
    }
}
