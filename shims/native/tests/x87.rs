//! `Stmt::X87` lowers to a module that validates: every operation, in a
//! plain block and in a fused loop.

use gex_ir::*;

fn x87_block(start: u64, target: u64) -> Block {
    let mut b = Builder::new(start);
    let sig = b.read(GuestReg::X(0));
    let se64 = b.read(GuestReg::X(1));
    let se = b.def(Expr::Wrap(se64));
    let cw = b.const_i32(0x37f);
    let mut n = 2;
    for op in [
        X87Op::Narrow { single: true },
        X87Op::Narrow { single: false },
        X87Op::ToInt { bits: 16, trunc: false },
        X87Op::ToInt { bits: 32, trunc: true },
        X87Op::ToInt { bits: 64, trunc: false },
    ] {
        let outs = b.x87(op, vec![sig, se, cw]);
        b.write(GuestReg::X(n), outs[0]);
        let f = b.def(Expr::Extend { a: outs[1], signed: false });
        b.write(GuestReg::X(n + 1), f);
        n += 2;
    }
    b.finish(Term::Goto { target })
}

#[test]
fn x87_operations_lower_to_a_valid_module() {
    let mut m = gex_emit::ModuleBuilder::new(0x1000, 0x1000);
    m.add_block(x87_block(0x1000, 0x1010));
    m.add_block(x87_block(0x1010, 0x1010));
    let module = m.finish();
    gex_native::validate_module(&module.wasm).expect("module with x87 operations validates");
}
