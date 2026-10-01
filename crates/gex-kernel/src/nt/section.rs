//! Sections and views: images (`SEC_IMAGE`), file mappings and anonymous
//! shared memory (docs/nt-layer.md §3.4).
//!
//! An image is parsed once with gex-ld and mapped as a copy at the address
//! the NT allocator chooses (docs/nt-layer.md §1.2: relocatable images are
//! packed low). The kernel applies the base relocations and updates
//! `OptionalHeader.ImageBase`, as Wine's unix side does for a dynamic base,
//! so the PE loader finds nothing left to relocate.

use super::object::Obj;
use super::vm::{self, page_prot, Kind, PAGE};
use super::{off, st, to_w, wstr, Abi, Call, Process, Status, G, R};
use gex_ld::pe::{self, PeImage};
use std::sync::{Arc, Mutex};

pub const SEC_IMAGE: u32 = 0x0100_0000;
pub const SEC_COMMIT: u32 = 0x0800_0000;
pub const SEC_RESERVE: u32 = 0x0400_0000;
pub const SEC_NOCACHE: u32 = 0x1000_0000;
pub const SEC_FILE: u32 = 0x0080_0000;

#[derive(Debug)]
pub enum SecKind {
    Image { pe: Arc<PeImage>, path: String, nt_path: Vec<u16> },
    /// A data file mapped as a copy. `file` is kept for a writable section,
    /// whose views are written back to it (see [`FileView`]).
    File { data: Arc<Vec<u8>>, nt_path: Vec<u16>, file: Option<Arc<dyn crate::vfs::FileHandle>> },
    /// Pagefile-backed memory.
    Anon,
}

#[derive(Debug)]
pub struct Section {
    pub kind: SecKind,
    pub size: u64,
    pub prot: u32,
    pub attrs: u32,
    /// Views mapped so far (an anonymous section's second view can't alias
    /// the first in gex's flat memory).
    pub views: Mutex<Vec<u64>>,
}

/// A writable view of a file-backed section. gex has no shared page cache,
/// so the view is a copy: what the program wrote goes back to the file when
/// the view is flushed or unmapped, or the process ends.
#[derive(Clone)]
pub struct FileView {
    pub file: Arc<dyn crate::vfs::FileHandle>,
    pub offset: u64,
    pub len: u64,
}

impl std::fmt::Debug for FileView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileView({:#x}+{:#x})", self.offset, self.len)
    }
}

/// Write view `base` back to its file.
pub fn write_back(c: &mut Call<'_>, base: u64) -> R<()> {
    let v = c.p.st.lock().unwrap().file_views.get(&base).cloned();
    let Some(v) = v else { return Ok(()) };
    let data = c.g.bytes(base, v.len as usize)?;
    let mut done = 0;
    while done < data.len() {
        let n = v.file.write_at(v.offset + done as u64, &data[done..]).map_err(|_| Status(st::UNSUCCESSFUL))? as usize;
        if n == 0 {
            return Err(Status(st::DISK_FULL));
        }
        done += n;
    }
    Ok(())
}

/// Write every writable file view back (the process is ending).
pub fn write_back_all(c: &mut Call<'_>) {
    let bases: Vec<u64> = c.p.st.lock().unwrap().file_views.keys().copied().collect();
    for b in bases {
        let _ = write_back(c, b);
    }
}

/// A mapped image, for the queries that ask about one.
#[derive(Clone, Debug)]
pub struct ImageView {
    pub base: u64,
    pub size: u64,
    pub pe: Arc<PeImage>,
    pub path: String,
    pub nt_path: Vec<u16>,
}

fn rd16(d: &[u8], o: usize) -> u16 {
    d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0)
}
fn rd32(d: &[u8], o: usize) -> u32 {
    d.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0)
}
fn rd64(d: &[u8], o: usize) -> u64 {
    d.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0)
}

/// Optional-header fields gex-ld doesn't keep.
pub struct OptExtra {
    pub os_major: u16,
    pub os_minor: u16,
    pub subsys_major: u16,
    pub subsys_minor: u16,
    pub checksum: u32,
    pub stack_reserve: u64,
    pub stack_commit: u64,
    pub loader_flags: u32,
}

