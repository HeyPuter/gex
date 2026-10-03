//! Standalone C for a set of functions: what `gexc eject decompile` writes.
//!
//! The same Phase B C as `build_functions` emits (`func::FnUnit`), as one
//! self-contained translation unit that does not need the splice:
//!
//! * every body, loop function and `gex_call_known` is `static`, so several
//!   such units link into one module without colliding;
//! * no dispatch root (`e_<F>`) is emitted, so nothing imports
//!   `gexc.dispatch`;
//! * each requested root `F` gets an exported `void NAME(int ctx)` that runs
//!   `F` exactly as its root would (`f_<F>(ctx, 0, NEED_ALL, ~0)`, and on a
//!   predicted return, which no caller can have, `ctx::PC = ~0`), and
//!   returns, leaving `ctx::PC` where the guest goes next: the return
//!   address after a guest `ret`, or the target of any exit to code the
//!   unit does not hold. Its caller dispatches on `ctx::PC`.
//!
//! Its imports are the `gex.*` host functions (`gex_abi.h`) and the memory.

use crate::func::{self, FnUnit};
use anyhow::{bail, Context, Result};
use gex_ir::func::{Function, Summary};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

/// One translation unit holding `funcs` (C bodies for each, calls between
/// them staying in C) and an exported entry for each of `roots`, as
/// `(function entry, export name)`. `funcs` are what [`crate::select`] kept,
/// so the C is the function tier's own; `sums` are S2's summaries, by entry;
/// `class32` says the guest is a 32-bit image (i386), as for the tier's units;
/// `read` gives a guest code byte (for `func::ret_pops`).
pub fn standalone(
    funcs: &[Function],
    roots: &[(u64, String)],
    sums: &HashMap<u64, Summary>,
    class32: bool,
    read: &(dyn Fn(u64) -> Option<u8> + Sync),
) -> Result<String> {
    let known: HashSet<u64> = funcs.iter().map(|f| f.entry).collect();
    for (r, name) in roots {
        if !known.contains(r) {
            bail!("root {r:#x} ({name}) is not among the functions given");
        }
    }
    let trapfree = func::trap_free(funcs, sums);
    let mut u = FnUnit::new_standalone();
    u.set_class32(class32);
    u.ret_pops = func::ret_pops(funcs, class32, read);
    for f in funcs {
        u.add(f, &known, sums, &trapfree)
            .with_context(|| format!("emitting C for the function at {:#x}", f.entry))?;
    }
    let mut s = u.source();
    // The by-address call helper, over this unit's functions only: an
    // indirect call to anything else leaves, as it would leave the tier.
    let _ = writeln!(
        s,
        "static int {}(int ctx, uint64_t target, int32_t depth, uint32_t need, uint64_t ret) {{",
        func::CALL_KNOWN
    );
    s.push_str("  switch (target) {\n");
    for f in funcs {
        let _ = writeln!(s, "  case 0x{:x}ull: return {}(ctx, depth, need, ret);", f.entry, func::body_name(f.entry));
    }
    s.push_str("  default: return -1;\n  }\n}\n\n");
    for (r, name) in roots {
        let _ = writeln!(
            s,
            "/* The guest function at {r:#x}, as gex's function tier runs it: on return,\n   \
             ctx's PC says where the guest goes next. */\n\
             __attribute__((export_name(\"{name}\"))) void {name}(int ctx) {{\n  \
             if ({}(ctx, 0, NEED_ALL, ~0ull)) GEX_C64({}u) = ~0ull;\n}}\n",
            func::body_name(*r),
            gex_abi::ctx::PC
        );
    }
    Ok(s)
}
