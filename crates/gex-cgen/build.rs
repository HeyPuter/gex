//! `GEX_CGEN_INPUTS`: a hash of every source that decides what a
//! function-tier unit holds -- recovery (gex-xlate's discovery, the lifters,
//! the decoders), the IR, and this crate's C -- so a unit cache keyed on it
//! (gex_native::nt_cgen) survives rebuilds that change nothing it depends on,
//! and is refused after any that do.
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

fn main() {
    let here = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let crates = here.parent().unwrap();
    let mut files = Vec::new();
    for c in [
        "gex-cgen", "gex-ir", "gex-xlate", "gex-lift", "gex-lift-x86", "gex-lift-ppc", "gex-isa", "gex-isa-x86",
        "gex-isa-ppc", "gex-abi", "gex-ld", "gex-emit",
    ] {
        let src = crates.join(c).join("src");
        println!("cargo:rerun-if-changed={}", src.display());
        walk(&src, &mut files);
    }
    walk(&here.join("runtime"), &mut files);
    println!("cargo:rerun-if-changed={}", here.join("runtime").display());
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
    println!("cargo:rustc-env=GEX_CGEN_INPUTS={a:016x}{:016x}", h2.finish());
}
