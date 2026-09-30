//! gex's PE loader against pewasm's, on the same fixtures.
//!
//! For each fixture this runs `pewasm.py <exe> --out <tmp>` (no `--build`),
//! then loads the same program with gex, pinning every module at the base
//! pewasm chose, and checks per module that:
//!
//! - the headers agree: preferred base, entry, image size, machine, sections;
//! - the mapped images are byte-identical outside the IAT slots (pewasm leaves
//!   IATs unbound; gex fills them). That covers section copying, every base
//!   relocation and the rewritten `ImageBase`;
//! - every IAT slot names the same DLL and symbol, and resolves to the same
//!   address when pewasm resolved it to lifted code (`kind: lifted`) or to a
//!   placeholder when pewasm bound it to its host (`host`/`unsupported`);
//! - exports (names and ordinals, forwarders excluded), TLS callbacks, `.pdata`
//!   and delay imports (slot, thunk, target) match;
//! - the code pointers pewasm derives from relocations match gex's.
//!
//! Differences that are pewasm's limits, not gex bugs, are handled by
//! adjusting the input (each is listed in `cases`):
//!
//! - pewasm insists that the delay-import directory's size include its null
//!   terminator; MinGW's does not. The x86 `uses.exe` is compared as a copy
//!   whose directory size is widened by one descriptor.
//! - pewasm has no PE32+ delay imports. The x64 `uses.exe` is compared as a
//!   copy with the delay-import directory cleared (gex's own delay handling
//!   on PE32+ is covered by `tests/pe.rs`).
//! - pewasm loads delay-imported DLLs only with `--extra-dll`, which is passed.
//! - pewasm places every image itself (packed from 0x10000) and cannot move an
//!   image without a `.reloc` section, which gex can when the image is
//!   `DYNAMIC_BASE`; the fixture DLLs all carry relocations so both can.
//! - pewasm's x86 runtime generation crashes on UCRT imports it has no host
//!   binding for (the x86 `uses.exe`); that is after its loader has written
//!   `images/module*.json`, so the images are compared and the crash noted.
//!
//! Skipped (and says so) when no pewasm checkout with a `.venv` is found next
//! to this repository, unless `GEX_REQUIRE_PEWASM=1`.

use gex_ld::pe::{self, PeOptions, PeProgram, Target};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/pe/bin")
}

fn find_pewasm() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PEWASM") {
        return Some(PathBuf::from(p));
    }
    let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while d.pop() {
        let p = d.join("pewasm");
        if p.join("pewasm.py").is_file() && p.join(".venv/bin/python").is_file() {
            return Some(p);
        }
    }
    None
}

/// How a fixture's input is adjusted before both loaders see it.
#[derive(Clone, Copy, PartialEq)]
enum Patch {
    None,
    /// Widen the delay-import directory to include its terminator.
    DelaySizeWithTerminator,
    /// Clear the delay-import data directory.
    NoDelay,
}

struct Case {
    name: String,
    exe: PathBuf,
    /// Run pewasm's loader (`load_modules` and `prepare_images`) directly
    /// instead of `pewasm.py`, which would go on to lift the whole program:
    /// for the large real programs named in `GEX_PEWASM_EXTRA`. The exe is
    /// used where it is, unpatched.
    direct: bool,
    patch: Patch,
    extra: Vec<PathBuf>,
    /// Both loaders must refuse it; pewasm's message contains this.
    expect_failure: Option<&'static str>,
}

fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    for arch in ["x86", "x64"] {
        let mut exes: Vec<PathBuf> = std::fs::read_dir(bin().join(arch))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "exe"))
            .collect();
        exes.sort();
        for exe in exes {
            let stem = exe.file_stem().unwrap().to_string_lossy().into_owned();
            let mut c = Case {
                name: format!("{arch}-{stem}"),
                exe: exe.clone(),
                direct: false,
                patch: Patch::None,
                extra: Vec::new(),
                expect_failure: None,
            };
            if stem == "uses" {
                c.patch = if arch == "x86" { Patch::DelaySizeWithTerminator } else { Patch::NoDelay };
                c.extra.push(bin().join(arch).join("delayed.dll"));
            }
            if stem == "highbase-fixed" {
                c.expect_failure = Some("rebasing requires base relocations");
            }
            out.push(c);
        }
    }
    // Real programs, when asked for: `GEX_PEWASM_EXTRA=/path/a.exe:/path/b.exe`.
    for p in std::env::var("GEX_PEWASM_EXTRA").unwrap_or_default().split(':').filter(|p| !p.is_empty()) {
        let exe = PathBuf::from(p);
        out.push(Case {
            name: format!("extra-{}", exe.file_stem().unwrap().to_string_lossy()),
            exe,
            direct: true,
            patch: Patch::None,
            extra: Vec::new(),
            expect_failure: None,
        });
    }
    out
}

