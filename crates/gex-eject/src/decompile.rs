//! `gexc eject decompile <project> <object> <symbol|0xoffset>`: the function
//! tier's C for a guest function, written into `user/src/` as an editable
//! override.
//!
//! The C is exactly what the function tier (`gex-cgen`, Phase B) emits for the
//! function, as a standalone unit (`gex_cgen::standalone`): registers as C
//! locals, guest memory through `gex_abi.h`'s helpers, system calls and
//! safepoints through the `gex.*` host functions. Its one export,
//! `void decompiled_<name>(int ctx)`, *is* the function: it returns with the
//! context's PC set to where the guest goes next. Bound with the "entry" ABI
//! (`Intrinsic::Override`), an unmodified decompiled function behaves exactly
//! as the original.
//!
//! What the C does not hold it leaves as the function tier does: a call to
//! another function writes everything back, sets PC to it, and returns, and
//! the rest of this function then runs as translated code (the original)
//! once the callee returns into it. `--callees` decompiles every function it
//! reaches by direct call as well, so those calls stay in C.

use crate::build::{find_object, function_hash, layout, symbol_in, Placed};
use crate::config::CONFIG_FILE;
use crate::json::{self, obj};
use crate::project::Project;
use anyhow::{anyhow, bail, Context, Result};
use gex_ir::func::{Callee, FTerm, Function};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct Decompiled {
    /// The C file written, relative to the project.
    pub file: String,
    /// The `user/eject.json` entry that binds it.
    pub entry: String,
    /// How many functions the file holds.
    pub functions: usize,
}

