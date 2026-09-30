//! PE loading against the fixtures in `tests/pe/bin` (built by
//! `tests/pe/build.sh`; see `tests/pe/src` for what each one exercises).

use gex_ld::pe::{
    self, ImportName, PeErrorKind, PeFiles, PeLayout, PeOptions, PeProgram, PlaceholderReason, Reason,
    Target,
};
use std::collections::HashMap;
use std::path::PathBuf;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/pe/bin")
}

fn fixture(rel: &str) -> String {
    bin().join(rel).to_string_lossy().into_owned()
}

fn load(rel: &str) -> PeProgram {
    pe::load_closure(&fixture(rel), &pe::HostFiles, &PeOptions::default())
        .unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Files held in memory, so a test can patch one.
struct MemFiles(HashMap<String, Vec<u8>>);

impl PeFiles for MemFiles {
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        self.0.get(path).cloned()
    }
    fn list(&self, dir: &str) -> Vec<String> {
        self.0
            .keys()
            .filter_map(|p| p.strip_prefix(dir)?.strip_prefix('/').map(str::to_string))
            .collect()
    }
}

fn arch_files(arch: &str) -> MemFiles {
    let mut m = HashMap::new();
    for e in std::fs::read_dir(bin().join(arch)).unwrap() {
        let e = e.unwrap();
        m.insert(format!("/{arch}/{}", e.file_name().to_string_lossy()), std::fs::read(e.path()).unwrap());
    }
    MemFiles(m)
}

fn read_ptr(m: &pe::PeModule, va: u64) -> u64 {
    let at = (va - m.base) as usize;
    if m.pe.is64 {
        u64::from_le_bytes(m.image[at..at + 8].try_into().unwrap())
    } else {
        u32::from_le_bytes(m.image[at..at + 4].try_into().unwrap()) as u64
    }
}

fn find<'a>(p: &'a PeProgram, name: &str) -> &'a pe::PeModule {
    p.modules.iter().find(|m| m.name == name).unwrap_or_else(|| panic!("no module {name}"))
}

fn import<'a>(m: &'a pe::PeModule, label: &str) -> &'a pe::BoundSlot {
    m.imports
        .iter()
        .find(|b| b.import.label() == label)
        .unwrap_or_else(|| panic!("{} imports no {label}", m.name))
}

/// Every executable fixture loads under the default layout, lands inside
/// gex's code range, and binds every slot to an image or a system placeholder.
#[test]
fn every_fixture_loads() {
    let mut n = 0;
    for arch in ["x86", "x64"] {
        for e in std::fs::read_dir(bin().join(arch)).unwrap() {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            if !name.ends_with(".exe") || name == "highbase-fixed.exe" {
                continue;
            }
            let p = load(&format!("{arch}/{name}"));
            n += 1;
            assert!(p.missing.is_empty(), "{name}: missing {:?}", p.missing);
            for m in &p.modules {
                assert!(m.base >= gex_abi::EXEC_BASE && m.end() <= gex_abi::MMAP_TOP, "{name}: {}", m.name);
                for b in &m.imports {
                    let v = read_ptr(m, b.slot);
                    match &b.target {
                        Target::Image { va, .. } => assert_eq!(v, *va),
                        Target::Placeholder(i) => {
                            let ph = &p.placeholders.entries[*i as usize];
                            assert_eq!(ph.reason, PlaceholderReason::System, "{name}: {}", ph.label());
                            assert_eq!(v, ph.addr);
                            assert_eq!(p.placeholders.lookup(v).unwrap().index, *i);
                        }
                    }
                }
                // Mappings cover the image, in order, with section permissions.
                let segs = &m.segments;
                assert!(segs.windows(2).all(|w| w[0].vaddr + w[0].mem_size <= w[1].vaddr));
                for s in &m.pe.sections {
                    let seg = segs.iter().find(|g| g.vaddr == m.base + s.rva as u64).unwrap();
                    assert_eq!(
                        (seg.readable, seg.writable, seg.executable),
                        (s.readable(), s.writable(), s.executable()),
                        "{name}: {}",
                        s.name
                    );
                }
            }
            let occupied = p.occupied();
            assert!(occupied.windows(2).all(|w| w[0].1 <= w[1].0), "{name}: images overlap");
        }
    }
    assert!(n >= 20, "only {n} fixtures");
}

