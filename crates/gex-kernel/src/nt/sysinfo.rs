//! System information, time, tokens, locale, NLS and atoms
//! (docs/nt-layer.md §4, §10).
//!
//! What gex reports is a fixed machine: the processor [`crate::x86::cpuid`]
//! describes, [`PROCESSORS`] of them, [`PHYS_PAGES`] of memory, Windows 10.0
//! build 19045 (what Wine 11.15 reports), en-US.

use super::object::Obj;
use super::{off, st, to_w, wstr, Abi, Call, Clocks, Process, Status, R};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Processors gex reports. Fixed rather than the host's count: a program
/// sizes its thread pool from it, and in the browser each thread is a Worker.
pub const PROCESSORS: u32 = 4;
/// Physical memory gex reports: 4 GiB.
pub const PHYS_PAGES: u64 = 0x10_0000;
/// The highest user address a program is told about: the top of the NT
/// allocator's range (docs/nt-layer.md §1.2).
pub const HIGHEST_USER_ADDRESS: u64 = 0x7ffe_ffff;
/// en-US.
pub const LCID: u32 = 0x0409;
/// The 100 ns QPC frequency Windows usually reports.
pub const QPC_FREQUENCY: u64 = 10_000_000;

// ---------------------------------------------------------------------------
// KUSER_SHARED_DATA
// ---------------------------------------------------------------------------

/// `ProcessorFeatures[]` for the processor gex's `cpuid` describes (SSE–SSSE3,
/// CMPXCHG16B, no SSE4, no AVX, no XSAVE); indices are winnt.h's `PF_*`.
const FEATURES: &[usize] = &[
    2,  // PF_COMPARE_EXCHANGE_DOUBLE
    3,  // PF_MMX_INSTRUCTIONS_AVAILABLE
    6,  // PF_XMMI_INSTRUCTIONS_AVAILABLE
    8,  // PF_RDTSC_INSTRUCTION_AVAILABLE
    10, // PF_XMMI64_INSTRUCTIONS_AVAILABLE
    12, // PF_NX_ENABLED
    13, // PF_SSE3_INSTRUCTIONS_AVAILABLE
    14, // PF_COMPARE_EXCHANGE128
    23, // PF_FASTFAIL_AVAILABLE
    32, // PF_RDTSCP_INSTRUCTION_AVAILABLE
    36, // PF_SSSE3_INSTRUCTIONS_AVAILABLE
];

/// Fill `KUSER_SHARED_DATA` at [`super::USD`] (Wine's
/// `virtual_init_user_shared_data`, `init_shared_data_cpuinfo` and
/// wineboot's version fields), with `SystemCall = 0` so syscall stubs issue
/// `syscall` (docs/nt-layer.md §2.1).
pub fn init_usd(m: &mut [u8], abi: Abi, clocks: &Clocks) {
    use super::layout::x64::kuser_shared_data as u;
    let base = super::USD as usize;
    let w32 = |m: &mut [u8], o: u64, v: u32| m[base + o as usize..][..4].copy_from_slice(&v.to_le_bytes());
    m[base..base + u::SIZEOF as usize].fill(0);
    w32(m, u::TICKCOUNTMULTIPLIER, 1 << 24);
    w32(m, u::LARGEPAGEMINIMUM, 2 << 20);
    w32(m, u::NTBUILDNUMBER, 19045);
    w32(m, u::NTPRODUCTTYPE, 1); // NtProductWinNt
    m[base + u::PRODUCTTYPEISVALID as usize] = 1;
    let arch: u16 = if abi == Abi::P64 { 9 } else { 0 }; // PROCESSOR_ARCHITECTURE_AMD64 / _INTEL
    m[base + u::NATIVEPROCESSORARCHITECTURE as usize..][..2].copy_from_slice(&arch.to_le_bytes());
    w32(m, u::NTMAJORVERSION, 10);
    w32(m, u::NTMINORVERSION, 0);
    for &f in FEATURES {
        m[base + u::PROCESSORFEATURES as usize + f] = 1;
    }
    w32(m, u::SUITEMASK, 0x100); // VER_SUITE_SINGLEUSERTS, as Wine reports
    m[base + u::NXSUPPORTPOLICY as usize] = 2; // NX_SUPPORT_POLICY_OPTIN
    w32(m, u::NUMBEROFPHYSICALPAGES, PHYS_PAGES as u32);
    m[base + u::QPCFREQUENCY as usize..][..8].copy_from_slice(&QPC_FREQUENCY.to_le_bytes());
    w32(m, u::SYSTEMCALL, 0);
    w32(m, u::COOKIE, 0x6765_7821);
    w32(m, u::ACTIVEPROCESSORCOUNT, PROCESSORS);
    m[base + u::ACTIVEGROUPCOUNT as usize] = 1;
    let root: Vec<u8> = to_w("C:\\windows").iter().flat_map(|c| c.to_le_bytes()).collect();
    m[base + u::NTSYSTEMROOT as usize..][..root.len()].copy_from_slice(&root);
    usd_tick(m, clocks);
}

