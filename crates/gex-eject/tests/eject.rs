//! End to end: a session for a tiny static program is ejected (twice, to
//! check the output is reproducible), built, run, and then run again with
//! its function replaced by a wasm one.

use gex_eject::{build, eject, run};
use gex_native::Exit;
use gex_xlate::profile::{FileEntry, ObjectEntry, ProgramEntry, Session};
use std::path::{Path, PathBuf};

const BASE: u64 = 0x40_0000;
/// Where the code starts: after the ELF header and the one program header.
const CODE: u64 = 64 + 56;

/// The program: `exit_group(f(40, 2))`, where `f` subtracts. Returns the code
/// and `f`'s offset in it.
fn program() -> (Vec<u8>, u64) {
    if gex_abi::X86 {
        (
            vec![
                0xbf, 40, 0, 0, 0, // mov edi, 40
                0xbe, 2, 0, 0, 0, // mov esi, 2
                0xe8, 9, 0, 0, 0, // call f
                0x89, 0xc7, // mov edi, eax
                0xb8, 231, 0, 0, 0, // mov eax, 231 (exit_group)
                0x0f, 0x05, // syscall
                0x48, 0x89, 0xf8, // f: mov rax, rdi
                0x48, 0x29, 0xf0, // sub rax, rsi
                0xc3, // ret
            ],
            24,
        )
    } else {
        let code: [u32; 7] = [
            0xd2800500, // movz x0, #40
            0xd2800041, // movz x1, #2
            0x94000003, // bl f
            0xd2800bc8, // movz x8, #94 (exit_group)
            0xd4000001, // svc #0
            0xcb010000, // f: sub x0, x0, x1
            0xd65f03c0, // ret
        ];
        (code.iter().flat_map(|w| w.to_le_bytes()).collect(), 20)
    }
}

/// A static `ET_EXEC` with one RX segment and a symbol table naming `f`.
fn elf() -> Vec<u8> {
    let (code, f_off) = program();
    let mut b = vec![0u8; CODE as usize];
    b.extend_from_slice(&code);
    let seg_len = b.len() as u64;
    let strtab_off = b.len() as u64;
    b.extend_from_slice(b"\0f\0");
    while b.len() % 8 != 0 {
        b.push(0);
    }
    let symtab_off = b.len() as u64;
    b.extend_from_slice(&[0u8; 24]);
    let mut sym = Vec::new();
    sym.extend_from_slice(&1u32.to_le_bytes()); // st_name: "f"
    sym.push(0x12); // STB_GLOBAL, STT_FUNC
    sym.push(0);
    sym.extend_from_slice(&0xfff1u16.to_le_bytes()); // SHN_ABS
    sym.extend_from_slice(&(BASE + CODE + f_off).to_le_bytes());
    sym.extend_from_slice(&(code.len() as u64 - f_off).to_le_bytes());
    b.extend_from_slice(&sym);
    let shoff = b.len() as u64;
    let sh = |ty: u32, off: u64, size: u64, link: u32, entsize: u64| {
        let mut s = vec![0u8; 64];
        s[4..8].copy_from_slice(&ty.to_le_bytes());
        s[24..32].copy_from_slice(&off.to_le_bytes());
        s[32..40].copy_from_slice(&size.to_le_bytes());
        s[40..44].copy_from_slice(&link.to_le_bytes());
        s[56..64].copy_from_slice(&entsize.to_le_bytes());
        s
    };
    b.extend(sh(0, 0, 0, 0, 0));
    b.extend(sh(2, symtab_off, 48, 2, 24)); // .symtab, strings in section 2
    b.extend(sh(3, strtab_off, 3, 0, 0)); // .strtab
    // The ELF header.
    b[0..4].copy_from_slice(b"\x7fELF");
    b[4] = 2; // 64-bit
    b[5] = 1; // little-endian
    b[6] = 1;
    b[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    b[18..20].copy_from_slice(&gex_abi::ELF_MACHINE.to_le_bytes());
    b[20..24].copy_from_slice(&1u32.to_le_bytes());
    b[24..32].copy_from_slice(&(BASE + CODE).to_le_bytes()); // e_entry
    b[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    b[40..48].copy_from_slice(&shoff.to_le_bytes());
    b[52..54].copy_from_slice(&64u16.to_le_bytes());
    b[54..56].copy_from_slice(&56u16.to_le_bytes());
    b[56..58].copy_from_slice(&1u16.to_le_bytes());
    b[58..60].copy_from_slice(&64u16.to_le_bytes());
    b[60..62].copy_from_slice(&3u16.to_le_bytes());
    // PT_LOAD, R+X, the header and the code.
    let p = 64;
    b[p..p + 4].copy_from_slice(&1u32.to_le_bytes());
    b[p + 4..p + 8].copy_from_slice(&5u32.to_le_bytes());
    b[p + 16..p + 24].copy_from_slice(&BASE.to_le_bytes());
    b[p + 24..p + 32].copy_from_slice(&BASE.to_le_bytes());
    b[p + 32..p + 40].copy_from_slice(&seg_len.to_le_bytes());
    b[p + 40..p + 48].copy_from_slice(&seg_len.to_le_bytes());
    b[p + 48..p + 56].copy_from_slice(&0x1000u64.to_le_bytes());
    b
}

/// `(func (export "add") (param i64 i64) (result i64) local.get 0 local.get 1 i64.add)`
const ADD_WASM: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
    0x01, 0x07, 0x01, 0x60, 0x02, 0x7e, 0x7e, 0x01, 0x7e, // type
    0x03, 0x02, 0x01, 0x00, // func
    0x07, 0x07, 0x01, 0x03, b'a', b'd', b'd', 0x00, 0x00, // export
    0x0a, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x7c, 0x0b, // code
];

fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.push((e.path().strip_prefix(dir).unwrap().to_path_buf(), std::fs::read(e.path()).unwrap()));
            }
        }
    }
    out.sort();
    out
}