fn patch(bytes: &mut [u8], how: Patch) {
    if how == Patch::None {
        return;
    }
    let img = pe::parse(bytes).unwrap();
    let dd = img.opt_offset + if img.is64 { 112 } else { 96 } + pe::dir::DELAY_IMPORT * 8;
    let size = u32::from_le_bytes(bytes[dd + 4..dd + 8].try_into().unwrap());
    let (rva, size) = match how {
        Patch::DelaySizeWithTerminator => (img.dir(pe::dir::DELAY_IMPORT).0, size + 32),
        Patch::NoDelay => (0, 0),
        Patch::None => unreachable!(),
    };
    bytes[dd..dd + 4].copy_from_slice(&rva.to_le_bytes());
    bytes[dd + 4..dd + 8].copy_from_slice(&size.to_le_bytes());
}

fn hex(v: &Value) -> u64 {
    v.as_u64().unwrap_or_else(|| panic!("not an integer: {v}"))
}

/// Runs one case; returns the differences found (empty when all agree).
fn compare(pewasm: &Path, work: &Path, case: &Case) -> Vec<String> {
    let mut diffs = Vec::new();
    let dir = work.join(&case.name);
    if case.direct {
        let out = dir.join("pewasm");
        std::fs::create_dir_all(out.join("images")).unwrap();
        let script = format!(
            "import sys, pathlib\nsys.path[:0] = [{pw:?}, {tools:?}]\nimport pewasm as P\n\
             mods = P.load_modules(pathlib.Path({exe:?}).resolve(), [])\n\
             P.prepare_images(mods, pathlib.Path({out:?}), 64, 8)\n",
            pw = pewasm.to_string_lossy(),
            tools = pewasm.join("tools").to_string_lossy(),
            exe = case.exe.to_string_lossy(),
            out = out.to_string_lossy(),
        );
        let r = Command::new(pewasm.join(".venv/bin/python")).args(["-c", &script]).output().unwrap();
        if !r.status.success() {
            return vec![format!("pewasm's loader failed: {}", String::from_utf8_lossy(&r.stderr))];
        }
        return compare_output(case, &case.exe, &out, None, PeOptions::default());
    }
    let input = dir.join("in");
    std::fs::create_dir_all(&input).unwrap();
    // The exe's directory is searched for DLLs, so bring the DLLs along.
    for e in std::fs::read_dir(case.exe.parent().unwrap()).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|e| e == "dll") {
            std::fs::copy(&p, input.join(p.file_name().unwrap())).unwrap();
        }
    }
    let exe = input.join(case.exe.file_name().unwrap());
    let mut bytes = std::fs::read(&case.exe).unwrap();
    patch(&mut bytes, case.patch);
    std::fs::write(&exe, &bytes).unwrap();
    let extra: Vec<PathBuf> = case.extra.iter().map(|p| input.join(p.file_name().unwrap())).collect();

    let out = dir.join("pewasm");
    let mut cmd = Command::new(pewasm.join(".venv/bin/python"));
    cmd.arg(pewasm.join("pewasm.py")).arg(&exe).arg("--out").arg(&out).args(["--jobs", "1"]);
    for x in &extra {
        cmd.arg("--extra-dll").arg(x);
    }
    let r = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&r.stderr).into_owned();

    let opts = PeOptions {
        extra_dlls: extra.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
        ..Default::default()
    };
    if let Some(want) = case.expect_failure {
        if r.status.success() || !stderr.contains(want) {
            diffs.push(format!("pewasm should have refused it with {want:?}: {stderr}"));
        }
        if pe::load_closure(&exe.to_string_lossy(), &pe::HostFiles, &opts).is_ok() {
            diffs.push("gex loaded an image pewasm refuses".into());
        }
        return diffs;
    }
    compare_output(case, &exe, &out, Some(&r), opts)
}

