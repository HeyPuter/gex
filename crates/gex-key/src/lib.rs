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
/// it (`recovery_inputs`, gex-cgen's `RECOVERY_INPUTS`) and the image as
/// placed. gex-cgen's `image::recovery_key` passes its own constants.
pub fn recovery_key(image: &Image, recovery_inputs: &str) -> String {
    let mut h = sha256::Sha256::new();
    h.update(b"gex-cgen-recovery 1");
    h.update(recovery_inputs.as_bytes());
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
        assert_eq!(recovery_key(&img, "recovery-inputs"), "e586c60522f7c873ddc04a9652df01f404529981dbb33ec325609e23d972b437");
        assert_eq!(unit_key(&img, "gex-cgen-image 3", "unit-inputs", 1, "recovery-inputs"), "cfb86de8e01048b353148fa2d7afdff15a1263bbec2bfb80c7f35dbfaf3b0059");
    }
}
