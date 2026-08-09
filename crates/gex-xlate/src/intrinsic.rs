//! Library functions the backend serves itself, instead of translating them.
//!
//! glibc's `memcpy` is a hand-written NEON loop with a dozen entry paths for
//! different sizes and alignments. Lifting it is correct and slow: every
//! iteration becomes a block, every block a dispatch, and the whole thing
//! competes with a wasm instruction -- `memory.copy` -- that does the same
//! work in one. Decompression and hashing, which is what `apt update` and
//! `xz` are, spend most of their time in exactly these functions.
//!
//! This is the trade box64 and FEX make: recognise the function by name and
//! bind it to a host implementation rather than translating it. The names
//! come from the object's own symbol table, which discovery already reads for
//! seeds.
//!
//! # Why a name is enough to be sure
//!
//! Substituting the wrong function would be a very bad bug, so the claim
//! being made is worth stating. A symbol here is `STT_FUNC` in the object's
//! symbol table -- `elf::load` keeps no other kind -- and it names a
//! *definition* in that object's text at that address. The names below are
//! reserved by the C standard or are glibc's own internal implementations of
//! them, whose behaviour is fixed by the standard.
//!
//! The exclusion of every other symbol type is what makes this safe against
//! IFUNCs, and aarch64 glibc resolves `memcpy` through one: `STT_GNU_IFUNC`'s
//! value is the address of the *resolver*, a function taking hwcaps and
//! returning a pointer, and serving that as a `memcpy` would be nonsense.
//! Those symbols never reach this table. What does reach it is
//! `__memcpy_generic` and its siblings, the real implementations, which is
//! what the resolver returns and what the guest actually calls.

use gex_ir::*;

/// A library function with a wasm instruction behind it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Intrinsic {
    /// `void *memcpy(void *dst, const void *src, size_t n)`.
    ///
    /// `memmove` shares this: `memory.copy` is defined as if the bytes went
    /// through a temporary, which is exactly `memmove`'s guarantee and a
    /// superset of `memcpy`'s.
    MemCpy,
    /// `void *mempcpy(void *dst, const void *src, size_t n)`, which differs
    /// from `memcpy` only in returning the *end* of the copy.
    MemPCpy,
    /// `void *memset(void *dst, int c, size_t n)`.
    MemSet,
}

/// The intrinsic a symbol names, if it names one.
///
/// Both the public name and glibc's private implementations of it, because on
/// aarch64 the public one is an IFUNC and never appears as a `STT_FUNC`
/// symbol at all -- so matching only the plain names would silently do
/// nothing on the C library that matters most.
pub fn recognise(name: &str) -> Option<Intrinsic> {
    // Two families of glibc helper share a stem with the function they help
    // and do not share its signature, so the shape match below must not
    // reach them:
    //
    // * `__memcpy_chk` takes a fourth argument -- the size the compiler
    //   believes the destination to be -- and aborts when the copy would
    //   overrun it. Throwing away a bounds check is not this pass's decision.
    // * `__mempcpy_small` is the out-of-line half of an inline expansion in
    //   glibc's own `string2.h`, and takes the bytes to store as separate
    //   arguments rather than a pointer and a length.
    //
    // Both are real symbols in Debian's libc, so this is a list of what was
    // actually there rather than a precaution.
    if name.ends_with("_chk") || name.ends_with("_small") {
        return None;
    }
    // `__memcpy_generic`, `__memcpy_simd`, `__memcpy_thunderx`,
    // `__memcpy_a64fx`, ... -- the set differs by glibc version and by which
    // cores it was built to know about, so the variant suffix is matched by
    // shape. Only under a leading `__`, which is what makes it glibc's
    // private namespace rather than some other library's `memcpy_from_page`.
    let stem = match name.strip_prefix("__") {
        Some(rest) => rest.split_once('_').map_or(rest, |(s, _)| s),
        None => name,
    };
    match stem {
        "memcpy" | "memmove" => Some(Intrinsic::MemCpy),
        "mempcpy" => Some(Intrinsic::MemPCpy),
        "memset" => Some(Intrinsic::MemSet),
        _ => None,
    }
}

/// The whole function as one block: read the arguments, do it, return.
///
/// The AAPCS64 argument registers are `x0`, `x1`, `x2` and the return address
/// is in `x30`, so this is the entire body. Registers a real implementation
/// would clobber are left alone, which no caller can object to -- the
/// procedure call standard lets a callee keep them, it does not require the
/// mess.
pub fn block(addr: u64, which: Intrinsic) -> Block {
    let mut b = Builder::new(addr);
    let dst = b.read(GuestReg::X(0));
    let src = b.read(GuestReg::X(1));
    let len = b.read(GuestReg::X(2));
    match which {
        Intrinsic::MemCpy => b.stmt(Stmt::MemCopy { dst, src, len }),
        Intrinsic::MemPCpy => {
            b.stmt(Stmt::MemCopy { dst, src, len });
            // The one difference, and the reason it is a separate kind: it
            // returns where the copy ended, and glibc's own string code uses
            // it precisely for that.
            let end = b.bin(BinOp::Add, Ty::I64, dst, len);
            b.write(GuestReg::X(0), end);
        }
        // `val` is an `int` whose low byte is the fill; `memory.fill` takes
        // the same and ignores the rest, so no masking is needed here.
        Intrinsic::MemSet => b.stmt(Stmt::MemFill { dst, val: src, len }),
    }
    let lr = b.read(GuestReg::X(30));
    b.finish(Term::Indirect {
        target: lr,
        is_ret: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_glibc_private_names() {
        assert_eq!(recognise("memcpy"), Some(Intrinsic::MemCpy));
        assert_eq!(recognise("memmove"), Some(Intrinsic::MemCpy));
        assert_eq!(recognise("__memcpy_generic"), Some(Intrinsic::MemCpy));
        assert_eq!(recognise("__memcpy_simd"), Some(Intrinsic::MemCpy));
        assert_eq!(recognise("__memmove_thunderx2"), Some(Intrinsic::MemCpy));
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
        assert_eq!(recognise("memcmp"), None);
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
        // returned its first argument in x0 already has it there.
        assert!(!b.stmts.iter().any(|s| matches!(s, Stmt::WriteReg { .. })));
    }

    #[test]
    fn mempcpy_returns_the_end() {
        let b = block(0x1000, Intrinsic::MemPCpy);
        assert!(b
            .stmts
            .iter()
            .any(|s| matches!(s, Stmt::WriteReg { reg: GuestReg::X(0), .. })));
    }
}
