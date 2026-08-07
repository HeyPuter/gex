//! Find libvirglrenderer, or tell the rest of the build it is not here.
//!
//! virglrenderer is a system library that gex does not vendor, so a machine
//! without it must still build everything else. The switch is a cfg rather
//! than a cargo feature because whether the library exists is a property of
//! the machine, not a choice made on the command line.
//!
//! The FFI declarations are hand-written, so the header is never needed --
//! only the shared object. That is worth knowing because distributions split
//! the two: the runtime library is installed far more often than the `-dev`
//! package that carries the header, the `.pc` file and the bare `.so`
//! symlink. Linking against the soname directly, with `-l:`, means a machine
//! that can *run* virglrenderer can also build against it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The soname, which is what a machine without the -dev package actually has.
const SONAME: &str = "libvirglrenderer.so.1";

fn main() {
    println!("cargo::rustc-check-cfg=cfg(have_virgl)");
    for var in ["GEX_NO_VIRGL", "GEX_VIRGL_LIB_DIR", "PKG_CONFIG_PATH"] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    if std::env::var_os("GEX_NO_VIRGL").is_some() {
        return;
    }

    let Some(dir) = find_lib() else {
        println!(
            "cargo:warning=libvirglrenderer not found; the virtio-gpu path \
             will be unavailable (set GEX_VIRGL_LIB_DIR to point at it)"
        );
        return;
    };

    // `-lvirglrenderer` needs an unversioned `libvirglrenderer.so`, and that
    // symlink belongs to the -dev package, which a machine that can happily
    // *run* virglrenderer often does not have. Making our own in OUT_DIR
    // costs nothing and keeps the build working on both kinds of machine.
    //
    // It has to be `rustc-link-lib` rather than `rustc-link-arg`: only the
    // former is carried to whatever binary eventually links this crate.
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let link = out.join("libvirglrenderer.so");
    let _ = std::fs::remove_file(&link);
    if let Err(e) = std::os::unix::fs::symlink(dir.join(SONAME), &link) {
        println!("cargo:warning=could not link virglrenderer: {e}");
        return;
    }
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=virglrenderer");
    // The winsys handed to virglrenderer is ours, so its EGL is linked here.
    println!("cargo:rustc-link-lib=EGL");
    println!("cargo::rustc-cfg=have_virgl");
}

/// The directory holding the shared object, if it is anywhere obvious.
fn find_lib() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GEX_VIRGL_LIB_DIR") {
        let dir = PathBuf::from(dir);
        if dir.join(SONAME).exists() {
            return Some(dir);
        }
    }

    // pkg-config when the -dev package is installed, which is the tidy case.
    if let Ok(out) = Command::new("pkg-config")
        .args(["--variable=libdir", "virglrenderer"])
        .output()
    {
        let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        if dir.join(SONAME).exists() {
            return Some(dir);
        }
    }

    // And the places a multiarch system puts it.
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let triple = match arch.as_str() {
        "x86_64" => "x86_64-linux-gnu",
        "aarch64" => "aarch64-linux-gnu",
        _ => "",
    };
    for dir in [
        format!("/usr/lib/{triple}"),
        format!("/usr/local/lib/{triple}"),
        "/usr/lib".into(),
        "/usr/local/lib".into(),
        "/usr/lib64".into(),
    ] {
        let dir = Path::new(&dir);
        if dir.join(SONAME).exists() {
            return Some(dir.to_path_buf());
        }
    }
    None
}