/// A C identifier from a symbol.
fn ident(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

/// The pages of the object's writable and executable segments, where
/// translated code carries a self-modification guard, so the function tier
/// keeps them (`gex_cgen::select`).
fn guarded(image: &gex_ld::Image) -> HashSet<u64> {
    image
        .segments
        .iter()
        .filter(|s| s.writable && s.executable)
        .flat_map(|s| (s.vaddr >> 12)..=((s.vaddr + s.mem_size.max(1) - 1) >> 12))
        .collect()
}

/// The functions `f` calls or tail-calls directly.
fn direct_callees(f: &Function) -> Vec<u64> {
    f.blocks
        .iter()
        .filter_map(|b| match &b.term {
            FTerm::Call { callee: Callee::Direct(a), .. } | FTerm::TailCall { callee: Callee::Direct(a) } => Some(*a),
            _ => None,
        })
        .collect()
}

pub fn decompile(dir: &Path, object: &str, what: &str, size: Option<u64>, callees: bool, force: bool) -> Result<Decompiled> {
    let project = Project::load(dir)?;
    if project.isa != gex_abi::ARCH_NAME {
        bail!("the project is for {} guests and this gexc runs {} guests", project.isa, gex_abi::ARCH_NAME);
    }
    let placed = layout(dir, &project)?;
    let p: &Placed = find_object(&placed, object)?;
    if p.image.class32 {
        bail!("{} is 32-bit (i386) code, which the function tier does not take yet", p.obj.path);
    }
    let (addr, size, name, key) = match what.strip_prefix("0x") {
        Some(h) => {
            let off = u64::from_str_radix(h, 16).map_err(|_| anyhow!("{what} is not a hex offset"))?;
            let addr = off.wrapping_add(p.image.bias);
            let size = match size {
                Some(s) => s,
                None => p
                    .image
                    .symbols
                    .iter()
                    .find(|s| s.value == addr && s.size > 0)
                    .map(|s| s.size)
                    .ok_or_else(|| anyhow!("no symbol gives the size of the function at {what}: pass --size"))?,
            };
            (addr, size, format!("fn_{h}"), ("offset", json::s(format!("{off:#x}"))))
        }
        None => {
            let (a, sz) = symbol_in(p, what)?;
            (a, size.unwrap_or(sz), what.to_string(), ("symbol", json::s(what)))
        }
    };
    let bytes = function_hash(&p.image, addr, size)?;

    // Recovery and summaries, as the function tier does them for this
    // object (with its `.eh_frame`, which a build from bytes has).
    let elf = std::fs::read(dir.join(crate::project::rootfs_rel(&p.obj.path)?))?;
    gex_xlate::set_compat(p.image.class32);
    let mut image = p.image.clone();
    if p.obj.role != "exe" {
        gex_ld::elf::apply_own_relative(&mut image);
    }
    let prog = gex_xlate::func::recover(&image, Some(&elf), &gex_xlate::func::RecoverOptions::default());
    let sums = gex_xlate::summary::analyze(&prog).map;
    // The entries tier-1 serves with a host builtin, as their one block. None
    // for a 32-bit (i386) object, as in gexc: the intrinsic block reads the
    // 64-bit register arguments, and i386 cdecl passes them on the stack.
    let (tlo, thi) = image.text_range;
    let intr_blocks: Vec<gex_ir::Block> = image
        .symbols
        .iter()
        .filter(|_| !image.class32)
        .filter(|s| !s.ifunc && s.value >= tlo && s.value < thi)
        .filter_map(|s| gex_xlate::intrinsic::recognise(&s.name).map(|k| gex_xlate::intrinsic::block(s.value, k)))
        .collect();
    let intr: HashMap<u64, &gex_ir::Block> = intr_blocks.iter().map(|b| (b.start, b)).collect();
    let all: Vec<Function> = prog.functions.into_values().collect();
    let recovered = all.iter().any(|f| f.entry == addr);
    let selected = gex_cgen::select(all, &intr, &guarded(&image))?;
    let by_entry: HashMap<u64, &Function> = selected.iter().map(|f| (f.entry, f)).collect();
    if !by_entry.contains_key(&addr) {
        if !recovered {
            bail!("function recovery found no function starting at {addr:#x} ({name}) in {}", p.obj.path);
        }
        bail!(
            "the function tier does not take {name} at {addr:#x} (self-modifying code nearby, a \
             statement with no C lowering yet such as x87, or a form it cannot emit): it stays \
             translated code"
        );
    }
    // The set: the function, and with `--callees` everything it reaches by
    // direct call that the tier takes.
    let mut set: BTreeSet<u64> = BTreeSet::from([addr]);
    if callees {
        let mut work = vec![addr];
        while let Some(a) = work.pop() {
            for c in direct_callees(by_entry[&a]) {
                if by_entry.contains_key(&c) && set.insert(c) {
                    work.push(c);
                }
            }
        }
    }
    let funcs: Vec<Function> = set.iter().map(|a| by_entry[a].clone()).collect();
    let export = format!("decompiled_{}", ident(&name));
    let body = gex_cgen::standalone::standalone(&funcs, &[(addr, export.clone())], &sums, p.image.class32)
        .with_context(|| format!("emitting C for {name}"))?;

    let objname = p.obj.path.rsplit('/').next().unwrap_or(&p.obj.path).to_string();
    let rel = format!("user/src/{export}.c");
    let path: PathBuf = dir.join(&rel);
    if path.exists() && !force {
        bail!("{rel} exists (yours, maybe edited); remove it or pass --force to overwrite it");
    }
    let mut fields = vec![("object", json::s(&objname)), ("sha256", json::s(&p.obj.id)), key];
    if what.starts_with("0x") {
        fields.push(("size", json::s(format!("{size:#x}"))));
    }
    fields.push(("bytes", json::s(&bytes)));
    fields.push(("export", json::s(&export)));
    fields.push(("abi", json::s("entry")));
    let entry = json::write(&obj(fields));
    let head = format!(
        "/* {name} ({} + {:#x}, at {addr:#x}), decompiled by `gexc eject decompile`.\n \
         *\n \
         * The function tier's C for the function{}: edit it freely, it is yours (a\n \
         * re-eject never touches user/). `{export}` is the function: it does what the\n \
         * guest function does, its return included, and leaves the context's PC where\n \
         * the guest goes next. A call to code this file does not hold writes every\n \
         * register back, sets PC to the callee and returns: the rest of the function\n \
         * then runs as the original translated code. Bind it in {CONFIG_FILE}:\n \
         *\n{} */\n\n",
        objname,
        addr.wrapping_sub(p.image.bias),
        if funcs.len() > 1 { format!(" and the {} it calls directly", funcs.len() - 1) } else { String::new() },
        entry.lines().map(|l| format!(" *   {l}\n")).collect::<String>(),
    );
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, format!("{head}{body}")).with_context(|| format!("writing {rel}"))?;
    Ok(Decompiled { file: rel, entry, functions: funcs.len() })
}