/// The closure fixture: imports by name and ordinal, a data import, forwarders
/// by name, by ordinal and into a system DLL, delay imports, TLS callbacks.
#[test]
fn the_closure_resolves_imports_forwarders_and_delay_imports() {
    for arch in ["x86", "x64"] {
        let p = load(&format!("{arch}/uses.exe"));
        let names: Vec<&str> = p.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["uses.exe", "liba.dll", "delayed.dll", "libb.dll"], "{arch}");
        let (exe, a, b, d) = (&p.modules[0], find(&p, "liba.dll"), find(&p, "libb.dll"), find(&p, "delayed.dll"));
        assert_eq!(a.reason, Reason::Import { by: "uses.exe".into() });
        assert_eq!(d.reason, Reason::Delay { by: "uses.exe".into() });
        assert_eq!(b.reason, Reason::Import { by: "liba.dll".into() });

        let exp = |m: &pe::PeModule, label: &str| {
            m.export_addresses().into_iter().find(|(l, _)| l == label).unwrap().1
        };
        let to = |s: &pe::BoundSlot| match &s.target {
            Target::Image { va, .. } => *va,
            t => panic!("{arch}: {} resolved to {t:?}", s.import.label()),
        };
        assert_eq!(to(import(exe, "a_add")), exp(a, "a_add"));
        assert_eq!(to(import(exe, "a_counter")), exp(a, "a_counter"));
        // Imported by ordinal: a_ord is NONAME.
        assert_eq!(import(exe, "#7").import, ImportName::Ordinal(7));
        assert_eq!(to(import(exe, "#7")), exp(a, "#7"));
        // Forwarded by name and by ordinal into libb.
        assert_eq!(to(import(exe, "a_fwd_b")), exp(b, "b_value"));
        assert_eq!(to(import(exe, "a_fwd_ord")), exp(b, "#3"));
        match &import(exe, "a_fwd_b").target {
            Target::Image { via, .. } => assert_eq!(via, &["liba.dll!a_fwd_b"]),
            _ => unreachable!(),
        }
        // Forwarded into a system DLL: a placeholder named for the target.
        let Target::Placeholder(i) = import(exe, "a_heap").target else { panic!() };
        let ph = &p.placeholders.entries[i as usize];
        assert_eq!(ph.label(), "kernel32.dll!HeapAlloc");
        assert_eq!(ph.via, ["liba.dll!a_heap"]);
        // liba's own import from libb.
        assert_eq!(to(import(a, "b_second")), exp(b, "b_second"));

        // Delay imports: the slots keep their thunks, inside the exe's text,
        // and report where they would bind.
        assert_eq!(exe.delay_imports.len(), 2);
        for s in &exe.delay_imports {
            assert_eq!(read_ptr(exe, s.slot), s.thunk);
            let text = exe.pe.sections.iter().find(|s| s.name == ".text").unwrap();
            assert!(text.contains((s.thunk - exe.base) as u32));
            let Target::Image { module, va, .. } = s.target else { panic!() };
            assert_eq!(p.modules[module].name, "delayed.dll");
            assert_eq!(va, exp(d, &s.import.label()));
        }

        // TLS: the CRT's callback plus ours, all in the exe's text.
        assert!(exe.tls_callbacks.len() >= 2, "{arch}: {:?}", exe.tls_callbacks);
        for &cb in &exe.tls_callbacks {
            assert!(exe.pe.sections.iter().any(|s| s.executable() && s.contains((cb - exe.base) as u32)));
        }

        // Resources: a string table, a named RCDATA and a version block.
        let res: Vec<String> = exe
            .pe
            .resources
            .iter()
            .map(|r| r.path.iter().map(|x| x.to_string()).collect::<Vec<_>>().join("/"))
            .collect();
        assert_eq!(res, ["6/1/1033", "10/\"GREETING\"/1033", "16/1/1033"], "{arch}");

        // Exports: ordinal-only and forwarders.
        let e = &a.pe.exports;
        assert_eq!(e.ordinal_base, 5);
        assert!(e.entries.iter().any(|x| x.ordinal == 7 && x.names.is_empty() && x.rva != 0));
        let fwd: Vec<_> = e.entries.iter().filter_map(|x| x.forwarder.clone()).collect();
        assert_eq!(fwd, ["libb.b_value", "libb.#3", "kernel32.HeapAlloc"]);
    }
}

