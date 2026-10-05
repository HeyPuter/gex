//! Embeds `libs/` (the stateful providers an eject can set up, such as the
//! Lua provider) so `gexc eject` can write them into a project without the
//! gex source tree.
use std::io::Write;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut v: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    v.sort();
    for p in v {
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn main() {
    let here = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let libs = here.join("libs");
    println!("cargo:rerun-if-changed={}", libs.display());
    let mut f = std::fs::File::create(PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("libs.rs")).unwrap();
    writeln!(f, "/// (library, path under it, bytes, executable).").unwrap();
    writeln!(f, "pub const LIBS: &[(&str, &str, &[u8], bool)] = &[").unwrap();
    let mut files = Vec::new();
    if libs.is_dir() {
        walk(&libs, &mut files);
    }
    for p in files {
        println!("cargo:rerun-if-changed={}", p.display());
        let rel = p.strip_prefix(&libs).unwrap();
        let mut c = rel.components();
        let lib = c.next().unwrap().as_os_str().to_string_lossy().into_owned();
        let path = c.as_path().to_string_lossy().into_owned();
        let exec = p.extension().is_some_and(|e| e == "sh");
        writeln!(f, "    ({lib:?}, {path:?}, include_bytes!({:?}), {exec}),", p.display().to_string()).unwrap();
    }
    writeln!(f, "];").unwrap();
}