pub fn opt_extra(p: &PeImage) -> OptExtra {
    let d = &p.data;
    let o = p.opt_offset;
    let (sr, sc, lf) = if p.is64 {
        (rd64(d, o + 72), rd64(d, o + 80), rd32(d, o + 104))
    } else {
        (rd32(d, o + 72) as u64, rd32(d, o + 76) as u64, rd32(d, o + 92))
    };
    OptExtra {
        os_major: rd16(d, o + 40),
        os_minor: rd16(d, o + 42),
        subsys_major: rd16(d, o + 48),
        subsys_minor: rd16(d, o + 50),
        checksum: rd32(d, o + 64),
        stack_reserve: sr,
        stack_commit: sc,
        loader_flags: lf,
    }
}

fn align_up(v: u64, a: u64) -> u64 {
    (v + a - 1) & !(a - 1)
}

/// Windows protection for an image section.
fn section_prot(s: &pe::Section) -> u32 {
    match (s.readable() || s.executable(), s.writable(), s.executable()) {
        (_, true, true) => page_prot::EXECUTE_WRITECOPY,
        (_, false, true) => page_prot::EXECUTE_READ,
        (_, true, false) => page_prot::WRITECOPY,
        (true, false, false) => page_prot::READONLY,
        (false, false, false) => page_prot::NOACCESS,
    }
}

/// The machine a process of this ABI runs.
fn native_machine(abi: Abi) -> u16 {
    match abi {
        Abi::P64 => pe::MACHINE_AMD64,
        Abi::P32 => pe::MACHINE_I386,
    }
}

/// Map an image. `want` is a caller-chosen base (0 for "choose").
/// Returns the base and the status (`SUCCESS`, or
/// `IMAGE_MACHINE_TYPE_MISMATCH` for another architecture's image).
pub fn map_image(
    p: &Process,
    k: &mut crate::Kernel,
    g: &mut G<'_>,
    img: &Arc<PeImage>,
    want: u64,
    path: &str,
    nt_path: &[u16],
) -> R<(u64, u32)> {
    let size = align_up(img.image_size as u64, PAGE);
    let mut stl = p.st.lock().unwrap();
    let base = if want != 0 {
        if want % vm::GRANULE != 0 || !stl.vm.is_free(want, want + size) {
            return Err(Status(st::CONFLICTING_ADDRESSES));
        }
        want
    } else if !img.relocatable() {
        let b = img.preferred_base;
        if b + size > vm::ALLOC_HIGH || !stl.vm.is_free(b, b + size) {
            return Err(Status(st::CONFLICTING_ADDRESSES));
        }
        b
    } else {
        stl.vm.find_free(align_up(size, vm::GRANULE), false, vm::ALLOC_HIGH).ok_or(Status(st::NO_MEMORY))?
    };
    let bytes = img.map(base).map_err(|e| {
        crate::nt_log!("gex-nt: mapping {path} at {base:#x}: {e}");
        Status(st::INVALID_IMAGE_FORMAT)
    })?;
    // Pages: headers read-only, each section with its own protection.
    let n = (size / PAGE) as usize;
    let mut pages = vec![vm::Page { committed: true, prot: page_prot::READONLY }; n];
    if (img.section_align as u64) < PAGE {
        pages.iter_mut().for_each(|pg| pg.prot = page_prot::EXECUTE_WRITECOPY);
    } else {
        for s in &img.sections {
            let vsize = if s.vsize != 0 { s.vsize } else { s.raw_size } as u64;
            let start = s.rva as u64;
            let end = align_up(start + vsize, img.section_align as u64).min(size);
            let prot = section_prot(s);
            for i in (start / PAGE) as usize..(align_up(end, PAGE) / PAGE) as usize {
                if i < n {
                    pages[i].prot = prot;
                }
            }
        }
    }
    stl.vm.insert(vm::Region {
        base,
        size,
        alloc_prot: page_prot::EXECUTE_WRITECOPY,
        kind: Kind::Image,
        pages,
        name: path.to_string(),
    });
    stl.images.insert(
        base,
        ImageView { base, size, pe: Arc::clone(img), path: path.to_string(), nt_path: nt_path.to_vec() },
    );
    g.zero(base, size as usize)?;
    g.write(base, &bytes)?;
    if stl.vm.mirror(&mut k.mem, base, base + size) {
        k.retired_code.push((base, base + size));
    }
    let status = if img.machine != native_machine(p.abi) { st::IMAGE_MACHINE_TYPE_MISMATCH } else { st::SUCCESS };
    Ok((base, status))
}

