//! Loading a dynamically linked program (PLAN §6.4 *cooperative mode*).
//!
//! Places the executable and its own interpreter and enters the interpreter,
//! letting it run translated. This is what musl needs (its loader state has no
//! symbolic interface), and the always-correct fallback for an unrecognised
//! `PT_INTERP`. [`crate::link`] is the alternative for glibc and static-PIE.
//!
//! The auxiliary vector must be exact: the interpreter is entered rather than
//! the program, so it needs `AT_BASE` to find itself, `AT_PHDR`/`AT_PHENT`/
//! `AT_PHNUM` describing the *executable*, and `AT_ENTRY` to jump to when done.

use crate::elf::{load, Image, LoadError};

/// A program and, when it needs one, its interpreter.
#[derive(Clone, Debug)]
pub struct Program {
    /// The executable.
    pub exe: Image,
    /// The dynamic linker, already placed.
    pub interp: Option<Image>,
}

impl Program {
    /// Where execution begins: the interpreter when there is one.
    pub fn entry(&self) -> u64 {
        match &self.interp {
            Some(i) => i.entry,
            None => self.exe.entry,
        }
    }

    /// Load bias of the interpreter, for `AT_BASE`.
    pub fn interp_base(&self) -> u64 {
        self.interp.as_ref().map(|i| i.bias).unwrap_or(0)
    }

    /// Every segment that must be placed in guest memory.
    pub fn segments(&self) -> impl Iterator<Item = &crate::elf::Segment> {
        self.exe
            .segments
            .iter()
            .chain(self.interp.iter().flat_map(|i| i.segments.iter()))
    }

    /// Every segment to place, paired with the interpreter's path when the
    /// segment is the interpreter's. [`Self::segments`] loses that distinction,
    /// which makes two processes fail to recognise `ld.so` as shared code and
    /// re-lift the whole dynamic linker per executable.
    pub fn placements(&self) -> impl Iterator<Item = (&crate::elf::Segment, Option<&str>)> {
        let interp = self.exe.interp.as_deref();
        self.exe
            .segments
            .iter()
            .map(|s| (s, None))
            .chain(self.interp.iter().flat_map(move |i| {
                i.segments.iter().map(move |s| (s, interp))
            }))
    }

    /// First address above everything loaded, where `brk` starts.
    pub fn brk(&self) -> u64 {
        let interp_end = self.interp.as_ref().map(|i| i.brk).unwrap_or(0);
        self.exe.brk.max(interp_end)
    }
}

/// Load a program, following `PT_INTERP` if it has one.
///
/// `read` resolves a guest path to file contents, so the interpreter comes
/// from the guest's own namespace — the sysroot when one is mounted — rather
/// than from the host.
pub fn load_program(
    bytes: &[u8],
    pie_base: u64,
    interp_base: u64,
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> Result<Program, LoadError> {
    let exe = load(bytes, pie_base)?;
    let Some(path) = exe.interp.clone() else {
        return Ok(Program { exe, interp: None });
    };
    let Some(interp_bytes) = read(&path) else {
        return Err(LoadError::NeedsInterpreter(path));
    };
    // The interpreter is position-independent and goes wherever we say, well
    // clear of the executable so neither can overlap the other's mappings.
    let interp = load(&interp_bytes, interp_base)?;
    Ok(Program {
        exe,
        interp: Some(interp),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_static_program_needs_no_interpreter() {
        // A minimal ET_EXEC header with one PT_LOAD.
        let img = minimal_elf(2, None);
        let p = load_program(&img, 0x2000_0000, 0x3000_0000, &|_| None).unwrap();
        assert!(p.interp.is_none());
        assert_eq!(p.entry(), p.exe.entry);
        assert_eq!(p.interp_base(), 0);
    }

    #[test]
    fn a_missing_interpreter_is_reported_by_name() {
        let img = minimal_elf(3, Some("/lib/ld-musl-aarch64.so.1"));
        let err = load_program(&img, 0x2000_0000, 0x3000_0000, &|_| None).unwrap_err();
        assert_eq!(
            err,
            LoadError::NeedsInterpreter("/lib/ld-musl-aarch64.so.1".into())
        );
    }

    #[test]
    fn the_interpreter_is_entered_not_the_program() {
        let exe = minimal_elf(3, Some("/interp"));
        let interp = minimal_elf(3, None);
        let p = load_program(&exe, 0x2000_0000, 0x3000_0000, &|path| {
            (path == "/interp").then(|| interp.clone())
        })
        .unwrap();
        assert!(p.interp.is_some());
        assert_eq!(p.interp_base(), 0x3000_0000);
        // Entry is the interpreter's, and it is placed at its own base.
        assert_eq!(p.entry(), 0x3000_0000 + 0x1000);
        assert_ne!(p.entry(), p.exe.entry);
        // Both images' segments must be placed.
        assert_eq!(p.segments().count(), 2);
    }

    /// A tiny but valid aarch64 ELF: one `PT_LOAD`, optionally a `PT_INTERP`.
    fn minimal_elf(e_type: u16, interp: Option<&str>) -> Vec<u8> {
        let phnum = if interp.is_some() { 2 } else { 1 };
        let phoff = 64usize;
        let interp_off = phoff + phnum * 56;
        let mut v = vec![0u8; 0x2000];
        v[0..4].copy_from_slice(b"\x7fELF");
        v[4] = 2; // ELFCLASS64
        v[5] = 1; // little endian
        v[6] = 1; // version
        v[16..18].copy_from_slice(&e_type.to_le_bytes());
        v[18..20].copy_from_slice(&gex_abi::ELF_MACHINE.to_le_bytes());
        v[24..32].copy_from_slice(&0x1000u64.to_le_bytes()); // e_entry
        v[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
        v[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        v[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());

        // PT_LOAD, executable, covering the whole file.
        let p = phoff;
        v[p..p + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        v[p + 4..p + 8].copy_from_slice(&5u32.to_le_bytes()); // PF_R | PF_X
        v[p + 8..p + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset
        v[p + 16..p + 24].copy_from_slice(&0u64.to_le_bytes()); // p_vaddr
        v[p + 32..p + 40].copy_from_slice(&0x2000u64.to_le_bytes()); // p_filesz
        v[p + 40..p + 48].copy_from_slice(&0x2000u64.to_le_bytes()); // p_memsz

        if let Some(path) = interp {
            let p = phoff + 56;
            v[p..p + 4].copy_from_slice(&3u32.to_le_bytes()); // PT_INTERP
            v[p + 8..p + 16].copy_from_slice(&(interp_off as u64).to_le_bytes());
            v[p + 32..p + 40].copy_from_slice(&((path.len() + 1) as u64).to_le_bytes());
            v[interp_off..interp_off + path.len()].copy_from_slice(path.as_bytes());
        }
        v
    }
}
