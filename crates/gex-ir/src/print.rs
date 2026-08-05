//! Human-readable GIR, for `gexc --dump-ir` and for reading test failures.

use crate::*;
use std::fmt::Write;

fn reg(r: GuestReg) -> String {
    match r {
        GuestReg::X(n) => format!("x{n}"),
        GuestReg::V128(n) => format!("v{n}"),
        GuestReg::Sp => "sp".into(),
        GuestReg::Pc => "pc".into(),
        GuestReg::N => "nf".into(),
        GuestReg::Z => "zf".into(),
        GuestReg::C => "cf".into(),
        GuestReg::V => "vf".into(),
        GuestReg::Tpidr => "tpidr".into(),
        GuestReg::TpidrRo => "tpidrro".into(),
        GuestReg::MonAddr => "mon.addr".into(),
        GuestReg::MonValue => "mon.val".into(),
        GuestReg::MonSize => "mon.size".into(),
    }
}

fn ty(t: Ty) -> &'static str {
    match t {
        Ty::I32 => "i32",
        Ty::I64 => "i64",
        Ty::V128 => "v128",
    }
}

/// Name a floating-point width.
fn fw(double: bool) -> &'static str {
    if double {
        "f64"
    } else {
        "f32"
    }
}

fn off(o: i64) -> String {
    if o == 0 {
        String::new()
    } else if o < 0 {
        format!("-0x{:x}", -o)
    } else {
        format!("+0x{o:x}")
    }
}

fn expr(e: &Expr) -> String {
    match e {
        Expr::ConstI32(v) => format!("i32 0x{v:x}"),
        Expr::ConstI64(v) => format!("i64 0x{v:x}"),
        Expr::ReadReg(r) => format!("read {}", reg(*r)),
        Expr::Bin { op, ty: t, a, b } => format!("{op:?}.{} {a:?}, {b:?}", ty(*t)).to_lowercase(),
        Expr::Un { op, ty: t, a } => format!("{op:?}.{} {a:?}", ty(*t)).to_lowercase(),
        Expr::Cmp { op, ty: t, a, b } => format!("cmp.{op:?}.{} {a:?}, {b:?}", ty(*t)),
        Expr::Select { cond, a, b, ty: t } => {
            format!("select.{} {cond:?} ? {a:?} : {b:?}", ty(*t))
        }
        Expr::Wrap(a) => format!("wrap {a:?}"),
        Expr::Extend { a, signed } => {
            format!("{} {a:?}", if *signed { "sext32" } else { "zext32" })
        }
        Expr::ExtendBits {
            a,
            ty: t,
            bits,
            signed,
        } => format!(
            "{}{bits}.{} {a:?}",
            if *signed { "sext" } else { "zext" },
            ty(*t)
        ),
        Expr::Load {
            addr,
            offset,
            sz,
            signed,
            ty: t,
            order,
        } => format!(
            "load{}{}.{} [{addr:?}{}]{}",
            sz.bytes() * 8,
            if *signed { "s" } else { "u" },
            ty(*t),
            off(*offset),
            if *order == Order::SeqCst { " seq_cst" } else { "" }
        ),
        Expr::Rmw {
            op,
            addr,
            val,
            sz,
            ty: t,
        } => format!(
            "rmw.{op:?}{}.{} [{addr:?}], {val:?}",
            sz.bytes() * 8,
            ty(*t)
        ),
        Expr::CmpXchg {
            addr,
            expected,
            replacement,
            sz,
            ty: t,
        } => format!(
            "cmpxchg{}.{} [{addr:?}], {expected:?}, {replacement:?}",
            sz.bytes() * 8,
            ty(*t)
        ),
        Expr::HostCall { func, args, ty: t } => {
            format!("hostcall.{} {func:?}({args:?})", ty(*t))
        }
        Expr::VLoad {
            addr,
            offset,
            bytes,
        } => format!("vload{} [{addr:?}{}]", bytes * 8, off(*offset)),
        Expr::VConst(b) => format!("v128 {b:02x?}"),
        Expr::VSplat { a, lane_bytes } => format!("splat{} {a:?}", lane_bytes * 8),
        Expr::VBin { op, a, b } => format!("v{op:?} {a:?}, {b:?}").to_lowercase(),
        Expr::VUn { op, a } => format!("v{op:?} {a:?}").to_lowercase(),
        Expr::VExtract {
            a,
            lane,
            lane_bytes,
            signed,
            ..
        } => format!(
            "extract{}{} {a:?}[{lane}]",
            lane_bytes * 8,
            if *signed { "s" } else { "u" }
        ),
        Expr::VReplace {
            a,
            lane,
            lane_bytes,
            val,
        } => format!("replace{} {a:?}[{lane}] = {val:?}", lane_bytes * 8),
        Expr::FpBin { op, double, a, b } => {
            format!("f{op:?}.{} {a:?}, {b:?}", fw(*double)).to_lowercase()
        }
        Expr::FpUn { op, double, a } => format!("f{op:?}.{} {a:?}", fw(*double)).to_lowercase(),
        Expr::FpCmp { op, double, a, b } => {
            format!("fcmp.{op:?}.{} {a:?}, {b:?}", fw(*double)).to_lowercase()
        }
        Expr::FpCvt {
            kind,
            src_wide,
            dst_wide,
            a,
        } => format!(
            "fcvt.{kind:?} {}->{} {a:?}",
            fw(*src_wide),
            fw(*dst_wide)
        ),
    }
}

/// Render a block as text.
pub fn block(b: &Block) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "block 0x{:x}..0x{:x}:", b.start, b.end);
    for stmt in &b.stmts {
        match stmt {
            Stmt::Def { dst, expr: e } => {
                let _ = writeln!(s, "  {dst:?} = {}", expr(e));
            }
            Stmt::WriteReg { reg: r, val } => {
                let _ = writeln!(s, "  write {} = {val:?}", reg(*r));
            }
            Stmt::Store {
                addr,
                offset,
                val,
                sz,
                order,
            } => {
                let _ = writeln!(
                    s,
                    "  store{} [{addr:?}{}] = {val:?}{}",
                    sz.bytes() * 8,
                    off(*offset),
                    if *order == Order::SeqCst { " seq_cst" } else { "" }
                );
            }
            Stmt::HostCall { func, args } => {
                let _ = writeln!(s, "  hostcall {func:?}({args:?})");
            }
            Stmt::VStore {
                addr,
                offset,
                val,
                bytes,
            } => {
                let _ = writeln!(s, "  vstore{} [{addr:?}{}] = {val:?}", bytes * 8, off(*offset));
            }
            Stmt::Fence => {
                let _ = writeln!(s, "  fence");
            }
        }
    }
    let _ = match &b.term {
        Term::Goto { target } => writeln!(s, "  goto 0x{target:x}"),
        Term::Branch {
            cond,
            taken,
            not_taken,
        } => writeln!(s, "  br {cond:?} ? 0x{taken:x} : 0x{not_taken:x}"),
        Term::Indirect { target, is_ret } => writeln!(
            s,
            "  {} {target:?}",
            if *is_ret { "ret" } else { "br_indirect" }
        ),
        Term::Syscall { next } => writeln!(s, "  syscall -> 0x{next:x}"),
        Term::Illegal { raw, pc } => writeln!(s, "  illegal 0x{raw:08x} @0x{pc:x}"),
    };
    s
}