fn read_file_handle(c: &Call<'_>, h: u32) -> R<(Arc<Vec<u8>>, String, Vec<u16>)> {
    let f = super::file::get_file(c, h)?;
    if f.is_dir {
        return Err(Status(st::INVALID_FILE_FOR_SECTION));
    }
    let size = f.handle.metadata()?.size as usize;
    let mut data = vec![0u8; size];
    let mut done = 0;
    while done < size {
        let n = f.handle.read_at(done as u64, &mut data[done..])? as usize;
        if n == 0 {
            break;
        }
        done += n;
    }
    data.truncate(done);
    Ok((Arc::new(data), f.path.clone(), f.nt_path.clone()))
}

/// `NtCreateSection(*handle, access, attr, *size, prot, attributes, file)`.
pub fn nt_create_section(c: &mut Call<'_>) -> R {
    let (hp, access, attr_p, size_p, prot, attrs, file) =
        (c.a(0), c.a32(1), c.a(2), c.a(3), c.a32(4), c.a32(5), c.a32(6));
    let want_size = if size_p != 0 { Some(c.g.u64(size_p)?) } else { None };
    let name = match c.g.obj_attr(attr_p)? {
        Some(a) if !a.name.is_empty() => Some(super::object::object_name(c, a.root, &a.name)?),
        _ => None,
    };
    let sec = if attrs & SEC_IMAGE != 0 {
        if file == 0 {
            return Err(Status(st::INVALID_FILE_FOR_SECTION));
        }
        let (data, path, nt_path) = read_file_handle(c, file)?;
        if !pe::is_pe(&data) {
            return Err(Status(if data.len() >= 2 && &data[..2] == b"MZ" {
                st::INVALID_IMAGE_FORMAT
            } else {
                st::INVALID_IMAGE_NOT_MZ
            }));
        }
        let img = pe::parse(&data).map_err(|e| {
            crate::nt_log!("gex-nt: {path} is not an image gex can map: {e}");
            Status(st::INVALID_IMAGE_FORMAT)
        })?;
        let size = align_up(img.image_size as u64, PAGE);
        Section {
            kind: SecKind::Image { pe: Arc::new(img), path, nt_path },
            size,
            prot,
            attrs,
            views: Mutex::new(Vec::new()),
        }
    } else if file != 0 {
        let (data, _path, nt_path) = read_file_handle(c, file)?;
        let size = want_size.filter(|&s| s != 0).unwrap_or(data.len() as u64);
        let writable = prot & (page_prot::READWRITE | page_prot::EXECUTE_READWRITE) != 0;
        if size > data.len() as u64 && !writable {
            return Err(Status(st::SECTION_TOO_BIG_STATUS));
        }
        let handle = if writable {
            let f = super::file::get_file(c, file)?;
            // A writable section larger than its file extends the file, as
            // on Windows.
            if size > data.len() as u64 {
                f.handle.write_at(size - 1, &[0]).map_err(|_| Status(st::DISK_FULL))?;
            }
            Some(Arc::clone(&f.handle))
        } else {
            None
        };
        Section { kind: SecKind::File { data, nt_path, file: handle }, size, prot, attrs, views: Mutex::new(Vec::new()) }
    } else {
        let size = want_size.ok_or(Status(st::INVALID_PARAMETER_4))?;
        if size == 0 {
            return Err(Status(st::INVALID_PARAMETER_4));
        }
        Section { kind: SecKind::Anon, size, prot, attrs, views: Mutex::new(Vec::new()) }
    };
    let sec = Arc::new(sec);
    let mut stl = c.p.st.lock().unwrap();
    if let Some(n) = &name {
        let key = n.to_ascii_lowercase();
        if let Some(existing) = stl.handles.named.get(&key).cloned() {
            let h = stl.handles.insert(existing, access);
            drop(stl);
            c.g.wptr(hp, h as u64)?;
            return Ok(st::OBJECT_NAME_EXISTS);
        }
        stl.handles.named.insert(key, Obj::Section(Arc::clone(&sec)));
    }
    let h = stl.handles.insert(Obj::Section(sec), access);
    drop(stl);
    c.g.wptr(hp, h as u64)?;
    Ok(st::SUCCESS)
}

