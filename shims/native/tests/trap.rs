//! `Stmt::Trap` lowers to a module that validates, in a plain block and in
//! a fused loop (where the registers live only in locals and the faulting
//! path must write them back).

use gex_ir::*;

fn trap_block(start: u64, target: u64) -> Block {
    let mut b = Builder::new(start);
    let x = b.read(GuestReg::X(0));
    let one = b.const_i64(1);
    let y = b.bin(BinOp::Add, Ty::I64, x, one);
    b.write(GuestReg::X(0), y);
    let zero = b.const_i64(0);
    let cond = b.cmp(CmpOp::Eq, Ty::I64, y, zero);
    b.stmt(Stmt::Trap {
        cond,
        pc: start + 4,
        code: gex_abi::imports::BRK_DIVIDE_BY_ZERO,
    });
    b.write(GuestReg::X(1), y);
    b.finish(Term::Goto { target })
}

#[test]
fn a_trap_lowers_to_a_valid_conditional_fault() {
    let mut m = gex_emit::ModuleBuilder::new(0x1000, 0x1000);
    // A block that falls through, and one that loops on itself.
    m.add_block(trap_block(0x1000, 0x1010));
    m.add_block(trap_block(0x1010, 0x1010));
    let module = m.finish();
    gex_native::validate_module(&module.wasm).expect("module with a Trap validates");
}