/// PE32+ has an exception directory; entries lie in executable sections.
#[test]
fn pdata_is_exposed_for_pe32_plus() {
    let p = load("x64/uses.exe");
    let exe = &p.modules[0];
    assert!(exe.pe.pdata.len() > 10);
    for f in &exe.pe.pdata {
        assert!(f.begin_rva < f.end_rva);
        assert!(exe.pe.sections.iter().any(|s| s.executable() && s.contains(f.begin_rva)));
        assert!(f.unwind_flags.is_some());
    }
    assert!(load("x86/uses.exe").modules[0].pe.pdata.is_empty());
}

/// A high PE32+ base is moved below 4 GiB, and every DIR64 target with it.
#[test]
fn a_high_base_pe32_plus_is_rebased_below_4g() {
    let p = load("x64/highbase.exe");
    let m = &p.modules[0];
    assert_eq!(m.pe.preferred_base, 0x1_4000_0000);
    assert!(m.rebased && m.base == gex_abi::EXEC_BASE);
    assert!(m.pe.reloc_counts.dir64 > 0);
    for r in &m.pe.relocs {
        let v = read_ptr(m, m.base + r.rva as u64);
        assert!(v >= m.base && v < m.end(), "{v:#x}");
    }
    // The header says where it is.
    let o = m.pe.opt_offset;
    assert_eq!(u64::from_le_bytes(m.image[o + 24..o + 32].try_into().unwrap()), m.base);
}

#[test]
fn a_fixed_high_base_image_fails_loudly() {
    let e = pe::load_closure(&fixture("x64/highbase-fixed.exe"), &pe::HostFiles, &PeOptions::default())
        .unwrap_err();
    assert_eq!(e.kind, PeErrorKind::Layout);
    let msg = e.to_string();
    assert!(msg.contains("0x140000000") && msg.contains("RELOCS_STRIPPED"), "{msg}");
}

/// Mapping at two bases differs exactly at the relocations (and ImageBase).
#[test]
fn relocation_moves_exactly_the_relocated_words() {
    for rel in ["x86/uses.exe", "x64/uses.exe", "x86/msvc.exe", "x64/draw.exe"] {
        let bytes = std::fs::read(fixture(rel)).unwrap();
        let img = pe::parse(&bytes).unwrap();
        let (b1, b2) = (0x0100_0000u64, 0x0230_0000u64);
        let (m1, m2) = (img.map(b1).unwrap(), img.map(b2).unwrap());
        let mut touched = vec![false; m1.len()];
        let w = if img.is64 { 8 } else { 4 };
        for r in &img.relocs {
            let at = r.rva as usize;
            let rw = if r.kind == pe::rel::DIR64 { 8 } else { 4 };
            let (x, y) = if rw == 8 {
                (
                    u64::from_le_bytes(m1[at..at + 8].try_into().unwrap()),
                    u64::from_le_bytes(m2[at..at + 8].try_into().unwrap()),
                )
            } else {
                (
                    u32::from_le_bytes(m1[at..at + 4].try_into().unwrap()) as u64,
                    u32::from_le_bytes(m2[at..at + 4].try_into().unwrap()) as u64,
                )
            };
            assert_eq!(y.wrapping_sub(x) & if rw == 8 { u64::MAX } else { 0xffff_ffff }, b2 - b1, "{rel}");
            touched[at..at + rw].iter_mut().for_each(|t| *t = true);
        }
        let hb = img.opt_offset + if img.is64 { 24 } else { 28 };
        touched[hb..hb + w].iter_mut().for_each(|t| *t = true);
        for i in 0..m1.len() {
            if !touched[i] {
                assert_eq!(m1[i], m2[i], "{rel}: byte {i:#x} moved without a relocation");
            }
        }
    }
}