/// `NtOpenSection(*handle, access, attr)`: named sections this process
/// created. `\KnownDlls` is empty, so the loader searches the disk.
pub fn nt_open_section(c: &mut Call<'_>) -> R {
    let (hp, access, attr_p) = (c.a(0), c.a32(1), c.a(2));
    let a = c.g.obj_attr(attr_p)?.ok_or(Status(st::INVALID_PARAMETER))?;
    let name = super::object::object_name(c, a.root, &a.name)?;
    let mut stl = c.p.st.lock().unwrap();
    let obj = stl.handles.named.get(&name.to_ascii_lowercase()).cloned();
    match obj {
        Some(o @ Obj::Section(_)) => {
            let h = stl.handles.insert(o, access);
            drop(stl);
            c.g.wptr(hp, h as u64)?;
            Ok(st::SUCCESS)
        }
        Some(_) => Err(Status(st::OBJECT_TYPE_MISMATCH)),
        None => Err(Status(st::OBJECT_NAME_NOT_FOUND)),
    }
}

fn get_section(c: &Call<'_>, h: u32) -> R<Arc<Section>> {
    match c.p.st.lock().unwrap().handles.get(h).map(|e| &e.obj) {
        Some(Obj::Section(s)) => Ok(Arc::clone(s)),
        Some(_) => Err(Status(st::OBJECT_TYPE_MISMATCH)),
        None => Err(Status(st::INVALID_HANDLE)),
    }
}

