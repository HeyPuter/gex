//! Content keys that every host must compute alike.
//!
//! The native host and the browser shim both find prebuilt code by these:
//! the AOT cache by an image's identity, the function tier by a unit's key.
//! A key computed one way here and another way there would load the wrong
//! module, or never find one, without a word -- so there is one
//! implementation, in a crate with no dependency either host cannot build
//! (the browser shim has no wasmtime), and a test pins its values.

pub mod sha256;

use gex_ld::Image;

/// An image's identity: its placement, entry, class and every segment's
/// address, permissions and bytes as loaded (relocated at its base).
pub fn image_identity(image: &Image) -> String {
    let mut h = sha256::Sha256::new();
    h.update(b"gex-image-1");
    h.update(&image.bias.to_le_bytes());
    h.update(&image.entry.to_le_bytes());
    h.update(&[image.class32 as u8]);
    for seg in &image.segments {
        h.update(&seg.vaddr.to_le_bytes());
        h.update(&[seg.executable as u8]);
        h.update(&(seg.bytes.len() as u64).to_le_bytes());
        h.update(&seg.bytes);
    }
    format!("image:{}", sha256::hex(&h.finish()))
}

/// The identity of an image's function-tier recovery: the sources that decide
/// it (`recovery_inputs`, gex-cgen's `RECOVERY_INPUTS`), the guest it is
/// decoded as, and the image as placed. gex-cgen's `image::recovery_key`
/// passes its own constants.
///
/// The guest is not in the sources' hash: every gexc flavour is built from
/// the same sources, and each decodes an image as its own guest's code. An
/// aarch64 gexc run on an i386 DLL wrote a recovery of AArch64 instructions
/// under the key the x86 gexc reads, and the x86 units built from it hung
/// Wine's boot. x86 adds nothing, so its keys, and
/// every cache built with them, stay as they were.
pub fn recovery_key(image: &Image, recovery_inputs: &str) -> String {
    let mut h = sha256::Sha256::new();
    h.update(b"gex-cgen-recovery 1");
    h.update(recovery_inputs.as_bytes());
    if !gex_abi::X86 {
        h.update(b":");
        h.update(gex_abi::ARCH_NAME.as_bytes());
    }
    h.update(image_identity(image).as_bytes());
    sha256::hex(&h.finish())
}

/// The identity of a function-tier unit: its format, the sources that decide
/// the C and the module, what it embeds from elsewhere, and the recovery it
/// was built from (gex-cgen's `image::image_key` passes its constants). The
/// unit lives in the directory of this name under the cgen cache.
pub fn unit_key(image: &Image, format: &str, unit_inputs: &str, unit_abi_version: u32, recovery_inputs: &str) -> String {
    let mut h = sha256::Sha256::new();
    h.update(format.as_bytes());
    h.update(unit_inputs.as_bytes());
    h.update(&unit_abi_version.to_le_bytes());
    h.update(recovery_key(image, recovery_inputs).as_bytes());
    sha256::hex(&h.finish())
}

/// One entry of an ejected site's `identities` (`base:key:identity:path`,
/// joined by `;`): a unit's key and its image's identity at a base, for the
/// processes of one flavour. The base is written after its flavour's marks:
/// `m` for a 64-bit memory's, then `k` for a compact process's (the KUSER
/// fold covering the entry page); neither is a hex digit. An unmarked key is a 32-bit memory's
/// process's without the entry fold; one process takes only its own
/// flavour's, since the flavours place an image at the same base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityLine<'a> {
    pub mem64: bool,
    pub compact: bool,
    pub base: u64,
    pub key: &'a str,
    pub identity: &'a str,
    pub path: &'a str,
}

impl<'a> IdentityLine<'a> {
    /// The entry as the eject writes it.
    pub fn write(&self) -> String {
        let (m, c) = (if self.mem64 { "m" } else { "" }, if self.compact { "k" } else { "" });
        format!("{m}{c}{:x}:{}:{}:{}", self.base, self.key, self.identity, self.path)
    }