/// A missing DLL and a missing export become placeholders with their reason.
#[test]
fn missing_dlls_and_exports_are_placeholders() {
    let mut files = arch_files("x86");
    files.0.remove("/x86/delayed.dll");
    // Rename liba's `a_add` export so the import no longer finds it.
    let liba = files.0.get_mut("/x86/liba.dll").unwrap();
    let at = liba.windows(6).position(|w| w == b"a_add\0").unwrap();
    liba[at + 4] = b'X';
    let p = pe::load_closure("/x86/uses.exe", &files, &PeOptions::default()).unwrap();
    assert_eq!(p.missing, [("delayed.dll".to_string(), "uses.exe".to_string())]);
    let exe = &p.modules[0];
    let Target::Placeholder(i) = import(exe, "a_add").target else { panic!() };
    assert_eq!(p.placeholders.entries[i as usize].reason, PlaceholderReason::MissingExport);
    let d = &exe.delay_imports[0];
    let Target::Placeholder(i) = d.target else { panic!() };
    let ph = &p.placeholders.entries[i as usize];
    assert_eq!(ph.reason, PlaceholderReason::MissingDll);
    assert_eq!(ph.label(), "delayed.dll!d_mul");
    assert!(ph.slots.iter().all(|s| s.delay));
    // The delay slot still holds its thunk, not the placeholder.
    assert_eq!(read_ptr(exe, d.slot), d.thunk);
}

/// DLL names match case-insensitively, as on Windows.
#[test]
fn dlls_are_found_case_insensitively() {
    let mut files = arch_files("x86");
    let b = files.0.remove("/x86/libb.dll").unwrap();
    files.0.insert("/x86/LIBB.DLL".into(), b);
    let p = pe::load_closure("/x86/uses.exe", &files, &PeOptions::default()).unwrap();
    assert_eq!(find(&p, "libb.dll").path, "/x86/LIBB.DLL");
}

/// A DLL whose preferred base is taken is rebased, and imports follow it.
#[test]
fn a_taken_preferred_base_rebases() {
    let files = arch_files("x86");
    let libb = pe::parse(&files.0["/x86/libb.dll"]).unwrap();
    let mut layout = PeLayout::default();
    layout.pins.insert("liba.dll".into(), libb.preferred_base);
    let p = pe::load_closure("/x86/uses.exe", &files, &PeOptions { layout, ..Default::default() }).unwrap();
    let (a, b) = (find(&p, "liba.dll"), find(&p, "libb.dll"));
    assert_eq!(a.base, libb.preferred_base);
    assert!(b.rebased && b.base != libb.preferred_base);
    let Target::Image { va, .. } = import(a, "b_second").target else { panic!() };
    assert!(va >= b.base && va < b.end());
}

/// `pack` rebases even images whose preferred base would do.
#[test]
fn pack_keeps_everything_low() {
    let layout = PeLayout { pack: true, ..Default::default() };
    let p = pe::load_closure(&fixture("x86/uses.exe"), &pe::HostFiles, &PeOptions { layout, ..Default::default() })
        .unwrap();
    assert!(p.modules.iter().all(|m| m.end() <= 0x0100_0000), "{:?}", p.occupied());
}

fn import_descriptor(pe: &pe::PeImage, dll: &str) -> usize {
    let (rva, _) = pe.dir(pe::dir::IMPORT);
    let i = pe.imports.iter().position(|d| d.dll == dll).unwrap();
    pe.rva_to_offset(rva + i as u32 * 20).unwrap()
}