/// Map a view; `(base_p, offset_p, size_p, prot)` as the syscall has them.
fn map_view(c: &mut Call<'_>, sh: u32, base_p: u64, offset_p: u64, size_p: u64, alloc_type: u32, prot: u32, limit: u64) -> R {
    let sec = get_section(c, sh)?;
    let want = c.g.ptr(base_p)?;
    match &sec.kind {
        SecKind::Image { pe, path, nt_path } => {
            let (pe, path, nt_path) = (Arc::clone(pe), path.clone(), nt_path.clone());
            let mut k = c.kernel.lock_kernel();
            let (base, status) = map_image(c.p, &mut k, &mut c.g, &pe, want, &path, &nt_path)?;
            drop(k);
            sec.views.lock().unwrap().push(base);
            c.g.wptr(base_p, base)?;
            c.g.wptr(size_p, align_up(pe.image_size as u64, PAGE))?;
            Ok(status)
        }
        SecKind::File { .. } | SecKind::Anon => {
            let offset = if offset_p != 0 { c.g.u64(offset_p)? } else { 0 };
            if offset % vm::GRANULE != 0 {
                return Err(Status(st::MAPPED_ALIGNMENT));
            }
            let mut size = c.g.ptr(size_p)?;
            if size == 0 {
                if offset >= sec.size {
                    return Err(Status(st::INVALID_VIEW_SIZE));
                }
                size = sec.size - offset;
            }
            if offset + size > sec.size {
                return Err(Status(st::INVALID_VIEW_SIZE));
            }
            let writable = prot & (page_prot::READWRITE | page_prot::EXECUTE_READWRITE) != 0;
            {
                // Views since unmapped no longer count.
                let mut views = sec.views.lock().unwrap();
                {
                    let stl = c.p.st.lock().unwrap();
                    views.retain(|&b| stl.vm.find(b).is_some_and(|r| r.base == b && r.kind == Kind::Mapped));
                }
                if !views.is_empty() && writable {
                    return Err(c.unimplemented("a second writable view of one section (views can't alias in gex)"));
                }
            }
            let write_back_to = match &sec.kind {
                SecKind::File { file: Some(f), .. } if writable => Some(Arc::clone(f)),
                _ => None,
            };
            let _ = alloc_type;
            let size_pg = align_up(size, PAGE);
            let mut stl = c.p.st.lock().unwrap();
            let base = if want != 0 {
                let b = want & !(vm::GRANULE - 1);
                if !stl.vm.is_free(b, b + size_pg) {
                    return Err(Status(st::CONFLICTING_ADDRESSES));
                }
                b
            } else {
                stl.vm
                    .find_free(align_up(size_pg, vm::GRANULE), alloc_type & vm::mem_flags::TOP_DOWN != 0, limit)
                    .ok_or(Status(st::NO_MEMORY))?
            };
            let commit = !matches!(sec.kind, SecKind::Anon) || sec.attrs & SEC_RESERVE == 0;
            stl.vm.insert(vm::Region {
                base,
                size: size_pg,
                alloc_prot: prot,
                kind: Kind::Mapped,
                pages: vec![vm::Page { committed: commit, prot: if commit { prot } else { 0 } }; (size_pg / PAGE) as usize],
                name: "[section]".into(),
            });
            drop(stl);
            c.g.zero(base, size_pg as usize)?;
            match &sec.kind {
                // A writable section's file has what earlier views wrote
                // back: read it now rather than the section's snapshot.
                SecKind::File { file: Some(f), .. } => {
                    let mut buf = vec![0u8; size as usize];
                    let mut done = 0;
                    while done < buf.len() {
                        let n = f.read_at(offset + done as u64, &mut buf[done..]).map_err(|_| Status(st::UNSUCCESSFUL))? as usize;
                        if n == 0 {
                            break;
                        }
                        done += n;
                    }
                    c.g.write(base, &buf[..done])?;
                }
                SecKind::File { data, .. } => {
                    let from = (offset as usize).min(data.len());
                    let to = ((offset + size) as usize).min(data.len());
                    c.g.write(base, &data[from..to])?;
                }
                _ => {}
            }
            vm::after_change(c, base, base + size_pg, &[])?;
            if let Some(file) = write_back_to {
                c.p.st.lock().unwrap().file_views.insert(base, FileView { file, offset, len: size });
            }
            sec.views.lock().unwrap().push(base);
            c.g.wptr(base_p, base)?;
            c.g.wptr(size_p, size_pg)?;
            Ok(st::SUCCESS)
        }
    }
}

/// `NtMapViewOfSection(section, process, *base, zero_bits, commit_size,
/// *offset, *size, inherit, alloc_type, prot)`.
pub fn nt_map_view_of_section(c: &mut Call<'_>) -> R {
    vm::check_process(c, c.a(1))?;
    let (sh, base_p, zero_bits, offset_p, size_p, alloc_type, prot) =
        (c.a32(0), c.a(2), c.a(3), c.a(5), c.a(6), c.a32(8), c.a32(9));
    let limit = if zero_bits == 0 { vm::ALLOC_HIGH } else { (1u64 << (32 - zero_bits.min(31))).min(vm::ALLOC_HIGH) };
    map_view(c, sh, base_p, offset_p, size_p, alloc_type, prot, limit)
}

/// `NtMapViewOfSectionEx(section, process, *base, *offset, *size, alloc_type,
/// prot, params, count)`.
pub fn nt_map_view_of_section_ex(c: &mut Call<'_>) -> R {
    vm::check_process(c, c.a(1))?;
    if c.a32(8) != 0 {
        return Err(c.unimplemented("NtMapViewOfSectionEx with extended parameters"));
    }
    let (sh, base_p, offset_p, size_p, alloc_type, prot) = (c.a32(0), c.a(2), c.a(3), c.a(4), c.a32(5), c.a32(6));
    map_view(c, sh, base_p, offset_p, size_p, alloc_type, prot, vm::ALLOC_HIGH)
}