/// Advance the time fields of `KUSER_SHARED_DATA` with Wine's
/// high1/low/high2 protocol (writers store High2, Low, High1; readers loop
/// until High1 == High2). A host calls this from a ticker so that a guest
/// polling `GetTickCount` without syscalls sees time pass.
pub fn usd_tick(m: &mut [u8], clocks: &Clocks) {
    use super::layout::x64::kuser_shared_data as u;
    let base = super::USD as usize;
    let mono = (clocks.mono_ns)() / 100;
    let ticks = mono / 10_000;
    let put = |m: &mut [u8], o: u64, v: u64| {
        let at = base + o as usize;
        m[at + 8..at + 12].copy_from_slice(&((v >> 32) as u32).to_le_bytes());
        m[at..at + 4].copy_from_slice(&(v as u32).to_le_bytes());
        m[at + 4..at + 8].copy_from_slice(&((v >> 32) as u32).to_le_bytes());
    };
    put(m, u::INTERRUPTTIME, mono);
    put(m, u::SYSTEMTIME, clocks.filetime());
    put(m, u::TICKCOUNT, ticks);
    m[base + u::TICKCOUNTLOWDEPRECATED as usize..][..4].copy_from_slice(&(ticks as u32).to_le_bytes());
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// `NtQueryPerformanceCounter(*counter, *frequency)`: 100 ns units of the
/// monotonic clock. No lock (docs/nt-layer.md §9).
pub fn nt_query_performance_counter(c: &mut Call<'_>) -> R {
    let (cp, fp) = (c.a(0), c.a(1));
    let now = (c.p.clocks.mono_ns)() / 100;
    c.g.w64(cp, now)?;
    if fp != 0 {
        c.g.w64(fp, QPC_FREQUENCY)?;
    }
    Ok(st::SUCCESS)
}

/// `NtQuerySystemTime(*time)`.
pub fn nt_query_system_time(c: &mut Call<'_>) -> R {
    let t = c.p.clocks.filetime();
    c.g.w64(c.a(0), t)?;
    Ok(st::SUCCESS)
}

/// `NtQueryTimerResolution(*min, *max, *current)`: 15.6 ms coarsest, 1 ms
/// finest, as Windows reports.
pub fn nt_query_timer_resolution(c: &mut Call<'_>) -> R {
    let (a, b, cur) = (c.a(0), c.a(1), c.a(2));
    c.g.out32(a, 156_250)?;
    c.g.out32(b, 10_000)?;
    c.g.out32(cur, 10_000)?;
    Ok(st::SUCCESS)
}

/// `NtSetTimerResolution(res, set, *current)`: gex's waits are not tick-based,
/// so every resolution is already in effect.
pub fn nt_set_timer_resolution(c: &mut Call<'_>) -> R {
    c.g.out32(c.a(2), 10_000)?;
    Ok(st::SUCCESS)
}

// ---------------------------------------------------------------------------
// System information
// ---------------------------------------------------------------------------

fn need(len: u64, n: u64) -> R<()> {
    if len < n {
        Err(Status(st::INFO_LENGTH_MISMATCH))
    } else {
        Ok(())
    }
}

fn write_basic_info(c: &mut Call<'_>, buf: u64, len: u64) -> R<u64> {
    let abi = c.p.abi;
    let size = off!(abi, system_basic_information::SIZEOF);
    need(len, size)?;
    macro_rules! o {
        ($f:ident) => {
            buf + off!(abi, system_basic_information::$f)
        };
    }
    c.g.zero(buf, size as usize)?;
    c.g.w32(o!(KEMAXIMUMINCREMENT), 156_250)?;
    c.g.w32(o!(PAGESIZE), 0x1000)?;
    c.g.w32(o!(MMNUMBEROFPHYSICALPAGES), PHYS_PAGES as u32)?;
    c.g.w32(o!(MMLOWESTPHYSICALPAGE), 1)?;
    c.g.w32(o!(MMHIGHESTPHYSICALPAGE), PHYS_PAGES as u32 - 1)?;
    c.g.w32(o!(ALLOCATIONGRANULARITY), 0x1_0000)?;
    c.g.wptr(o!(LOWESTUSERADDRESS), 0x1_0000)?;
    c.g.wptr(o!(HIGHESTUSERADDRESS), HIGHEST_USER_ADDRESS)?;
    c.g.wptr(o!(ACTIVEPROCESSORSAFFINITYMASK), (1u64 << PROCESSORS) - 1)?;
    c.g.w8(o!(NUMBEROFPROCESSORS), PROCESSORS as u8)?;
    Ok(size)
}

/// `NtQuerySystemInformation(class, buffer, len, *ret_len)`.
pub fn nt_query_system_information(c: &mut Call<'_>) -> R {
    let (class, buf, len, ret) = (c.a32(0), c.a(1), c.a32(2) as u64, c.a(3));
    let n = query_system(c, class, buf, len, ret)?;
    c.g.out32(ret, n as u32)?;
    Ok(st::SUCCESS)
}

fn query_system(c: &mut Call<'_>, class: u32, buf: u64, len: u64, ret: u64) -> R<u64> {
    Ok(match class {
        // SystemBasicInformation, SystemNativeBasicInformation,
        // SystemEmulationBasicInformation
        0 | 114 | 62 => write_basic_info(c, buf, len)?,
        // SystemCpuInformation, SystemEmulationProcessorInformation:
        // architecture, level (family), revision (model << 8 | stepping),
        // maximum processors, feature bits.
        1 | 63 => {
            need(len, 12)?;
            let arch: u16 = if c.p.abi == Abi::P64 { 9 } else { 0 };
            c.g.w16(buf, arch)?;
            c.g.w16(buf + 2, 6)?;
            c.g.w16(buf + 4, 0x0f0b)?;
            c.g.w16(buf + 6, PROCESSORS as u16)?;
            // CPU_FEATURE_*: VME TSC CMOV PGE PSE MTRR CX8 MMX X86 PAT FXSR
            // SEP SSE 3DNOW SSE2 SSE3 CX128 NX SSSE3.
            let bits: u32 = 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x80 | 0x100 | 0x400 | 0x800 | 0x1000
                | 0x2000 | 0x4000 | 0x1_0000 | 0x2_0000 | 0x8_0000 | 0x20_0000 | 0x100_0000;
            c.g.w32(buf + 8, bits)?;
            12
        }
        // SystemTimeOfDayInformation: boot time, now, time-zone bias (UTC),
        // time-zone id, reserved, boot time bias, sleep time bias.
        3 => {
            let size = 48u64;
            let n = len.min(size);
            let now = c.p.clocks.filetime();
            let up = (c.p.clocks.mono_ns)() / 100;
            let mut b = vec![0u8; size as usize];
            b[0..8].copy_from_slice(&now.saturating_sub(up).to_le_bytes());
            b[8..16].copy_from_slice(&now.to_le_bytes());
            c.g.write(buf, &b[..n as usize])?;
            n
        }
        // SystemRangeStartInformation: where kernel space begins.
        50 => {
            let ps = c.g.psize();
            need(len, ps)?;
            c.g.wptr(buf, if c.p.abi == Abi::P64 { 0xffff_8000_0000_0000 } else { 0x8000_0000 })?;
            ps
        }
        // SystemDynamicTimeZoneInformation: UTC, gex's only time zone
        // (RTL_DYNAMIC_TIME_ZONE_INFORMATION: bias, standard name[32] and
        // date and bias, daylight name[32] and date and bias, key name[128],
        // dynamic-daylight-disabled).
        102 => {
            let size = 432u64;
            need(len, size)?;
            c.g.zero(buf, size as usize)?;
            let name = to_w("Coordinated Universal Time");
            c.g.write_wchars(buf + 4, &name)?;
            c.g.write_wchars(buf + 88, &name)?;
            c.g.write_wchars(buf + 172, &to_w("UTC"))?;
            size
        }
        // SystemWineVersionInformation: version, build, sysname, release.
        1000 => {
            let s = b"11.15\0wine-11.15 (gex)\0Linux\0gex\0";
            c.g.write(buf, &s[..(len as usize).min(s.len())])?;
            if len < s.len() as u64 {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            s.len() as u64 - 1
        }
        // SystemKernelDebuggerInformation: no debugger.
        35 => {
            need(len, 2)?;
            c.g.w16(buf, 0)?;
            2
        }
        _ => {
            let _ = ret;
            return Err(c.unimplemented(&format!("NtQuerySystemInformation class {class}")));
        }
    })
}

/// `NtQuerySystemInformationEx(class, query, query_len, buffer, len,
/// *ret_len)`.
pub fn nt_query_system_information_ex(c: &mut Call<'_>) -> R {
    let (class, _q, _ql, _buf, _len, _ret) = (c.a32(0), c.a(1), c.a32(2), c.a(3), c.a32(4), c.a(5));
    Err(c.unimplemented(&format!("NtQuerySystemInformationEx class {class}")))
}

// ---------------------------------------------------------------------------
// Locale and NLS
// ---------------------------------------------------------------------------

/// `NtQueryDefaultLocale(user, *lcid)`.
pub fn nt_query_default_locale(c: &mut Call<'_>) -> R {
    c.g.w32(c.a(1), LCID)?;
    Ok(st::SUCCESS)
}

/// `NtQueryDefaultUILanguage(*langid)`.
pub fn nt_query_default_ui_language(c: &mut Call<'_>) -> R {
    c.g.w16(c.a(0), LCID as u16)?;
    Ok(st::SUCCESS)
}

/// `NtQueryInstallUILanguage(*langid)`.
pub fn nt_query_install_ui_language(c: &mut Call<'_>) -> R {
    c.g.w16(c.a(0), LCID as u16)?;
    Ok(st::SUCCESS)
}

/// Map a whole NLS file from the prefix's `nls/` read-only, as a named
/// section so a second request maps the same bytes. Returns (base, size).
fn map_nls(c: &mut Call<'_>, file: &str, section_name: Option<&str>) -> R<(u64, u64)> {
    use super::section::{SecKind, Section};
    let existing = section_name.and_then(|n| c.p.st.lock().unwrap().handles.named.get(&n.to_ascii_lowercase()).cloned());
    let sec = match existing {
        Some(Obj::Section(s)) => s,
        _ => {
            let path = format!("{}/nls/{file}", c.p.cfg.prefix);
            let data = super::file::read_guest_file(&c.kernel(), &path).map_err(|e| {
                eprintln!("gex-nt: cannot read {path}: {e:?}");
                Status(st::OBJECT_NAME_NOT_FOUND)
            })?;
            let size = data.len() as u64;
            let nt_path = to_w(&format!("\\??\\C:\\windows\\system32\\{file}"));
            let s = Arc::new(Section {
                kind: SecKind::File { data: Arc::new(data), nt_path },
                size,
                prot: super::vm::page_prot::READONLY,
                attrs: super::section::SEC_COMMIT,
                views: Default::default(),
            });
            if let Some(n) = section_name {
                c.p.st.lock().unwrap().handles.named.insert(n.to_ascii_lowercase(), Obj::Section(Arc::clone(&s)));
            }
            s
        }
    };
    let SecKind::File { data, .. } = &sec.kind else { return Err(Status(st::OBJECT_TYPE_MISMATCH)) };
    let data = Arc::clone(data);
    let base = map_readonly_copy(c, &data, "[nls]")?;
    Ok((base, data.len() as u64))
}

/// Map `data` as a fresh read-only view from the NT allocator.
pub fn map_readonly_copy(c: &mut Call<'_>, data: &[u8], name: &str) -> R<u64> {
    use super::vm::{self, page_prot, Kind};
    let size = (data.len() as u64 + vm::PAGE - 1) & !(vm::PAGE - 1);
    let base = {
        let mut stl = c.p.st.lock().unwrap();
        let b = stl.vm.find_free((size + vm::GRANULE - 1) & !(vm::GRANULE - 1), false, vm::ALLOC_HIGH)
            .ok_or(Status(st::NO_MEMORY))?;
        stl.vm.reserve(b, size, page_prot::READONLY, true, Kind::Mapped, name);
        b
    };
    c.g.zero(base, size as usize)?;
    c.g.write(base, data)?;
    vm::after_change(c, base, base + size, &[])?;
    Ok(base)
}

/// `NtInitializeNlsFiles(*ptr, *lcid, *size)`: `locale.nls`.
pub fn nt_initialize_nls_files(c: &mut Call<'_>) -> R {
    let (pp, lp, sp) = (c.a(0), c.a(1), c.a(2));
    let (base, size) = map_nls(c, "locale.nls", None)?;
    c.g.wptr(pp, base)?;
    c.g.out32(lp, LCID)?;
    if sp != 0 {
        c.g.w64(sp, size)?;
    }
    Ok(st::SUCCESS)
}

/// `NtGetNlsSectionPtr(type, id, unknown, *ptr, *size)` (Wine's
/// `get_nls_section_name` and `get_nls_file_path`).
pub fn nt_get_nls_section_ptr(c: &mut Call<'_>) -> R {
    let (ty, id, pp, sp) = (c.a32(0), c.a32(1), c.a(3), c.a(4));
    let (section, file) = match ty {
        // NLS_SECTION_SORTKEYS
        9 => {
            if id != 0 {
                return Err(Status(st::INVALID_PARAMETER_1));
            }
            ("\\NLS\\NlsSectionSORTDEFAULT".to_string(), "sortdefault.nls".to_string())
        }
        // NLS_SECTION_CASEMAP
        10 => {
            if id != 0 {
                return Err(Status(st::UNSUCCESSFUL));
            }
            ("\\NLS\\NlsSectionLANG_INTL".to_string(), "l_intl.nls".to_string())
        }
        // NLS_SECTION_CODEPAGE
        11 => (format!("\\NLS\\NlsSectionCP{id:03}"), format!("c_{id:03}.nls")),
        // NLS_SECTION_NORMALIZE
        12 => {
            let name = match id {
                1 => "normnfc",
                2 => "normnfd",
                5 => "normnfkc",
                6 => "normnfkd",
                13 => "normidna",
                _ => return Err(Status(st::OBJECT_NAME_NOT_FOUND)),
            };
            (format!("\\NLS\\NlsSectionNORM{id:08x}"), format!("{name}.nls"))
        }
        _ => return Err(Status(st::INVALID_PARAMETER_1)),
    };
    let (base, size) = map_nls(c, &file, Some(&section))?;
    c.g.wptr(pp, base)?;
    c.g.out_ptr(sp, size)?;
    Ok(st::SUCCESS)
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// `NtOpenProcessToken(process, access, *handle)` and the `Ex` form
/// (`process, access, attributes, *handle`).
pub fn nt_open_process_token(c: &mut Call<'_>) -> R {
    let ex = c.nr == super::sysno::ntdll::NtOpenProcessTokenEx;
    let hp = if ex { c.a(3) } else { c.a(2) };
    let access = c.a32(1);
    let h = c.p.st.lock().unwrap().handles.insert(Obj::Token, access);
    c.g.wptr(hp, h as u64)?;
    Ok(st::SUCCESS)
}

/// `NtOpenThreadToken(thread, access, self, *handle)` and the `Ex` form: no
/// thread impersonates, which is `STATUS_NO_TOKEN`.
pub fn nt_open_thread_token(_c: &mut Call<'_>) -> R {
    Err(Status(st::NO_TOKEN))
}

/// A SID's binary form.
fn sid(auth: u8, subs: &[u32]) -> Vec<u8> {
    let mut v = vec![1, subs.len() as u8, 0, 0, 0, 0, 0, auth];
    for s in subs {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// The user: `S-1-5-21-0-0-0-1000` (docs: [`super::reg::USER_SID`]).
fn user_sid() -> Vec<u8> {
    sid(5, &[21, 0, 0, 0, 1000])
}

/// `NtQueryInformationToken(token, class, buffer, len, *ret_len)`. The
/// token is an unelevated member of Users and Administrators at medium
/// integrity, as Wine's default token is.
pub fn nt_query_information_token(c: &mut Call<'_>) -> R {
    let (h, class, buf, len, ret) = (c.a32(0), c.a32(1), c.a(2), c.a32(3) as u64, c.a(4));
    if !matches!(super::object::resolve(c, h)?, Obj::Token) {
        return Err(Status(st::OBJECT_TYPE_MISMATCH));
    }
    let ps = c.g.psize();
    // Write `header` (with pointer slots) then the SIDs it points at.
    let sid_list = |c: &mut Call<'_>, fixed: u64, sids: &[(u64, Vec<u8>, Option<(u64, u32)>)]| -> R<u64> {
        let need = fixed + sids.iter().map(|s| s.1.len() as u64).sum::<u64>();
        c.g.out32(ret, need as u32)?;
        if len < need {
            return Err(Status(st::BUFFER_TOO_SMALL));
        }
        c.g.zero(buf, fixed as usize)?;
        let mut at = buf + fixed;
        for (ptr_off, bytes, attr) in sids {
            c.g.wptr(buf + ptr_off, at)?;
            if let Some((o, v)) = attr {
                c.g.w32(buf + o, *v)?;
            }
            c.g.write(at, bytes)?;
            at += bytes.len() as u64;
        }
        Ok(need)
    };
    match class {
        // TokenUser: SID_AND_ATTRIBUTES.
        1 => {
            sid_list(c, 2 * ps, &[(0, user_sid(), None)])?;
        }
        // TokenGroups: count, then SID_AND_ATTRIBUTES[].
        2 => {
            let groups = [
                (sid(1, &[0]), 7u32),            // Everyone
                (sid(5, &[32, 545]), 7),         // BUILTIN\Users
                (sid(5, &[32, 544]), 0x10),      // BUILTIN\Administrators, deny-only
                (sid(5, &[4]), 7),               // INTERACTIVE
                (sid(5, &[11]), 7),              // Authenticated Users
                (sid(16, &[0x2000]), 0x60),      // Mandatory Label\Medium
            ];
            let fixed = ps + groups.len() as u64 * 2 * ps;
            let list: Vec<(u64, Vec<u8>, Option<(u64, u32)>)> = groups
                .iter()
                .enumerate()
                .map(|(i, (s, a))| {
                    let e = ps + i as u64 * 2 * ps;
                    (e, s.clone(), Some((e + ps, *a)))
                })
                .collect();
            sid_list(c, fixed, &list)?;
            c.g.w32(buf, groups.len() as u32)?;
        }
        // TokenOwner, TokenPrimaryGroup: a SID pointer.
        4 | 5 => {
            sid_list(c, ps, &[(0, user_sid(), None)])?;
        }
        // TokenType: TokenPrimary.
        8 => {
            need(len, 4)?;
            c.g.w32(buf, 1)?;
            c.g.out32(ret, 4)?;
        }
        // TokenSessionId
        12 => {
            need(len, 4)?;
            c.g.w32(buf, 1)?;
            c.g.out32(ret, 4)?;
        }
        // TokenElevationType: TokenElevationTypeDefault; TokenElevation: no.
        18 | 20 => {
            need(len, 4)?;
            c.g.w32(buf, if class == 18 { 1 } else { 0 })?;
            c.g.out32(ret, 4)?;
        }
        // TokenIntegrityLevel: TOKEN_MANDATORY_LABEL (SID_AND_ATTRIBUTES).
        25 => {
            sid_list(c, 2 * ps, &[(0, sid(16, &[0x2000]), Some((ps, 0x60)))])?;
        }
        // TokenIsAppContainer: no.
        29 => {
            need(len, 4)?;
            c.g.w32(buf, 0)?;
            c.g.out32(ret, 4)?;
        }
        _ => return Err(c.unimplemented(&format!("NtQueryInformationToken class {class}"))),
    }
    Ok(st::SUCCESS)
}

// ---------------------------------------------------------------------------
// Atoms and LUIDs
// ---------------------------------------------------------------------------

/// The global atom table: string atoms from 0xc000.
#[derive(Default, Debug)]
pub struct Atoms {
    names: Vec<(Vec<u16>, u32)>,
}

const MAXINTATOM: u32 = 0xc000;

fn int_atom(name: &[u16]) -> Option<u32> {
    let s = wstr(name);
    let n = s.strip_prefix('#')?.parse::<u32>().ok()?;
    (n > 0 && n < MAXINTATOM).then_some(n)
}

fn lower(s: &[u16]) -> Vec<u16> {
    to_w(&wstr(s).to_lowercase())
}

fn atom_name(c: &Call<'_>, p: u64, len: u64) -> R<Vec<u16>> {
    if len > 255 * 2 {
        return Err(Status(st::INVALID_PARAMETER));
    }
    c.g.wchars(p, (len / 2) as usize)
}

/// `NtAddAtom(name, len, *atom)`.
pub fn nt_add_atom(c: &mut Call<'_>) -> R {
    let (np, len, ap) = (c.a(0), c.a32(1) as u64, c.a(2));
    let name = atom_name(c, np, len)?;
    let atom = match int_atom(&name) {
        Some(a) => a,
        None => {
            let key = lower(&name);
            let mut t = c.p.atoms.lock().unwrap();
            match t.names.iter_mut().position(|(n, u)| *u > 0 && lower(n) == key) {
                Some(i) => {
                    t.names[i].1 += 1;
                    MAXINTATOM + i as u32
                }
                None => {
                    t.names.push((name, 1));
                    MAXINTATOM + t.names.len() as u32 - 1
                }
            }
        }
    };
    if ap != 0 {
        c.g.w16(ap, atom as u16)?;
    }
    Ok(st::SUCCESS)
}

/// `NtDeleteAtom(atom)`: drop a reference; the last one frees the name.
pub fn nt_delete_atom(c: &mut Call<'_>) -> R {
    let atom = c.a32(0) & 0xffff;
    if atom < MAXINTATOM {
        return Ok(st::SUCCESS);
    }
    let mut t = c.p.atoms.lock().unwrap();
    match t.names.get_mut((atom - MAXINTATOM) as usize) {
        Some(e) if e.1 > 0 => {
            e.1 -= 1;
            if e.1 == 0 {
                // The slot stays (atoms are indices); an empty name matches
                // nothing.
                e.0.clear();
            }
            Ok(st::SUCCESS)
        }
        _ => Err(Status(st::INVALID_HANDLE)),
    }
}

/// `NtFindAtom(name, len, *atom)`.
pub fn nt_find_atom(c: &mut Call<'_>) -> R {
    let (np, len, ap) = (c.a(0), c.a32(1) as u64, c.a(2));
    let name = atom_name(c, np, len)?;
    let atom = match int_atom(&name) {
        Some(a) => a,
        None => {
            let key = lower(&name);
            let t = c.p.atoms.lock().unwrap();
            let i = t.names.iter().position(|(n, u)| *u > 0 && lower(n) == key).ok_or(Status(st::OBJECT_NAME_NOT_FOUND))?;
            MAXINTATOM + i as u32
        }
    };
    if ap != 0 {
        c.g.w16(ap, atom as u16)?;
    }
    Ok(st::SUCCESS)
}

/// `NtQueryInformationAtom(atom, class, buffer, len, *ret_len)`:
/// `AtomBasicInformation` (usage, flags, name length, name).
pub fn nt_query_information_atom(c: &mut Call<'_>) -> R {
    let (atom, class, buf, len, ret) = (c.a32(0) & 0xffff, c.a32(1), c.a(2), c.a32(3) as u64, c.a(4));
    if class != 0 {
        return Err(c.unimplemented(&format!("NtQueryInformationAtom class {class}")));
    }
    let (name, usage, flags) = if atom < MAXINTATOM {
        (to_w(&format!("#{atom}")), 1u16, 1u16) // ATOM_FLAG_PINNED for integer atoms
    } else {
        let t = c.p.atoms.lock().unwrap();
        let (n, u) = t.names.get((atom - MAXINTATOM) as usize).cloned().ok_or(Status(st::INVALID_HANDLE))?;
        (n, u as u16, 0)
    };
    need(len, 6)?;
    let room = ((len - 6) / 2) as usize;
    let n = name.len().min(room.saturating_sub(1));
    c.g.w16(buf, usage)?;
    c.g.w16(buf + 2, flags)?;
    c.g.w16(buf + 4, (n * 2) as u16)?;
    c.g.write_wchars(buf + 6, &name[..n])?;
    if room > n {
        c.g.w16(buf + 6 + 2 * n as u64, 0)?;
    }
    c.g.out32(ret, 6 + 2 * n as u32)?;
    if n < name.len() {
        return Err(Status(st::BUFFER_TOO_SMALL));
    }
    Ok(st::SUCCESS)
}

static NEXT_LUID: AtomicU64 = AtomicU64::new(0x1000);

/// `NtAllocateLocallyUniqueId(*luid)`.
pub fn nt_allocate_locally_unique_id(c: &mut Call<'_>) -> R {
    let v = NEXT_LUID.fetch_add(1, Ordering::Relaxed);
    c.g.w64(c.a(0), v)?;
    Ok(st::SUCCESS)
}

/// `NtQueryLicenseValue(name, *type, buffer, len, *ret_len)`: no licensing
/// values exist, as in Wine.
pub fn nt_query_license_value(c: &mut Call<'_>) -> R {
    let name = c.g.ustr(c.a(0))?.unwrap_or_default();
    let _ = name;
    Err(Status(st::OBJECT_NAME_NOT_FOUND))
}

#[allow(dead_code)]
fn _unused(_: &Process) {}

/// `NtWow64IsProcessorFeaturePresent(feature)`: a BOOLEAN, from
/// `KUSER_SHARED_DATA.ProcessorFeatures` (i386 ntdll asks this way).
pub fn nt_wow64_is_processor_feature_present(c: &mut Call<'_>) -> R {
    use super::layout::x64::kuser_shared_data as u;
    let f = c.a32(0) as u64;
    if f >= 64 {
        return Ok(0);
    }
    Ok(c.g.u8(super::USD + u::PROCESSORFEATURES + f)? as u32)
}
