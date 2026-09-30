//! Intel-syntax disassembly for [`Inst`], close enough to `objdump -M intel`
//! that the two can be diffed by mnemonic.

use crate::*;

const GP64: [&str; 16] = [
    "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13",
    "r14", "r15",
];
const GP32: [&str; 16] = [
    "eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d",
    "r13d", "r14d", "r15d",
];
const GP16: [&str; 16] = [
    "ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w", "r13w",
    "r14w", "r15w",
];
const GP8: [&str; 16] = [
    "al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b",
    "r13b", "r14b", "r15b",
];

pub fn reg(r: Reg) -> String {
    match r {
        Reg::Gp(n, Width::Q) => GP64[n as usize & 15].into(),
        Reg::Gp(n, Width::D) => GP32[n as usize & 15].into(),
        Reg::Gp(n, Width::W) => GP16[n as usize & 15].into(),
        Reg::Gp(n, Width::B) => GP8[n as usize & 15].into(),
        Reg::High(n) => ["ah", "ch", "dh", "bh"][n as usize & 3].into(),
        Reg::Xmm(n) => format!("xmm{n}"),
        Reg::Mm(n) => format!("mm{n}"),
        Reg::St(n) => format!("st({n})"),
        Reg::Seg(s) => s.name().into(),
    }
}

fn mem(m: &Mem) -> String {
    let mut s = String::new();
    if let Some(seg) = m.seg {
        s.push_str(seg.name());
        s.push(':');
    }
    if m.rip {
        s.push_str(&format!("[rip] # 0x{:x}", m.disp));
        return s;
    }
    let names = if m.addr32 { &GP32 } else { &GP64 };
    s.push('[');
    let mut first = true;
    if let Some(b) = m.base {
        s.push_str(names[b as usize & 15]);
        first = false;
    }
    if let Some(i) = m.index {
        if !first {
            s.push('+');
        }
        s.push_str(names[i as usize & 15]);
        if m.scale > 1 {
            s.push_str(&format!("*{}", m.scale));
        }
        first = false;
    }
    if m.disp != 0 || first {
        if first {
            s.push_str(&format!("0x{:x}", m.disp as u64));
        } else if m.disp < 0 {
            s.push_str(&format!("-0x{:x}", m.disp.unsigned_abs()));
        } else {
            s.push_str(&format!("+0x{:x}", m.disp));
        }
    }
    s.push(']');
    s
}

fn operand(o: &Operand) -> String {
    match o {
        Operand::Reg(r) => reg(*r),
        Operand::Mem(m) => mem(m),
        Operand::Imm(v) => {
            if *v < 0 {
                format!("-0x{:x}", v.unsigned_abs())
            } else {
                format!("0x{v:x}")
            }
        }
    }
}

/// `ps`/`pd`/`ss`/`sd` for a float lane and scalar flag.
fn fsfx(lane: Lane, scalar: bool) -> &'static str {
    match (lane, scalar) {
        (Lane::F64, false) => "pd",
        (Lane::F64, true) => "sd",
        (_, false) => "ps",
        (_, true) => "ss",
    }
}

fn isfx(lane: Lane) -> &'static str {
    match lane {
        Lane::I8 => "b",
        Lane::I16 => "w",
        Lane::I32 | Lane::F32 => "d",
        Lane::I64 | Lane::F64 => "q",
    }
}

fn x87_src_pop(src: X87Src) -> bool {
    matches!(src, X87Src::StiSt0 { pop: true, .. })
}

fn x87_int(src: X87Src) -> bool {
    matches!(src, X87Src::MemI(_))
}