/// A pre-bound IAT is rebound from the name table; one with no name table
/// is refused by name. The bound-import directory is parsed.
#[test]
fn bound_imports_are_rebound() {
    let mut files = arch_files("x86");
    let exe = files.0.get_mut("/x86/uses.exe").unwrap();
    let img = pe::parse(exe).unwrap();
    let k32 = img.imports.iter().find(|d| d.dll == "KERNEL32.dll").unwrap().clone();
    let desc = import_descriptor(&img, "KERNEL32.dll");
    // New-style binding: timestamp -1, stale addresses in the IAT, and a
    // bound-import directory in the header slack.
    exe[desc + 4..desc + 8].copy_from_slice(&u32::MAX.to_le_bytes());
    for s in &k32.symbols {
        let o = img.rva_to_offset(s.iat_rva).unwrap();
        exe[o..o + 4].copy_from_slice(&0x7654_3210u32.to_le_bytes());
    }
    let slack = 0x380usize;
    assert!(exe[slack..slack + 0x40].iter().all(|&b| b == 0));
    let mut dirent = Vec::new();
    dirent.extend_from_slice(&0x1234_5678u32.to_le_bytes());
    dirent.extend_from_slice(&16u16.to_le_bytes());
    dirent.extend_from_slice(&0u16.to_le_bytes());
    dirent.extend_from_slice(&[0; 8]);
    dirent.extend_from_slice(b"KERNEL32.dll\0");
    exe[slack..slack + dirent.len()].copy_from_slice(&dirent);
    let dd = img.opt_offset + 96 + pe::dir::BOUND_IMPORT * 8;
    exe[dd..dd + 4].copy_from_slice(&(slack as u32).to_le_bytes());
    exe[dd + 4..dd + 8].copy_from_slice(&(dirent.len() as u32).to_le_bytes());

    let p = pe::load_closure("/x86/uses.exe", &files, &PeOptions::default()).unwrap();
    let m = &p.modules[0];
    assert_eq!(m.pe.bound_imports.len(), 1);
    assert_eq!(m.pe.bound_imports[0].dll, "KERNEL32.dll");
    assert_eq!(m.pe.bound_imports[0].timestamp, 0x1234_5678);
    let s = import(m, "GetLastError");
    let Target::Placeholder(i) = s.target else { panic!() };
    assert_eq!(read_ptr(m, s.slot), p.placeholders.entries[i as usize].addr);

    // Bound, with no name table to rebind from.
    let exe = files.0.get_mut("/x86/uses.exe").unwrap();
    exe[desc..desc + 4].copy_from_slice(&0u32.to_le_bytes());
    let e = pe::parse(exe).unwrap_err();
    assert_eq!(e.kind, PeErrorKind::Unsupported);
    assert!(e.to_string().contains("bound imports from KERNEL32.dll"), "{e}");
}

/// A relocation type gex does not apply is refused by name.
#[test]
fn unknown_relocation_types_fail_loudly() {
    let mut bytes = std::fs::read(fixture("x86/uses.exe")).unwrap();
    let img = pe::parse(&bytes).unwrap();
    let (rva, _) = img.dir(pe::dir::BASERELOC);
    let o = img.rva_to_offset(rva).unwrap() + 8;
    let w = u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    bytes[o..o + 2].copy_from_slice(&((w & 0xfff) | (1 << 12)).to_le_bytes());
    let e = pe::parse(&bytes).unwrap_err();
    assert_eq!(e.kind, PeErrorKind::Unsupported);
    assert!(e.to_string().contains("base relocation type 1 (HIGH)"), "{e}");
}

/// A managed image is refused by name.
#[test]
fn clr_images_fail_loudly() {
    let mut bytes = std::fs::read(fixture("x86/hello32.exe")).unwrap();
    let img = pe::parse(&bytes).unwrap();
    let dd = img.opt_offset + 96 + pe::dir::CLR * 8;
    bytes[dd..dd + 4].copy_from_slice(&0x1000u32.to_le_bytes());
    bytes[dd + 4..dd + 8].copy_from_slice(&0x48u32.to_le_bytes());
    let e = pe::parse(&bytes).unwrap_err();
    assert!(e.to_string().contains(".NET/CLR"), "{e}");
}

/// With no `.reloc`, an image moves only if it was linked `DYNAMIC_BASE`
/// (linkers omit an empty `.reloc`); otherwise a needed move fails loudly.
#[test]
fn no_reloc_directory_moves_only_when_dynamic_base() {
    let mut bytes = std::fs::read(fixture("x64/msvc.exe")).unwrap();
    let img = pe::parse(&bytes).unwrap();
    assert_eq!(img.preferred_base, 0x1_4000_0000);
    let dd = img.opt_offset + 112 + pe::dir::BASERELOC * 8;
    bytes[dd..dd + 8].fill(0);
    let img = pe::parse(&bytes).unwrap();
    assert!(img.relocs.is_empty() && img.dll_characteristics & pe::DLLCHAR_DYNAMIC_BASE != 0);
    assert!(img.relocatable());
    img.map(gex_abi::EXEC_BASE).unwrap();
    let dc = img.opt_offset + 70;
    let v = u16::from_le_bytes([bytes[dc], bytes[dc + 1]]) & !pe::DLLCHAR_DYNAMIC_BASE;
    bytes[dc..dc + 2].copy_from_slice(&v.to_le_bytes());
    let img = pe::parse(&bytes).unwrap();
    assert!(!img.relocatable());
    let e = img.map(gex_abi::EXEC_BASE).unwrap_err();
    assert_eq!(e.kind, PeErrorKind::Layout);
    assert!(e.to_string().contains("not linked /DYNAMICBASE"), "{e}");
}