/// Compares pewasm's output directory with gex's load of `exe`. `run` is the
/// `pewasm.py` process, when there was one.
fn compare_output(
    case: &Case,
    exe: &Path,
    out: &Path,
    run: Option<&std::process::Output>,
    mut opts: PeOptions,
) -> Vec<String> {
    let mut diffs = Vec::new();
    // pewasm writes images/module*.json once loading and placement are done,
    // before lifting and runtime generation. A failure after that point is
    // not about loading: it is reported as a note, and the images compared.
    let metas: Vec<Value> = (0..)
        .map(|i| out.join(format!("images/module{i}.json")))
        .take_while(|p| p.exists())
        .map(|p| serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap())
        .collect();
    let stderr = run.map(|r| String::from_utf8_lossy(&r.stderr).into_owned()).unwrap_or_default();
    if run.is_some_and(|r| !r.status.success()) {
        if metas.is_empty() || stderr.contains("PEWASM:") {
            return vec![format!("pewasm failed: {stderr}")];
        }
        let last = stderr.lines().last().unwrap_or("");
        eprintln!("{}: note: pewasm failed after writing its images ({last}); comparing those", case.name);
    } else if run.is_some() {
        // The report's module list must agree with the images.
        let project: Value = serde_json::from_slice(&std::fs::read(out.join("project.json")).unwrap()).unwrap();
        let listed: Vec<(String, u64)> = project["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| (m["name"].as_str().unwrap().to_string(), hex(&m["base"])))
            .collect();
        let imaged: Vec<(String, u64)> =
            metas.iter().map(|m| (m["name"].as_str().unwrap().to_string(), hex(&m["base"]))).collect();
        assert_eq!(listed, imaged, "{}: project.json and images disagree", case.name);
    }
    for m in &metas {
        opts.layout.pins.insert(m["name"].as_str().unwrap().to_ascii_lowercase(), hex(&m["base"]));
    }
    let prog = match pe::load_closure(&exe.to_string_lossy(), &pe::HostFiles, &opts) {
        Ok(p) => p,
        Err(e) => return vec![format!("gex failed: {e}")],
    };
    let pw_names: BTreeSet<String> =
        metas.iter().map(|m| m["name"].as_str().unwrap().to_ascii_lowercase()).collect();
    let gex_names: BTreeSet<String> = prog.modules.iter().map(|m| m.name.clone()).collect();
    if pw_names != gex_names {
        diffs.push(format!("closure: pewasm {pw_names:?}, gex {gex_names:?}"));
    }
    for (i, meta) in metas.iter().enumerate() {
        let img = std::fs::read(out.join(format!("images/module{i}.img"))).unwrap();
        let name = meta["name"].as_str().unwrap().to_ascii_lowercase();
        let Some(m) = prog.modules.iter().find(|m| m.name == name) else {
            continue;
        };
        compare_module(&prog, m, meta, &img, &mut diffs);
    }
    diffs
}

fn compare_module(prog: &PeProgram, m: &pe::PeModule, meta: &Value, img: &[u8], diffs: &mut Vec<String>) {
    let n = &m.name;
    fn check(diffs: &mut Vec<String>, n: &str, what: &str, pw: String, gex: String) {
        if pw != gex {
            diffs.push(format!("{n}: {what}: pewasm {pw}, gex {gex}"));
        }
    }
    check(diffs, n, "base", format!("{:#x}", hex(&meta["base"])), format!("{:#x}", m.base));
    check(diffs, n, "preferred base", format!("{:#x}", hex(&meta["original_base"])), format!("{:#x}", m.pe.preferred_base));
    check(diffs, n, "entry rva", format!("{:#x}", hex(&meta["entry_rva"])), format!("{:#x}", m.pe.entry_rva));
    check(diffs, n, "image size", format!("{:#x}", hex(&meta["image_size"])), format!("{:#x}", m.pe.image_size));
    check(diffs, n, "machine", format!("{:#x}", hex(&meta["machine"])), format!("{:#x}", m.pe.machine));
    let pw_secs: Vec<String> = meta["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            format!(
                "{} {:#x} {:#x} {:#x} {:#x}",
                s["name"].as_str().unwrap(),
                hex(&s["rva"]),
                hex(&s["vsize"]),
                hex(&s["raw_size"]),
                hex(&s["chars"])
            )
        })
        .collect();
    let gex_secs: Vec<String> = m
        .pe
        .sections
        .iter()
        .map(|s| format!("{} {:#x} {:#x} {:#x} {:#x}", s.name, s.rva, s.vsize, s.raw_size, s.characteristics))
        .collect();
    check(diffs, n, "sections", format!("{pw_secs:?}"), format!("{gex_secs:?}"));

    // The images, outside the IAT slots gex binds.
    let w = if m.pe.is64 { 8 } else { 4 };
    let mut mask = vec![false; m.image.len()];
    for b in &m.imports {
        let at = (b.slot - m.base) as usize;
        mask[at..at + w].iter_mut().for_each(|x| *x = true);
    }
    if img.len() != m.pe.image_size as usize {
        diffs.push(format!("{n}: pewasm image is {:#x} bytes", img.len()));
    } else {
        let bad: Vec<usize> = (0..img.len()).filter(|&i| !mask[i] && img[i] != m.image[i]).collect();
        if let Some(&first) = bad.first() {
            diffs.push(format!("{n}: {} image bytes differ, first at rva {first:#x}", bad.len()));
        }
    }

    // Imports, slot by slot.
    let pw_imports: HashMap<u64, &Value> =
        meta["imports"].as_array().unwrap().iter().map(|r| (hex(&r["iat_va"]), r)).collect();
    if pw_imports.len() != m.imports.len() {
        diffs.push(format!("{n}: {} import slots in pewasm, {} in gex", pw_imports.len(), m.imports.len()));
    }
    for b in &m.imports {
        let Some(r) = pw_imports.get(&b.slot) else {
            diffs.push(format!("{n}: pewasm has no import slot at {:#x}", b.slot));
            continue;
        };
        let what = format!("import slot {:#x}", b.slot);
        if r["dll"].as_str() != Some(b.dll.as_str()) || r["label"].as_str() != Some(&b.import.pewasm_label()) {
            diffs.push(format!("{n}: {what}: pewasm {}!{}, gex {}!{}", r["dll"], r["label"], b.dll, b.import.pewasm_label()));
        }
        match (r["kind"].as_str().unwrap(), &b.target) {
            ("lifted", Target::Image { va, .. }) if hex(&r["target_va"]) == *va => {}
            ("host" | "unsupported", Target::Placeholder(_)) => {}
            (k, t) => diffs.push(format!("{n}: {what}: pewasm {k} {}, gex {t:?}", r.get("target_va").unwrap_or(&Value::Null))),
        }
    }

    // Exports, in pewasm's labels.
    let pw_exports: BTreeMap<String, u64> =
        meta["exports"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), hex(v))).collect();
    let gex_exports: BTreeMap<String, u64> = m
        .export_addresses()
        .into_iter()
        .map(|(l, v)| (l.strip_prefix('#').map(|o| format!("ordinal_{o}")).unwrap_or(l), v))
        .collect();
    check(diffs, n, "exports", format!("{pw_exports:x?}"), format!("{gex_exports:x?}"));

    let pw_tls: Vec<u64> = meta["tls_callbacks"].as_array().unwrap().iter().map(hex).collect();
    check(diffs, n, "TLS callbacks", format!("{pw_tls:x?}"), format!("{:x?}", m.tls_callbacks));

    let pw_pdata: Vec<String> = meta["pdata"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| format!("{:#x}-{:#x} {:#x} {}", hex(&p["begin"]), hex(&p["end"]), hex(&p["unwind"]), p["flags"]))
        .collect();
    let gex_pdata: Vec<String> = m
        .pe
        .pdata
        .iter()
        .map(|f| {
            format!(
                "{:#x}-{:#x} {:#x} {}",
                m.base + f.begin_rva as u64,
                m.base + f.end_rva as u64,
                m.base + f.unwind as u64,
                f.unwind_flags.map(|x| x as i64).unwrap_or(-1)
            )
        })
        .collect();
    check(diffs, n, "pdata", format!("{pw_pdata:?}"), format!("{gex_pdata:?}"));

    let pw_delay: Vec<String> = meta["delay_imports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            let target = match d["availability"].as_str() {
                Some("lifted") => format!("{:#x}", hex(&d["target_va"])),
                _ => "placeholder".into(),
            };
            format!(
                "{}!{} slot {:#x} thunk {:#x} desc {:#x} hmod {:#x} -> {target}",
                d["dll"].as_str().unwrap(),
                d["label"].as_str().unwrap(),
                hex(&d["iat_va"]),
                hex(&d["thunk_va"]),
                hex(&d["descriptor_va"]),
                hex(&d["module_handle_va"])
            )
        })
        .collect();
    let gex_delay: Vec<String> = m
        .delay_imports
        .iter()
        .map(|d| {
            let target = match &d.target {
                Target::Image { va, .. } => format!("{va:#x}"),
                Target::Placeholder(_) => "placeholder".into(),
            };
            format!(
                "{}!{} slot {:#x} thunk {:#x} desc {:#x} hmod {:#x} -> {target}",
                d.dll,
                d.import.pewasm_label(),
                d.slot,
                d.thunk,
                d.descriptor,
                d.module_handle
            )
        })
        .collect();
    check(diffs, n, "delay imports", format!("{pw_delay:#?}"), format!("{gex_delay:#?}"));

    // Code pointers: relocated words that land in executable sections, plus
    // delay thunks. pewasm seeds function discovery with these.
    let pw_cp: BTreeSet<u64> = meta["code_pointers"].as_array().unwrap().iter().map(hex).collect();
    let mut gex_cp: BTreeSet<u64> = BTreeSet::new();
    for r in &m.pe.relocs {
        let at = r.rva as usize;
        let v = if r.kind == pe::rel::DIR64 {
            u64::from_le_bytes(m.image[at..at + 8].try_into().unwrap())
        } else {
            u32::from_le_bytes(m.image[at..at + 4].try_into().unwrap()) as u64
        };
        if m.pe.sections.iter().any(|s| s.executable() && v >= m.base + s.rva as u64 && v < m.base + (s.rva + s.vsize) as u64) {
            gex_cp.insert(v);
        }
    }
    gex_cp.extend(m.delay_imports.iter().map(|d| d.thunk));
    check(diffs, n, "code pointers", format!("{pw_cp:x?}"), format!("{gex_cp:x?}"));
    let _ = prog;
}