/// Decompile `f`, compile it as the project's build.sh would, bind it, and
/// build. False (skipped) without clang and wasm-ld.
fn decompile_round_trip(p: &Path, id: &str) -> bool {
    let have = |t: &str| std::process::Command::new(t).arg("--version").output().is_ok_and(|o| o.status.success());
    if !have("clang") || !have("wasm-ld") {
        eprintln!("skip: decompile round trip needs clang and wasm-ld");
        return false;
    }
    let d = gex_eject::decompile::decompile(p, "prog", "f", None, false, false).unwrap();
    assert!(d.entry.contains("\"abi\": \"entry\""), "{}", d.entry);
    assert!(d.entry.contains(id));
    let c = p.join(&d.file);
    let src = std::fs::read_to_string(&c).unwrap();
    assert!(src.contains("decompiled_f(int ctx)") && !src.contains("gexc_dispatch(ctx)"), "{src}");
    let obj = p.join("build/f.o");
    std::fs::create_dir_all(p.join("build")).unwrap();
    let ok = std::process::Command::new("clang")
        .args(gex_cgen::toolchain::CFLAGS)
        .arg("-fvisibility=default")
        .arg(format!("-I{}", p.join("gen").display()))
        .arg("-c")
        .arg(&c)
        .arg("-o")
        .arg(&obj)
        .status()
        .unwrap();
    assert!(ok.success(), "clang failed on the decompiled C");
    let ok = std::process::Command::new("wasm-ld")
        .args(["--no-entry", "--export-dynamic", "--import-memory", "--shared-memory"])
        .arg(format!("--max-memory={}", gex_abi::GUEST_MEM_BYTES))
        .arg(format!("--global-base={}", gex_native::aot::REPL_BASE))
        .arg("--export=__stack_pointer")
        .arg("-o")
        .arg(p.join("build/user.wasm"))
        .arg(&obj)
        .status()
        .unwrap();
    assert!(ok.success(), "wasm-ld failed on the decompiled C");
    std::fs::write(p.join("user/eject.json"), format!("{{\"replace\": [{}]}}", d.entry)).unwrap();
    let r = build::build(p).unwrap();
    assert_eq!(r.replacements[0].abi, gex_native::aot::Abi::Entry);
    // Not twice: the file is the user's now.
    assert!(gex_eject::decompile::decompile(p, "prog", "f", None, false, false).is_err());
    true
}

