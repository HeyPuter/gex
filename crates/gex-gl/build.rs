//! Builds native/gexgl-native.cpp, the native host's decoder for the
//! generated GL commands (the same cases the browser's Mesa decoder runs),
//! when the `host` feature is on and the target has a host GL.

fn main() {
    println!("cargo:rerun-if-changed=native/gexgl-native.cpp");
    println!("cargo:rerun-if-changed=native/hand-cases.inc");
    println!("cargo:rerun-if-changed=src/proto.rs");
    for f in [
        "../../ports/mesa/src/gl44-generated-cases.inc",
        "../../ports/mesa/src/gl-compat-generated-cases.inc",
        "../../ports/mesa/src/gexgl-compat-host.h",
        "../../guest/gl/gexgl-plan.h",
        "../../guest/gl/gexgl-pixel.h",
        "../../guest/gl/gl-pname-counts.h",
    ] {
        println!("cargo:rerun-if-changed={f}");
    }
    let host = std::env::var_os("CARGO_FEATURE_HOST").is_some();
    let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32");
    if !host || wasm {
        return;
    }
    // The 4.4 cases call some GL functions directly, which the browser's
    // Mesa links statically; here every one goes through the proc table
    // (native/gexgl-native.cpp's GEX_GL_CALL), as the native host links only
    // EGL and GLES.
    let cases = std::fs::read_to_string("../../ports/mesa/src/gl44-generated-cases.inc").unwrap_or_default()
        + &std::fs::read_to_string("native/hand-cases.inc").unwrap_or_default();
    let mut direct = std::collections::BTreeSet::new();
    let b = cases.as_bytes();
    for (i, _) in cases.match_indices("gl") {
        let starts = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
        if !starts || !b.get(i + 2).is_some_and(|c| c.is_ascii_uppercase()) {
            continue;
        }
        let end = i + cases[i..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(0);
        if b.get(end) == Some(&b'(') {
            direct.insert(&cases[i..end]);
        }
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let header: String =
        direct.iter().map(|f| format!("#define {f}(...) GEX_GL_CALL({f}, __VA_ARGS__)\n")).collect();
    std::fs::write(out.join("gexgl-direct-calls.h"), header).unwrap();
    // The hand-written opcodes' names, from proto.rs, for hand-cases.inc.
    let proto = std::fs::read_to_string("src/proto.rs").unwrap();
    let ops = &proto[proto.find("pub mod op {").unwrap()..];
    let ops = &ops[..ops.find("\n}").unwrap()];
    let mut names = String::new();
    for line in ops.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("pub const ") {
            if let Some((name, value)) = rest.split_once(": u32 = ") {
                let value = value.split(';').next().unwrap_or("0").trim();
                names += &format!("    {name} = {value},\n");
            }
        }
    }
    std::fs::write(out.join("gexgl-hand-ops.h"), names).unwrap();
    let mut build = cc::Build::new();
    // macOS has no system EGL or desktop GL headers; Homebrew's mesa has
    // both, and the libEGL/libGLESv2 that src/host.rs links.
    // `GEX_MESA_PREFIX` overrides where it is looked for.
    println!("cargo:rerun-if-env-changed=GEX_MESA_PREFIX");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let prefix = std::env::var_os("GEX_MESA_PREFIX").map(std::path::PathBuf::from).or_else(|| {
            ["/opt/homebrew/opt/mesa", "/usr/local/opt/mesa"]
                .into_iter()
                .map(std::path::PathBuf::from)
                .find(|p| p.join("include/EGL/egl.h").exists())
        });
        let Some(prefix) = prefix else {
            panic!("gex-gl: no EGL/GL headers on macOS; `brew install mesa` or set GEX_MESA_PREFIX");
        };
        build.include(prefix.join("include"));
        println!("cargo:rustc-link-search=native={}", prefix.join("lib").display());
    }
    build
        .cpp(true)
        .include(&out)
        .std("c++17")
        .file("native/gexgl-native.cpp")
        .flag_if_supported("-Wno-unused-function")
        .flag_if_supported("-Wno-unused-variable")
        .flag_if_supported("-Wno-unused-parameter")
        .opt_level(2)
        .compile("gexgl_native");
}