/// The instruction's mnemonic, as binutils spells it in Intel mode.
pub fn mnemonic(i: &Inst) -> String {
    use Op::*;
    let w = i.width;
    let l = i.lane;
    let sc = i.scalar;
    let s: String = match i.op {
        Unknown => "(bad)".into(),
        Undefined => "ud2".into(),
        Mov => "mov".into(),
        Movabs => "movabs".into(),
        Movzx(_) => "movzx".into(),
        Movsx(_) => "movsx".into(),
        Movsxd => "movsxd".into(),
        Lea => "lea".into(),
        Push => "push".into(),
        Pop => "pop".into(),
        Xchg => "xchg".into(),
        Cmovcc(c) => format!("cmov{}", c.name()),
        Setcc(c) => format!("set{}", c.name()),
        Cbw => match w {
            Width::W => "cbw",
            Width::D => "cwde",
            _ => "cdqe",
        }
        .into(),
        Cwd => match w {
            Width::W => "cwd",
            Width::D => "cdq",
            _ => "cqo",
        }
        .into(),
        Bswap => "bswap".into(),
        Movbe => "movbe".into(),
        Xlat => "xlat".into(),
        Add => "add".into(),
        Or => "or".into(),
        Adc => "adc".into(),
        Sbb => "sbb".into(),
        And => "and".into(),
        Sub => "sub".into(),
        Xor => "xor".into(),
        Cmp => "cmp".into(),
        Test => "test".into(),
        Inc => "inc".into(),
        Dec => "dec".into(),
        Neg => "neg".into(),
        Not => "not".into(),
        Mul => "mul".into(),
        Imul1 | Imul => "imul".into(),
        Div => "div".into(),
        Idiv => "idiv".into(),
        Shl => "shl".into(),
        Shr => "shr".into(),
        Sar => "sar".into(),
        Rol => "rol".into(),
        Ror => "ror".into(),
        Rcl => "rcl".into(),
        Rcr => "rcr".into(),
        Shld => "shld".into(),
        Shrd => "shrd".into(),
        Bt => "bt".into(),
        Bts => "bts".into(),
        Btr => "btr".into(),
        Btc => "btc".into(),
        Bsf => "bsf".into(),
        Bsr => "bsr".into(),
        Tzcnt => "tzcnt".into(),
        Lzcnt => "lzcnt".into(),
        Popcnt => "popcnt".into(),
        Xadd => "xadd".into(),
        Cmpxchg => "cmpxchg".into(),
        CmpxchgNb => if w == Width::Q { "cmpxchg16b" } else { "cmpxchg8b" }.into(),
        Crc32 => "crc32".into(),
        Andn => "andn".into(),
        Bzhi => "bzhi".into(),
        Blsr => "blsr".into(),
        Blsmsk => "blsmsk".into(),
        Blsi => "blsi".into(),
        Sarx => "sarx".into(),
        Shlx => "shlx".into(),
        Shrx => "shrx".into(),
        Bextr => "bextr".into(),
        Jmp => "jmp".into(),
        Jcc(c) => format!("j{}", c.name()),
        Jrcxz => if w == Width::D { "jecxz" } else { "jrcxz" }.into(),
        Loop => "loop".into(),
        Loope => "loope".into(),
        Loopne => "loopne".into(),
        Call => "call".into(),
        Ret => "ret".into(),
        Leave => "leave".into(),
        Enter => "enter".into(),
        Syscall => "syscall".into(),
        Int3 => "int3".into(),
        Int => "int".into(),
        Hlt => "hlt".into(),
        Privileged(op) => match op {
            0x6c | 0x6d => "ins",
            0x6e | 0x6f => "outs",
            0xe4 | 0xe5 | 0xec | 0xed => "in",
            0xe6 | 0xe7 | 0xee | 0xef => "out",
            0xfa => "cli",
            _ => "sti",
        }
        .into(),
        Nop => "nop".into(),
        Endbr => "endbr64".into(),
        Pause => "pause".into(),
        Cpuid => "cpuid".into(),
        Rdtsc => "rdtsc".into(),
        Rdtscp => "rdtscp".into(),
        Xgetbv => "xgetbv".into(),
        Rdrand => "rdrand".into(),
        Rdseed => "rdseed".into(),
        Fence => "mfence".into(),
        Prefetch => "prefetcht0".into(),
        Clflush => "clflush".into(),
        Xsave => "xsave".into(),
        Xrstor => "xrstor".into(),
        Fxsave => "fxsave".into(),
        Fxrstor => "fxrstor".into(),
        Clc => "clc".into(),
        Stc => "stc".into(),
        Cmc => "cmc".into(),
        Cld => "cld".into(),
        Std => "std".into(),
        Sahf => "sahf".into(),
        Lahf => "lahf".into(),
        Pushf => "pushf".into(),
        Popf => "popf".into(),
        Movs => "movs".into(),
        Stos => "stos".into(),
        Lods => "lods".into(),
        Scas => "scas".into(),
        Cmps => "cmps".into(),

        Movaps => match l {
            Lane::F32 => "movaps",
            Lane::F64 => "movapd",
            _ => "movdqa",
        }
        .into(),
        Movups => match l {
            Lane::F32 => "movups",
            Lane::F64 => "movupd",
            _ => "movdqu",
        }
        .into(),
        Movss => "movss".into(),
        Movsd => "movsd".into(),
        Movd => if w == Width::Q { "movq" } else { "movd" }.into(),
        Movq => "movq".into(),
        Movlps => if l == Lane::F64 { "movlpd" } else { "movlps" }.into(),
        Movhps => if l == Lane::F64 { "movhpd" } else { "movhps" }.into(),
        Movhlps => "movhlps".into(),
        Movlhps => "movlhps".into(),
        Movmsk => format!("movmsk{}", fsfx(l, false)),
        Pmovmskb => "pmovmskb".into(),
        Movnti => "movnti".into(),
        Movsldup => "movsldup".into(),
        Movshdup => "movshdup".into(),
        Movddup => "movddup".into(),
        Maskmov => if i.mmx { "maskmovq" } else { "maskmovdqu" }.into(),

        Addp => format!("add{}", fsfx(l, sc)),
        Subp => format!("sub{}", fsfx(l, sc)),
        Mulp => format!("mul{}", fsfx(l, sc)),
        Divp => format!("div{}", fsfx(l, sc)),
        Minp => format!("min{}", fsfx(l, sc)),
        Maxp => format!("max{}", fsfx(l, sc)),
        Sqrtp => format!("sqrt{}", fsfx(l, sc)),
        Rsqrtp => format!("rsqrt{}", fsfx(l, sc)),
        Rcpp => format!("rcp{}", fsfx(l, sc)),
        Andp => format!("and{}", fsfx(l, false)),
        Andnp => format!("andn{}", fsfx(l, false)),
        Orp => format!("or{}", fsfx(l, false)),
        Xorp => format!("xor{}", fsfx(l, false)),
        Cmpp => {
            let pred = match i.ops[2] {
                Some(Operand::Imm(p)) => {
                    ["eq", "lt", "le", "unord", "neq", "nlt", "nle", "ord"][(p & 7) as usize]
                }
                _ => "",
            };
            format!("cmp{pred}{}", fsfx(l, sc))
        }
        Comis => format!("comi{}", fsfx(l, true)),
        Ucomis => format!("ucomi{}", fsfx(l, true)),
        Haddp => format!("hadd{}", fsfx(l, false)),
        Hsubp => format!("hsub{}", fsfx(l, false)),
        Addsubp => format!("addsub{}", fsfx(l, false)),
        Roundp => format!("round{}", fsfx(l, sc)),
        Dpp => format!("dp{}", fsfx(l, false)),
        Blendp => format!("blend{}", fsfx(l, false)),
        Blendvp => format!("blendv{}", fsfx(l, false)),
        Shufp => format!("shuf{}", fsfx(l, false)),
        Unpcklp => format!("unpckl{}", fsfx(l, false)),
        Unpckhp => format!("unpckh{}", fsfx(l, false)),

        Cvtsi2s => format!("cvtsi2{}", fsfx(l, true)),
        Cvts2si { truncate } => format!("cvt{}{}2si", if truncate { "t" } else { "" }, fsfx(l, true)),
        Cvts2s => if l == Lane::F32 { "cvtss2sd" } else { "cvtsd2ss" }.into(),
        Cvtp2p => if l == Lane::F32 { "cvtps2pd" } else { "cvtpd2ps" }.into(),
        Cvtdq2p => format!("cvtdq2{}", fsfx(l, false)),
        Cvtp2dq { truncate } => format!("cvt{}{}2dq", if truncate { "t" } else { "" }, fsfx(l, false)),
        CvtMmx => "cvtpi2ps".into(),

        Padd => format!("padd{}", isfx(l)),
        Psub => format!("psub{}", isfx(l)),
        Padds => format!("padds{}", isfx(l)),
        Paddus => format!("paddus{}", isfx(l)),
        Psubs => format!("psubs{}", isfx(l)),
        Psubus => format!("psubus{}", isfx(l)),
        Pmullw => "pmullw".into(),
        Pmulhw => "pmulhw".into(),
        Pmulhuw => "pmulhuw".into(),
        Pmulhrsw => "pmulhrsw".into(),
        Pmulld => "pmulld".into(),
        Pmuludq => "pmuludq".into(),
        Pmuldq => "pmuldq".into(),
        Pmaddwd => "pmaddwd".into(),
        Pmaddubsw => "pmaddubsw".into(),
        Pavg => format!("pavg{}", isfx(l)),
        Pmaxu => format!("pmaxu{}", isfx(l)),
        Pminu => format!("pminu{}", isfx(l)),
        Pmaxs => format!("pmaxs{}", isfx(l)),
        Pmins => format!("pmins{}", isfx(l)),
        Psadbw => "psadbw".into(),
        Pcmpeq => format!("pcmpeq{}", isfx(l)),
        Pcmpgt => format!("pcmpgt{}", isfx(l)),
        Pand => "pand".into(),
        Pandn => "pandn".into(),
        Por => "por".into(),
        Pxor => "pxor".into(),
        Psll => format!("psll{}", isfx(l)),
        Psrl => format!("psrl{}", isfx(l)),
        Psra => format!("psra{}", isfx(l)),
        Pslldq => "pslldq".into(),
        Psrldq => "psrldq".into(),
        Packss => if l == Lane::I16 { "packsswb" } else { "packssdw" }.into(),
        Packus => if l == Lane::I16 { "packuswb" } else { "packusdw" }.into(),
        Punpckl => match l {
            Lane::I8 => "punpcklbw",
            Lane::I16 => "punpcklwd",
            Lane::I32 => "punpckldq",
            _ => "punpcklqdq",
        }
        .into(),
        Punpckh => match l {
            Lane::I8 => "punpckhbw",
            Lane::I16 => "punpckhwd",
            Lane::I32 => "punpckhdq",
            _ => "punpckhqdq",
        }
        .into(),
        Pshufd => "pshufd".into(),
        Pshufhw => "pshufhw".into(),
        Pshuflw => "pshuflw".into(),
        Pshufw => "pshufw".into(),
        Pshufb => "pshufb".into(),
        Palignr => "palignr".into(),
        Pabs => format!("pabs{}", isfx(l)),
        Psign => format!("psign{}", isfx(l)),
        Phadd => format!("phadd{}", isfx(l)),
        Phadds => "phaddsw".into(),
        Phsub => format!("phsub{}", isfx(l)),
        Phsubs => "phsubsw".into(),
        Pblendvb => "pblendvb".into(),
        Pblendw => "pblendw".into(),
        Ptest => "ptest".into(),
        Pextr => format!("pextr{}", isfx(l)),
        Pinsr => format!("pinsr{}", isfx(l)),
        Extractps => "extractps".into(),
        Insertps => "insertps".into(),
        Pmovsx(a, b) => format!("pmovsx{}{}", isfx(a), isfx(b)),
        Pmovzx(a, b) => format!("pmovzx{}{}", isfx(a), isfx(b)),
        Phminposuw => "phminposuw".into(),
        Pcmpistri => "pcmpistri".into(),
        Pcmpistrm => "pcmpistrm".into(),
        Pcmpestri => "pcmpestri".into(),
        Pcmpestrm => "pcmpestrm".into(),
        Pclmulqdq => "pclmulqdq".into(),
        Mpsadbw => "mpsadbw".into(),
        Emms => "emms".into(),
        Ldmxcsr => "ldmxcsr".into(),
        Stmxcsr => "stmxcsr".into(),

        Fld(_) | FldSt(_) => "fld".into(),
        Fild(_) => "fild".into(),
        Fst { pop, .. } | FstSt { pop, .. } => if pop { "fstp" } else { "fst" }.into(),
        Fist { pop, truncate, .. } => match (truncate, pop) {
            (true, _) => "fisttp",
            (false, true) => "fistp",
            (false, false) => "fist",
        }
        .into(),
        Fxch(_) => "fxch".into(),
        Fadd(s) => format!("f{}add{}", if x87_int(s) { "i" } else { "" }, if x87_src_pop(s) { "p" } else { "" }),
        Fsub(s) => format!("f{}sub{}", if x87_int(s) { "i" } else { "" }, if x87_src_pop(s) { "p" } else { "" }),
        Fsubr(s) => format!("f{}subr{}", if x87_int(s) { "i" } else { "" }, if x87_src_pop(s) { "p" } else { "" }),
        Fmul(s) => format!("f{}mul{}", if x87_int(s) { "i" } else { "" }, if x87_src_pop(s) { "p" } else { "" }),
        Fdiv(s) => format!("f{}div{}", if x87_int(s) { "i" } else { "" }, if x87_src_pop(s) { "p" } else { "" }),
        Fdivr(s) => format!("f{}divr{}", if x87_int(s) { "i" } else { "" }, if x87_src_pop(s) { "p" } else { "" }),
        Fcom { pop, .. } => match pop {
            0 => "fcom",
            1 => "fcomp",
            _ => "fcompp",
        }
        .into(),
        Fucom { pop, .. } => match pop {
            0 => "fucom",
            1 => "fucomp",
            _ => "fucompp",
        }
        .into(),
        Fcomi { pop, unordered, .. } => match (unordered, pop) {
            (true, true) => "fucomip",
            (true, false) => "fucomi",
            (false, true) => "fcomip",
            (false, false) => "fcomi",
        }
        .into(),
        Ficom { pop, .. } => if pop { "ficomp" } else { "ficom" }.into(),
        Fchs => "fchs".into(),
        Fabs => "fabs".into(),
        Ftst => "ftst".into(),
        Fxam => "fxam".into(),
        Fld1 => "fld1".into(),
        Fldl2t => "fldl2t".into(),
        Fldl2e => "fldl2e".into(),
        Fldpi => "fldpi".into(),
        Fldlg2 => "fldlg2".into(),
        Fldln2 => "fldln2".into(),
        Fldz => "fldz".into(),
        F2xm1 => "f2xm1".into(),
        Fyl2x => "fyl2x".into(),
        Fptan => "fptan".into(),
        Fpatan => "fpatan".into(),
        Fxtract => "fxtract".into(),
        Fprem1 => "fprem1".into(),
        Fdecstp => "fdecstp".into(),
        Fincstp => "fincstp".into(),
        Fprem => "fprem".into(),
        Fyl2xp1 => "fyl2xp1".into(),
        Fsqrt => "fsqrt".into(),
        Fsincos => "fsincos".into(),
        Frndint => "frndint".into(),
        Fscale => "fscale".into(),
        Fsin => "fsin".into(),
        Fcos => "fcos".into(),
        Fnop => "fnop".into(),
        Fninit => "fninit".into(),
        Fnclex => "fnclex".into(),
        Fldcw => "fldcw".into(),
        Fnstcw => "fnstcw".into(),
        Fnstsw | FnstswAx => "fnstsw".into(),
        Fldenv => "fldenv".into(),
        Fnstenv => "fnstenv".into(),
        Frstor => "frstor".into(),
        Fnsave => "fnsave".into(),
        Ffree(_) => "ffree".into(),
        Ffreep(_) => "ffreep".into(),
        Fcmovcc { cond, .. } => match cond.0 {
            2 => "fcmovb",
            4 => "fcmove",
            6 => "fcmovbe",
            10 => "fcmovu",
            3 => "fcmovnb",
            5 => "fcmovne",
            7 => "fcmovnbe",
            _ => "fcmovnu",
        }
        .into(),
        Fwait => "fwait".into(),
    };
    s
}

/// Full disassembly text: prefixes, mnemonic, operands.
pub fn disasm(i: &Inst, _pc: u64) -> String {
    let mut s = String::new();
    if i.lock {
        s.push_str("lock ");
    }
    match (i.rep, i.op) {
        (Rep::Rep, Op::Movs | Op::Stos | Op::Lods) => s.push_str("rep "),
        (Rep::Rep, Op::Cmps | Op::Scas) => s.push_str("repe "),
        (Rep::RepNe, Op::Cmps | Op::Scas) => s.push_str("repne "),
        _ => {}
    }
    s.push_str(&mnemonic(i));
    let ops: Vec<String> = i.ops.iter().flatten().map(operand).collect();
    if !ops.is_empty() {
        s.push(' ');
        s.push_str(&ops.join(","));
    }
    s
}