    /// One `;`-separated entry; `None` for a malformed one.
    pub fn parse(item: &'a str) -> Option<Self> {
        let mut f = item.splitn(4, ':');
        let (base, key, identity, path) = (f.next()?, f.next()?, f.next()?, f.next()?);
        let (mem64, base) = base.strip_prefix('m').map_or((false, base), |b| (true, b));
        let (compact, base) = base.strip_prefix('k').map_or((false, base), |b| (true, b));
        let base = u64::from_str_radix(base, 16).ok()?;
        Some(Self { mem64, compact, base, key, identity, path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small fixed image: two segments, one executable.
    fn fixture() -> Image {
        let seg = |vaddr: u64, bytes: Vec<u8>, executable: bool| gex_ld::elf::Segment {
            vaddr,
            mem_size: bytes.len() as u64,
            bytes,
            readable: true,
            writable: !executable,
            executable,
        };
        Image {
            soname: None,
            needed: Vec::new(),
            runpath: Vec::new(),
            bias: 0x40_0000,
            entry: 0x40_1000,
            code: vec![(0x40_1000, 0x40_1010)],
            segments: vec![
                seg(0x40_1000, vec![0x55, 0x89, 0xe5, 0x31, 0xc0, 0x5d, 0xc3, 0x90], true),
                seg(0x40_2000, b"data".to_vec(), false),
            ],
            phdr_vaddr: 0x40_0000,
            phentsize: 0,
            phnum: 0,
            brk: 0x40_3000,
            text_range: (0x40_1000, 0x40_1010),
            tls: None,
            symbols: Vec::new(),
            named_relocs: Vec::new(),
            relocs: Vec::new(),
            init_array: Vec::new(),
            dynsyms: Vec::new(),
            irelative: Vec::new(),
            pie: false,
            interp: None,
            class32: true,
        }
    }

    #[test]
    fn sha256_known_answer() {
        let mut h = sha256::Sha256::new();
        h.update(b"abc");
        assert_eq!(sha256::hex(&h.finish()), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    /// The values both hosts must agree on. A change here changes every key:
    /// every cached unit and AOT module is then rebuilt, which is the point of
    /// pinning it -- it must be deliberate.
    #[test]
    fn keys_are_pinned() {
        let img = fixture();
        assert_eq!(image_identity(&img), "image:f9c66b197e4206a0b666bb966e724f0e60ac7f4f6a6fc0540b9915025a3a1dbb");
        // The x86 keys; another guest's differ from them (`recovery_key`).
        let rec = recovery_key(&img, "recovery-inputs");
        let unit = unit_key(&img, "gex-cgen-image 3", "unit-inputs", 1, "recovery-inputs");
        if gex_abi::X86 {
            assert_eq!(rec, "e586c60522f7c873ddc04a9652df01f404529981dbb33ec325609e23d972b437");
            assert_eq!(unit, "cfb86de8e01048b353148fa2d7afdff15a1263bbec2bfb80c7f35dbfaf3b0059");
        } else {
            assert_ne!(rec, "e586c60522f7c873ddc04a9652df01f404529981dbb33ec325609e23d972b437");
            assert_ne!(unit, "cfb86de8e01048b353148fa2d7afdff15a1263bbec2bfb80c7f35dbfaf3b0059");
        }
    }

    #[test]
    fn identity_lines_keep_their_flavour() {
        for (mem64, compact) in [(false, false), (true, false), (false, true), (true, true)] {
            let l = IdentityLine { mem64, compact, base: 0x6b0c0000, key: "k1", identity: "i1", path: "/w/user32.dll" };
            let w = l.write();
            assert_eq!(IdentityLine::parse(&w), Some(l), "{w}");
        }
        // The marks are not hex digits, so a base beginning with one is not misread.
        assert_eq!(IdentityLine::parse("k1f00000:k:i:/p").map(|l| (l.compact, l.base)), Some((true, 0x1f00000)));
        assert_eq!(IdentityLine::parse("c1f00000:k:i:/p").map(|l| (l.compact, l.base)), Some((false, 0xc1f00000)));
        assert_eq!(IdentityLine::parse("1f00000:k:i"), None);
    }
}