fn unmap(c: &mut Call<'_>, addr: u64) -> R {
    let base = {
        let stl = c.p.st.lock().unwrap();
        let r = stl.vm.find(addr).cloned().ok_or(Status(st::NOT_MAPPED_VIEW))?;
        if r.kind == Kind::Private {
            return Err(Status(st::NOT_MAPPED_VIEW));
        }
        r.base
    };
    write_back(c, base)?;
    let mut stl = c.p.st.lock().unwrap();
    stl.file_views.remove(&base);
    let r = stl.vm.find(addr).cloned().ok_or(Status(st::NOT_MAPPED_VIEW))?;
    stl.vm.remove(r.base);
    stl.images.remove(&r.base);
    drop(stl);
    vm::after_change(c, r.base, r.end(), &[])?;
    Ok(st::SUCCESS)
}

/// `NtUnmapViewOfSection(process, base)`.
pub fn nt_unmap_view_of_section(c: &mut Call<'_>) -> R {
    vm::check_process(c, c.a(0))?;
    unmap(c, c.a(1))
}

/// `NtUnmapViewOfSectionEx(process, base, flags)`.
pub fn nt_unmap_view_of_section_ex(c: &mut Call<'_>) -> R {
    vm::check_process(c, c.a(0))?;
    if c.a32(2) != 0 {
        return Err(c.unimplemented("NtUnmapViewOfSectionEx flags"));
    }
    unmap(c, c.a(1))
}

/// Write `SECTION_IMAGE_INFORMATION` for an image.
pub fn write_image_info(g: &mut G<'_>, buf: u64, img: &PeImage) -> R<()> {
    let abi = g.abi;
    let x = opt_extra(img);
    g.zero(buf, off!(abi, section_image_information::SIZEOF) as usize)?;
    macro_rules! o {
        ($f:ident) => {
            buf + off!(abi, section_image_information::$f)
        };
    }
    g.wptr(o!(TRANSFERADDRESS), img.preferred_base.wrapping_add(img.entry_rva as u64))?;
    g.wptr(o!(MAXIMUMSTACKSIZE), x.stack_reserve)?;
    g.wptr(o!(COMMITTEDSTACKSIZE), x.stack_commit)?;
    g.w32(o!(SUBSYSTEMTYPE), img.subsystem as u32)?;
    g.w16(o!(MINORSUBSYSTEMVERSION), x.subsys_minor)?;
    g.w16(o!(MAJORSUBSYSTEMVERSION), x.subsys_major)?;
    g.w16(o!(MAJOROPERATINGSYSTEMVERSION), x.os_major)?;
    g.w16(o!(MINOROPERATINGSYSTEMVERSION), x.os_minor)?;
    g.w16(o!(IMAGECHARACTERISTICS), img.characteristics)?;
    g.w16(o!(DLLCHARACTERISTICS), img.dll_characteristics)?;
    g.w16(o!(MACHINE), img.machine)?;
    let code = img.sections.iter().any(|s| s.executable() || s.characteristics & pe::SCN_CNT_CODE != 0);
    g.w8(o!(IMAGECONTAINSCODE), code as u8)?;
    g.w32(o!(LOADERFLAGS), x.loader_flags)?;
    g.w32(o!(IMAGEFILESIZE), img.data.len() as u32)?;
    g.w32(o!(CHECKSUM), x.checksum)?;
    Ok(())
}

