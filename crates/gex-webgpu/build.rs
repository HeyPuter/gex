//! Builds guest/gpu/wire-server.c against webgpu.h and links Dawn's
//! `libwebgpu_dawn.a` from `GEX_DAWN_PREFIX` (a Dawn release's layout:
//! `lib/libwebgpu_dawn.a`), when the `dawn` feature is on.

fn main() {
    let gpu = std::path::Path::new("../../guest/gpu");
    for f in ["wire-server.c", "wire-server.h", "wire.h", "gen/wire-gen.h", "gen/wire-server-gen.inc"] {
        println!("cargo:rerun-if-changed={}", gpu.join(f).display());
    }
    println!("cargo:rerun-if-changed=native/server-dawn.c");
    println!("cargo:rerun-if-env-changed=GEX_DAWN_PREFIX");
    if std::env::var_os("CARGO_FEATURE_DAWN").is_none() {
        return;
    }
    let prefix = std::env::var_os("GEX_DAWN_PREFIX")
        .map(std::path::PathBuf::from)
        .expect("gex-webgpu: set GEX_DAWN_PREFIX to a Dawn release (lib/libwebgpu_dawn.a)");
    cc::Build::new()
        .include(gpu)
        .include("../../third_party/dawn-api")
        .file(gpu.join("wire-server.c"))
        .file("native/server-dawn.c")
        .flag_if_supported("-Wno-unused-function")
        .flag_if_supported("-Wno-unused-parameter")
        .opt_level(2)
        .compile("gexgpu_server");
    println!("cargo:rustc-link-search=native={}", prefix.join("lib").display());
    println!("cargo:rustc-link-lib=static=webgpu_dawn");
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => {
            println!("cargo:rustc-link-lib=c++");
            for f in ["Metal", "QuartzCore", "IOSurface", "IOKit", "Foundation", "CoreFoundation", "CoreGraphics"] {
                println!("cargo:rustc-link-lib=framework={f}");
            }
        }
        _ => println!("cargo:rustc-link-lib=stdc++"),
    }
}
