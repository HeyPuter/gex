//! Find libvirglrenderer, or set no cfg so the rest of the build knows it is
//! absent. A cfg rather than a cargo feature: whether the library exists is a
//! property of the machine. The FFI is hand-written, so only the `.so` is
//! needed, not the `-dev` package's header and `.pc` file.

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

    // `-lvirglrenderer` needs an unversioned `libvirglrenderer.so`, which
    // belongs to the -dev package; make our own symlink in OUT_DIR so a
    // machine that can only *run* virglrenderer still links. `rustc-link-lib`,
    // not `rustc-link-arg`: only the former carries to the final binary.
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