#[test]
fn gex_loads_pe_fixtures_like_pewasm() {
    let Some(pewasm) = find_pewasm() else {
        if std::env::var("GEX_REQUIRE_PEWASM").is_ok_and(|v| v == "1") {
            panic!("GEX_REQUIRE_PEWASM=1 but no pewasm checkout with a .venv was found");
        }
        eprintln!("skipping: no pewasm checkout with a .venv next to this repository (set PEWASM)");
        return;
    };
    let work = std::env::temp_dir().join(format!("gex-pe-pewasm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let cases = cases();
    assert!(cases.len() >= 20);
    let results: Vec<(String, Vec<String>)> = std::thread::scope(|s| {
        let chunks: Vec<&[Case]> = cases.chunks(cases.len().div_ceil(4)).collect();
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                let (pewasm, work) = (&pewasm, &work);
                s.spawn(move || chunk.iter().map(|c| (c.name.clone(), compare(pewasm, work, c))).collect::<Vec<_>>())
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    let _ = std::fs::remove_dir_all(&work);
    let mut failed = 0;
    for (name, diffs) in &results {
        if diffs.is_empty() {
            eprintln!("{name}: agrees with pewasm");
        } else {
            failed += 1;
            for d in diffs {
                eprintln!("{name}: {d}");
            }
        }
    }
    assert_eq!(failed, 0, "{failed} of {} fixtures differ from pewasm", results.len());
}