#[test]
fn eject_build_run_and_replace() {
    let tmp = std::env::temp_dir().join(format!("gex-eject-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("rec")).unwrap();
    let exe = tmp.join("prog");
    let bytes = elf();
    std::fs::write(&exe, &bytes).unwrap();
    let path = exe.to_string_lossy().into_owned();
    let id = eject::sha256(&bytes);

    let session = Session {
        isa: gex_abi::ARCH_NAME.into(),
        argv: vec![path.clone()],
        env: vec!["HOME=/".into()],
        cwd: Some("/".into()),
        objects: vec![ObjectEntry {
            id: id.clone(),
            path: path.clone(),
            host_path: None,
            size: bytes.len() as u64,
            role: "exe".into(),
        }],
        programs: vec![ProgramEntry {
            pid: 1,
            exe: path.clone(),
            argv: vec![path.clone()],
            env: vec![],
        }],
        processes: 1,
        threads: 1,
        exit: Some("status 38".into()),
        ..Session::default()
    };
    std::fs::write(tmp.join("rec/session.json"), session.to_json()).unwrap();

    let opts = eject::EjectOptions {
        runtime: std::env::current_exe().unwrap(),
        runtime_build: None,
        web_shim: None,
        web_page: None,
    };
    let (p, p2) = (tmp.join("p"), tmp.join("p2"));
    eject::eject(&tmp.join("rec"), &p, &opts).unwrap();
    eject::eject(&tmp.join("rec"), &p2, &opts).unwrap();
    assert_eq!(files(&p), files(&p2), "two ejects of one session differ");

    build::build(&p).unwrap();
    // GEX_EJECT_BUILD's view of it: the build's modules, and its memory
    // whatever was learned; --mem64 asked of it is refused, naming --mem32.
    let (aot, pins) = run::debug_build(&p).unwrap();
    // With no replacements the program is a function-tier unit: no image module.
    let built = std::fs::read_to_string(p.join("build/build.json")).unwrap();
    assert!(aot.images.is_empty() && built.contains("\"elf-unit\"") && pins.is_empty(), "a static program's build: its unit, no pins");
    assert!(!run::build_mem(None).unwrap().is_64() && !run::build_mem(Some(false)).unwrap().is_64());
    let e = run::build_mem(Some(true)).unwrap_err().to_string();
    assert!(e.contains("--mem32") && !e.contains('\n'), "{e}");
    let flags = run::RunFlags::default();
    // The session made no system calls, so the ejected runtime allows none:
    // its exit_group is refused, by name.
    match run::run(&p, &flags).unwrap() {
        Exit::Signal { signal: 31, detail, internal: true, .. } => assert!(detail.contains("exit_group"), "{detail}"),
        other => panic!("{other:?}"),
    }
    // As the real session would have it.
    let mut session = session;
    session.syscalls.insert(94, 1); // exit_group, canonical
    std::fs::write(tmp.join("rec/session.json"), session.to_json()).unwrap();
    eject::eject(&tmp.join("rec"), &p, &opts).unwrap();
    build::build(&p).unwrap();
    assert_eq!(run::run(&p, &flags).unwrap(), Exit::Status(38));

    // Replace `f` with wasm that adds.
    std::fs::write(p.join("user/add.wasm"), ADD_WASM).unwrap();
    let config = format!(
        r#"{{"replace": [{{"object": "prog", "sha256": "{id}", "symbol": "f",
             "wasm": "user/add.wasm", "export": "add"}}]}}"#
    );
    std::fs::write(p.join("user/eject.json"), &config).unwrap();
    // Not rebuilt yet: refused.
    assert!(run::run(&p, &flags).unwrap_err().to_string().contains("changed since it was built"));
    let r = build::build(&p).unwrap();
    assert_eq!(r.replacements.len(), 1);
    assert_eq!(r.replacements[0].addr, BASE + CODE + program().1);
    assert_eq!(run::run(&p, &flags).unwrap(), Exit::Status(42));

    // The same function by offset, with the hash of its bytes: what
    // `gexc eject override` prints for a stripped object.
    let off = BASE + CODE + program().1;
    let entry = build::describe(&p, "prog", &format!("{off:#x}"), Some(program().0.len() as u64 - program().1))
        .unwrap()
        .replace(&format!("\"fn_{off:x}\""), "\"add\"")
        .replace("\"abi\"", "\"wasm\": \"user/add.wasm\", \"abi\"");
    std::fs::write(p.join("user/eject.json"), format!("{{\"replace\": [{entry}]}}")).unwrap();
    let r = build::build(&p).unwrap();
    assert_eq!((r.replacements[0].kind.as_str(), r.replacements[0].addr), ("offset", off));
    assert_eq!(run::run(&p, &flags).unwrap(), Exit::Status(42));
    // A function whose bytes changed is reported, not bound.
    let hash = entry.split("\"bytes\": \"").nth(1).unwrap().split('"').next().unwrap().to_string();
    std::fs::write(
        p.join("user/eject.json"),
        format!("{{\"replace\": [{}]}}", entry.replace(&hash, "sha256:00")),
    )
    .unwrap();
    let e = format!("{:#}", build::build(&p).unwrap_err());
    assert!(e.contains("stale") && e.contains(&hash), "{e}");
    std::fs::write(p.join("user/eject.json"), &config).unwrap();

    // Decompiled: the function tier's C for `f`, compiled as build.sh does
    // and bound with the entry ABI, behaves exactly as `f` (40 - 2).
    if decompile_round_trip(&p, &id) {
        assert_eq!(run::run(&p, &flags).unwrap(), Exit::Status(38));
    }
    std::fs::write(p.join("user/eject.json"), &config).unwrap();

    // A re-eject keeps user/ and drops the build.
    eject::eject(&tmp.join("rec"), &p, &opts).unwrap();
    assert_eq!(std::fs::read_to_string(p.join("user/eject.json")).unwrap(), config);
    assert!(!p.join("build").exists());

    // A replacement written for another version of the object is refused.
    std::fs::write(p.join("user/eject.json"), config.replace(&id, "sha256:00")).unwrap();
    let e = format!("{:#}", build::build(&p).unwrap_err());
    assert!(e.contains("stale"), "{e}");

    // So is an edited rootfs.
    std::fs::write(p.join("user/eject.json"), &config).unwrap();
    let obj = p.join(gex_eject::project::rootfs_rel(&path).unwrap());
    let mut changed = bytes.clone();
    *changed.last_mut().unwrap() ^= 1;
    std::fs::write(&obj, changed).unwrap();
    let e = format!("{:#}", build::build(&p).unwrap_err());
    assert!(e.contains("changed since the eject"), "{e}");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// A file the session only looked at (stat, access, readlink) is an input