/// `NtQuerySection(handle, class, buffer, len, *ret_len)`.
pub fn nt_query_section(c: &mut Call<'_>) -> R {
    let (h, class, buf, len, ret) = (c.a32(0), c.a32(1), c.a(2), c.a(3), c.a(4));
    let sec = get_section(c, h)?;
    let abi = c.p.abi;
    match class {
        // SectionBasicInformation
        0 => {
            let size = off!(abi, section_basic_information::SIZEOF);
            if len < size {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            c.g.zero(buf, size as usize)?;
            c.g.w32(buf + off!(abi, section_basic_information::ATTRIBUTES), sec.attrs | if matches!(sec.kind, SecKind::File{..}) { SEC_FILE } else { 0 })?;
            c.g.w64(buf + off!(abi, section_basic_information::SIZE), sec.size)?;
            c.g.out_ptr(ret, size)?;
            Ok(st::SUCCESS)
        }
        // SectionImageInformation
        1 => {
            let size = off!(abi, section_image_information::SIZEOF);
            if len < size {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            let SecKind::Image { pe, .. } = &sec.kind else {
                return Err(Status(st::SECTION_NOT_IMAGE));
            };
            let pe = Arc::clone(pe);
            write_image_info(&mut c.g, buf, &pe)?;
            c.g.out_ptr(ret, size)?;
            Ok(st::SUCCESS)
        }
        _ => Err(c.unimplemented(&format!("NtQuerySection class {class}"))),
    }
}

/// `NtQueryVirtualMemory(MemoryMappedFilenameInformation)`: a
/// `UNICODE_STRING` with the NT name, then the name.
pub fn query_mapped_filename(c: &mut Call<'_>, addr: u64, buf: u64, len: u64, ret_p: u64) -> R {
    let name = {
        let stl = c.p.st.lock().unwrap();
        let r = stl.vm.find(addr).ok_or(Status(st::INVALID_ADDRESS))?;
        if r.kind != Kind::Image {
            return Err(Status(st::FILE_INVALID));
        }
        stl.images.get(&r.base).map(|v| v.nt_path.clone()).ok_or(Status(st::FILE_INVALID))?
    };
    // Windows reports `\Device\HarddiskVolume1\…`; Wine reports the DOS
    // path's NT form. Keep Wine's.
    let us = c.g.us_size();
    let need = us + name.len() as u64 * 2 + 2;
    c.g.out_ptr(ret_p, need)?;
    if len < need {
        return Err(Status(st::BUFFER_OVERFLOW));
    }
    let b = name.len() as u16 * 2;
    c.g.w_ustr_hdr(buf, b, b + 2, buf + us)?;
    let mut z = name;
    z.push(0);
    c.g.write_wchars(buf + us, &z)?;
    Ok(st::SUCCESS)
}

/// `NtQueryVirtualMemory(MemoryImageInformation)`: `MEMORY_IMAGE_INFORMATION`
/// (base, size, flags).
pub fn query_image_information(c: &mut Call<'_>, addr: u64, buf: u64, len: u64, ret_p: u64) -> R {
    let ps = c.g.psize();
    let size = 2 * ps + 8;
    if len < size {
        return Err(Status(st::INFO_LENGTH_MISMATCH));
    }
    let v = {
        let stl = c.p.st.lock().unwrap();
        let r = stl.vm.find(addr).cloned();
        match r {
            Some(r) if r.kind == Kind::Image => stl.images.get(&r.base).cloned(),
            _ => None,
        }
    };
    c.g.zero(buf, size as usize)?;
    if let Some(v) = v {
        c.g.wptr(buf, v.base)?;
        c.g.wptr(buf + ps, v.size)?;
    }
    c.g.out_ptr(ret_p, size)?;
    Ok(st::SUCCESS)
}

#[allow(dead_code)]
fn _names(n: &[u16]) -> String {
    let _ = to_w("");
    wstr(n)
}

/// `NtFlushVirtualMemory(process, *addr, *size, iosb)`: a writable file
/// view's contents go back to the file.
pub fn nt_flush_virtual_memory(c: &mut Call<'_>) -> R {
    vm::check_process(c, c.a(0))?;
    let (addr_p, size_p, iosb) = (c.a(1), c.a(2), c.a(3));
    let addr = c.g.ptr(addr_p)?;
    let base = {
        let stl = c.p.st.lock().unwrap();
        let r = stl.vm.find(addr).cloned().ok_or(Status(st::NOT_MAPPED_VIEW))?;
        if r.kind == Kind::Private {
            return Err(Status(st::NOT_MAPPED_VIEW));
        }
        r.base
    };
    write_back(c, base)?;
    let len = c.p.st.lock().unwrap().file_views.get(&base).map(|v| v.len).unwrap_or(0);
    c.g.wptr(addr_p, base)?;
    c.g.wptr(size_p, align_up(len, PAGE))?;
    if iosb != 0 {
        c.g.wptr(iosb, 0)?;
        c.g.wptr(iosb + c.g.psize(), 0)?;
    }
    Ok(st::SUCCESS)
}
