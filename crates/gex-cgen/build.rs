//! Two hashes of the sources a function-tier unit depends on (gex_native::
//! nt_cgen keys its caches on them; docs/function-tier.md §11):
//!
//! * `GEX_CGEN_RECOVERY_INPUTS`: what decides an image's recovery (its GIR
//!   and summaries) -- the IR, the lifters and decoders, gex-xlate's recovery
//!   (everything in it but the runtime translator's `lib.rs` and the
//!   profile and validation code), gex-abi, and gex-ld (placing the image);
//! * `GEX_CGEN_UNIT_INPUTS`: what decides the C and the module from a
//!   recovery -- this crate (and its runtime) and gex-abi. What the C embeds
//!   from elsewhere (gex-emit's constants) is pinned by `UNIT_ABI_VERSION`
//!   and its test, not hashed, so a tier-1 change does not invalidate units.
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs" || x == "h") {
            out.push(p);
        }
    }
}

fn hash(crates: &Path, mut files: Vec<PathBuf>) -> String {
    files.sort();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for f in &files {
        f.strip_prefix(crates).unwrap_or(f).hash(&mut h);
        std::fs::read(f).unwrap_or_default().hash(&mut h);
    }
    let a = h.finish();
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    a.hash(&mut h2);
    files.len().hash(&mut h2);
    format!("{a:016x}{:016x}", h2.finish())
}

fn main() {
    let here = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let crates = here.parent().unwrap().to_path_buf();
    let src = |c: &str| {
        let d = crates.join(c).join("src");
        println!("cargo:rerun-if-changed={}", d.display());
        let mut v = Vec::new();
        walk(&d, &mut v);
        v
    };
    let mut rec = Vec::new();
    for c in ["gex-ir", "gex-lift", "gex-lift-x86", "gex-lift-ppc", "gex-isa", "gex-isa-x86", "gex-isa-ppc", "gex-abi", "gex-ld"] {
        rec.extend(src(c));
    }
    rec.extend(src("gex-xlate").into_iter().filter(|p| {
        !matches!(p.file_name().and_then(|n| n.to_str()), Some("lib.rs" | "profile.rs" | "validate.rs"))
    }));
    let mut unit = src("gex-cgen");
    unit.extend(src("gex-abi"));
    let rt = here.join("runtime");
    println!("cargo:rerun-if-changed={}", rt.display());
    walk(&rt, &mut unit);
    println!("cargo:rustc-env=GEX_CGEN_RECOVERY_INPUTS={}", hash(&crates, rec));
    println!("cargo:rustc-env=GEX_CGEN_UNIT_INPUTS={}", hash(&crates, unit));
}