/// too: a QML source Qt checks the time of and never opens, behind its
/// cached compilation. Kept when it is there, a directory made; a failed
/// look stays missing, and one at something gone since is not an input.
#[test]
fn eject_keeps_files_only_looked_at() {
    // Not under /tmp: a project's /tmp is a fresh tmpfs, so nothing there is an input.
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gex-eject-stat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("rec")).unwrap();
    std::fs::create_dir_all(tmp.join("in/ui")).unwrap();
    let exe = tmp.join("prog");
    let bytes = elf();
    std::fs::write(&exe, &bytes).unwrap();
    std::fs::write(tmp.join("in/ui/main.qml"), b"Item {}").unwrap();
    let path = exe.to_string_lossy().into_owned();
    let at = |rel: &str| tmp.join(rel).to_string_lossy().into_owned();
    let looked = |p: String, ok: u64| FileEntry { path: p, stat: true, ok, failed: 1 - ok, ..Default::default() };
    let mut session = Session {
        isa: gex_abi::ARCH_NAME.into(),
        argv: vec![path.clone()],
        cwd: Some("/".into()),
        objects: vec![ObjectEntry {
            id: eject::sha256(&bytes),
            path: path.clone(),
            host_path: None,
            size: bytes.len() as u64,
            role: "exe".into(),
        }],
        programs: vec![ProgramEntry { pid: 1, exe: path.clone(), argv: vec![path.clone()], env: vec![] }],
        processes: 1,
        threads: 1,
        ..Session::default()
    };
    for f in [
        looked(at("in/ui/main.qml"), 1),
        looked(at("in/ui"), 1),
        looked(at("in/none.qml"), 0),
        // There when the session looked, gone by the eject.
        looked(at("in/gone.qml"), 1),
        // A stat of `dir/..`: the directory is reached by its own path.
        looked(at("in/ui/.."), 1),
    ] {
        session.files.insert(f.path.clone(), f);
    }
    // Through the session's JSON, as `gexc eject` reads it.
    let text = session.to_json();
    assert_eq!(Session::from_json(&text).unwrap().files, session.files, "the stat flag round-trips");
    std::fs::write(tmp.join("rec/session.json"), text).unwrap();
    let opts = eject::EjectOptions { runtime: std::env::current_exe().unwrap(), runtime_build: None, web_shim: None, web_page: None };
    let p = tmp.join("p");
    eject::eject(&tmp.join("rec"), &p, &opts).unwrap();
    let root = |rel: &str| p.join("rootfs").join(at(rel).trim_start_matches('/'));
    assert_eq!(std::fs::read(root("in/ui/main.qml")).unwrap(), b"Item {}");
    assert!(root("in/ui").is_dir());
    assert!(!root("in/none.qml").exists() && !root("in/gone.qml").exists());
    let project = std::fs::read_to_string(p.join("gex-project.json")).unwrap();
    assert!(project.contains("none.qml") && !project.contains("gone.qml"), "a failed look stays missing; a vanished one is nothing");
    let _ = std::fs::remove_dir_all(&tmp);
}
